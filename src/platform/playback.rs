//! On-demand desktop animation playback, shared by both native backends.
//! 两个原生后端共用的按需桌面动画播放器。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{App, Rgba, Task, component::ActiveTheme};
use image::{AnimationDecoder, DynamicImage, ImageResult, RgbaImage, codecs::webp::WebPDecoder};

use crate::emotes::{self, EmoteId, EmoteImage, EmoteStore, has_visible_pixels};
use crate::gesture::Position;
use crate::model::{Location, TextEffect, TextMotion, TextOverlaySettings, TextPosition};

use super::{animation_presenter, system_reduces_motion, text_presenter};

pub(super) const MAX_ACTIVE: usize = 16;
const TRANSITION_DURATION: Duration = Duration::from_millis(180);
const TRANSITION_FRAME: Duration = Duration::from_millis(16);
pub(super) const TEXT_WIDTH: u32 = 360;
pub(super) const TEXT_PADDING: u32 = 14;
pub(super) const TEXT_RADIUS: u32 = 12;
pub(super) const TEXT_FONT_SIZE: u32 = 16;
pub(super) const TEXT_MAX_LINES: u32 = 6;
pub(super) const TEXT_MARGIN: u32 = 24;

pub(super) struct TextColors {
    pub(super) background: Rgba,
    pub(super) foreground: Rgba,
}

pub(super) struct TextFrame {
    pub(super) opacity: f32,
    pub(super) offset_x: f64,
    pub(super) offset_y: f64,
}

pub(super) enum FrameUpdate {
    Image(RgbaImage),
    Opacity(f32),
}

pub(crate) struct Animation {
    size: u32,
    source: AnimationSource,
    poster: Option<&'static [u8]>,
    timeout: Duration,
    fade_out: bool,
}

enum AnimationSource {
    WebP(Cow<'static, [u8]>),
    Custom(EmoteStore, EmoteId),
}

impl Animation {
    /// Loops animated WebP at its source frame rate until the timeout.
    /// 按素材帧率循环播放 WebP，直到超时。
    pub(crate) fn webp(bytes: impl Into<Cow<'static, [u8]>>, size: u32, timeout: Duration) -> Self {
        Self {
            size: size.clamp(1, 1024),
            source: AnimationSource::WebP(bytes.into()),
            poster: None,
            timeout,
            fade_out: false,
        }
    }

    pub(super) fn custom(store: EmoteStore, id: EmoteId, size: u32, timeout: Duration) -> Self {
        Self {
            size: size.clamp(1, 1024),
            source: AnimationSource::Custom(store, id),
            poster: None,
            timeout,
            fade_out: true,
        }
    }

    pub(super) fn with_poster(mut self, png: &'static [u8]) -> Self {
        self.poster = Some(png);
        self
    }

    pub(super) fn with_fade_out(mut self) -> Self {
        self.fade_out = true;
        self
    }
}

#[derive(Default)]
pub(crate) struct AnimationOverlay {
    next: u64,
    active: Rc<RefCell<BTreeMap<u64, Task<()>>>>,
    text: Option<Task<()>>,
}

impl AnimationOverlay {
    pub(crate) fn play(
        &mut self,
        animation: Animation,
        local: Option<Position>,
        location: Option<Location>,
        cx: &App,
    ) {
        let Some((output_pixels, mut present)) =
            animation_presenter(animation.size, local, location)
        else {
            return;
        };
        let mut active = self.active.borrow_mut();
        if active.len() == MAX_ACTIVE {
            active.pop_first();
        }
        let id = self.next;
        self.next += 1;
        // Tasks own their native windows. A weak registry allows owner teardown to cancel every stream.
        // 任务持有原生窗口，弱引用避免循环持有，播放器销毁时会取消所有播放。
        let registry = Rc::downgrade(&self.active);
        let reduce_motion = animation.fade_out && (cx.reduce_motion() || system_reduces_motion());
        let task = cx.spawn(async move |_| {
            run(animation, output_pixels, reduce_motion, &mut present).await;
            if let Some(registry) = registry.upgrade()
                && let Some(task) = registry.borrow_mut().remove(&id)
            {
                task.detach();
            }
        });
        active.insert(id, task);
    }

    pub(super) fn play_text(
        &mut self,
        text: &str,
        settings: TextOverlaySettings,
        effect: TextEffect,
        cx: &App,
    ) {
        let theme = cx.theme();
        let colors = TextColors {
            background: theme.popover.into(),
            foreground: theme.popover_foreground.into(),
        };
        let Some(present) = text_presenter(text, settings.position, colors) else {
            return;
        };
        let reduce_motion = cx.reduce_motion() || system_reduces_motion();
        // A new message takes over the card immediately, so bursts cannot obscure each other.
        // 新消息立即接替这张卡片，连发时卡片不会互相遮挡。
        self.text = Some(cx.spawn(async move |_| {
            play_text_overlay(settings, effect, reduce_motion, present).await;
        }));
    }
}

#[derive(Clone, Copy)]
enum TextEntrance {
    Basic(TextMotion),
    FlyIn,
    Shake,
    Bounce,
}

impl TextEntrance {
    fn duration(self) -> Duration {
        match self {
            Self::Basic(TextMotion::None) => Duration::ZERO,
            Self::Basic(_) => TRANSITION_DURATION,
            Self::FlyIn => Duration::from_millis(300),
            Self::Shake => Duration::from_millis(380),
            Self::Bounce => Duration::from_millis(440),
        }
    }

    fn frame(self, position: TextPosition, progress: f32) -> TextFrame {
        let remaining = 1.0 - progress;
        match self {
            Self::Basic(motion) => basic_text_frame(position, motion, remaining.powi(3)),
            Self::FlyIn => {
                let inward_x = match position {
                    TextPosition::TopLeft | TextPosition::BottomLeft => 1.0,
                    TextPosition::TopRight | TextPosition::BottomRight => -1.0,
                };
                TextFrame {
                    opacity: (progress * 3.0).min(1.0),
                    offset_x: inward_x * 120.0 * f64::from(remaining.powi(3)),
                    offset_y: 0.0,
                }
            }
            Self::Shake => {
                // A quick reveal is followed by a damped, two-axis shake around the anchor.
                // 文字快速显现，随后围绕定位点做幅度递减的双轴抖动。
                let phase = f64::from(progress) * std::f64::consts::TAU;
                let amplitude = f64::from(remaining.powi(2));
                TextFrame {
                    opacity: (progress * 12.0).min(1.0),
                    offset_x: 18.0 * amplitude * (phase * 3.0).sin(),
                    offset_y: 8.0 * amplitude * (phase * 2.0).sin(),
                }
            }
            Self::Bounce => {
                // Rebound into the work area after each contact with the resting position.
                // 每次触及停靠位置后，都向工作区内侧回弹。
                let phase = f64::from(progress) * std::f64::consts::TAU * 1.4;
                TextFrame {
                    opacity: (progress * 4.0).min(1.0),
                    offset_x: 0.0,
                    offset_y: vertical_direction(position)
                        * 68.0
                        * f64::from(remaining.powi(2))
                        * phase.cos().abs(),
                }
            }
        }
    }
}

async fn play_text_overlay(
    settings: TextOverlaySettings,
    effect: TextEffect,
    reduce_motion: bool,
    mut present: impl FnMut(TextFrame),
) {
    let (enter, exit) = if reduce_motion {
        (TextEntrance::Basic(TextMotion::None), TextMotion::None)
    } else {
        match effect {
            TextEffect::Default => (TextEntrance::Basic(settings.enter), settings.exit),
            TextEffect::Fade => (TextEntrance::Basic(TextMotion::Fade), TextMotion::Fade),
            TextEffect::Slide => (TextEntrance::Basic(TextMotion::Slide), TextMotion::Slide),
            TextEffect::FlyIn => (TextEntrance::FlyIn, TextMotion::Fade),
            TextEffect::Shake => (TextEntrance::Shake, TextMotion::Fade),
            TextEffect::Bounce => (TextEntrance::Bounce, TextMotion::Fade),
            TextEffect::None => (TextEntrance::Basic(TextMotion::None), TextMotion::None),
        }
    };
    animate(enter.duration(), |progress| {
        present(enter.frame(settings.position, progress));
    })
    .await;
    smol::Timer::after(Duration::from_millis(u64::from(settings.timeout_ms))).await;
    if exit != TextMotion::None {
        transition(|hidden| present(basic_text_frame(settings.position, exit, hidden))).await;
    }
}

// Work-area coordinates are measured from the top left on both backends.
// 两个平台的工作区坐标统一从左上角计算。
pub(super) fn text_anchor(
    position: TextPosition,
    free_x: f64,
    free_y: f64,
    margin: f64,
) -> (f64, f64) {
    let horizontal = margin.min(free_x / 2.0);
    let vertical = margin.min(free_y / 2.0);
    let x = match position {
        TextPosition::TopLeft | TextPosition::BottomLeft => horizontal,
        TextPosition::TopRight | TextPosition::BottomRight => free_x - horizontal,
    };
    let y = match position {
        TextPosition::TopLeft | TextPosition::TopRight => vertical,
        TextPosition::BottomLeft | TextPosition::BottomRight => free_y - vertical,
    };
    (x, y)
}

fn vertical_direction(position: TextPosition) -> f64 {
    match position {
        TextPosition::TopLeft | TextPosition::TopRight => 1.0,
        TextPosition::BottomLeft | TextPosition::BottomRight => -1.0,
    }
}

fn basic_text_frame(position: TextPosition, motion: TextMotion, hidden: f32) -> TextFrame {
    TextFrame {
        opacity: 1.0 - hidden,
        offset_x: 0.0,
        offset_y: if motion == TextMotion::Slide {
            -vertical_direction(position) * 20.0 * f64::from(hidden)
        } else {
            0.0
        },
    }
}

async fn transition(mut present: impl FnMut(f32)) {
    animate(TRANSITION_DURATION, |progress| present(progress.powi(2))).await;
}

async fn animate(duration: Duration, mut present: impl FnMut(f32)) {
    let start = Instant::now();
    let end = start + duration;
    loop {
        let now = Instant::now();
        if now >= end {
            present(1.0);
            break;
        }
        present(now.duration_since(start).as_secs_f32() / duration.as_secs_f32());
        smol::Timer::at((now + TRANSITION_FRAME).min(end)).await;
    }
}

type TimedFrame = (RgbaImage, Duration);

fn prepare(image: RgbaImage, size: Option<u32>) -> RgbaImage {
    let image = match size {
        Some(size) if image.width() != image.height() => {
            let fitted = image::DynamicImage::ImageRgba8(image)
                .resize(size, size, image::imageops::FilterType::Triangle)
                .into_rgba8();
            let mut square = RgbaImage::new(size, size);
            image::imageops::replace(
                &mut square,
                &fitted,
                i64::from((size - fitted.width()) / 2),
                i64::from((size - fitted.height()) / 2),
            );
            square
        }
        Some(size) if image.dimensions() != (size, size) => {
            image::imageops::resize(&image, size, size, image::imageops::FilterType::Triangle)
        }
        _ => image,
    };
    #[cfg(target_os = "windows")]
    {
        // The window thread only copies ready-to-present premultiplied BGRA into the DIB.
        // 在后台生成预乘 BGRA，窗口线程只需拷贝到 DIB。
        let mut image = image;
        for pixel in image.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
            let [r, g, b, a] = *pixel;
            let premultiply = |v: u8| ((u16::from(v) * u16::from(a) + 127) / 255) as u8;
            *pixel = [premultiply(b), premultiply(g), premultiply(r), a];
        }
        image
    }
    #[cfg(not(target_os = "windows"))]
    image
}

fn webp(bytes: &[u8]) -> ImageResult<EmoteImage<'_>> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))?;
    if decoder.has_animation() {
        Ok(EmoteImage::Animated(decoder.into_frames()))
    } else {
        Ok(EmoteImage::Still(
            DynamicImage::from_decoder(decoder)?.into_rgba8(),
        ))
    }
}

fn decode(animation: &Animation, size: Option<u32>, send: async_channel::Sender<TimedFrame>) {
    let (bytes, custom) = match &animation.source {
        AnimationSource::WebP(bytes) => (Cow::Borrowed(bytes.as_ref()), false),
        AnimationSource::Custom(store, id) => {
            let Ok(bytes) = store.read(*id) else { return };
            (Cow::Owned(bytes), true)
        }
    };
    let mut sent = false;
    'cycles: while !send.is_closed() {
        let source = if custom {
            emotes::decode(&bytes)
        } else {
            webp(&bytes).map_err(Into::into)
        };
        let frames = match source {
            Ok(EmoteImage::Still(image)) => {
                let _ = send.send_blocking((prepare(image, size), animation.timeout));
                return;
            }
            Ok(EmoteImage::Animated(frames)) => frames,
            Err(_) => break,
        };

        let mut visible = false;
        let mut pending_blank: Option<TimedFrame> = None;
        for frame in frames {
            let Ok(frame) = frame else { break 'cycles };
            if send.is_closed() {
                return;
            }
            let (n, d) = frame.delay().numer_denom_ms();
            let delay = if n == 0 {
                Duration::from_millis(16)
            } else {
                Duration::from_secs_f64(f64::from(n) / f64::from(d.max(1)) / 1000.0)
            };
            let image = frame.into_buffer();
            // Keep transparent frames inside a cycle, and skip the transparent padding at its edges.
            // 保留一轮动画中间的透明帧，跳过首尾用于填充的透明帧。
            if !custom && !has_visible_pixels(&image) {
                if let Some((_, total)) = &mut pending_blank {
                    *total += delay;
                } else if visible {
                    pending_blank = Some((image, delay));
                }
                continue;
            }
            if let Some((blank, delay)) = pending_blank.take()
                && send.send_blocking((prepare(blank, size), delay)).is_err()
            {
                return;
            }
            if send.send_blocking((prepare(image, size), delay)).is_err() {
                return;
            }
            visible = true;
            sent = true;
        }
        if !visible {
            break;
        }
    }
    if !sent
        && !send.is_closed()
        && let Some(poster) = animation.poster
        && let Ok(image) = image::load_from_memory_with_format(poster, image::ImageFormat::Png)
    {
        let _ = send.send_blocking((prepare(image.into_rgba8(), size), animation.timeout));
    }
}

async fn run(
    animation: Animation,
    size: Option<u32>,
    reduce_motion: bool,
    mut present: impl FnMut(FrameUpdate),
) {
    let expires = Instant::now() + animation.timeout;
    let fade_out = animation.fade_out && !reduce_motion;
    let (send, receive) = async_channel::bounded(2);
    let worker = smol::unblock(move || decode(&animation, size, send));
    let mut shown = false;
    let frames = async {
        let Ok(mut current) = receive.recv().await else {
            return;
        };
        let mut deadline = Instant::now();
        loop {
            deadline += current.1;
            // Coalesce late frames while preserving the source clock across loop boundaries.
            // 合并过期帧，循环衔接时仍沿用素材的时间轴。
            while deadline <= Instant::now() {
                let Ok(next) = receive.try_recv() else { break };
                current = next;
                deadline += current.1;
            }
            if Instant::now() >= expires {
                return;
            }
            present(FrameUpdate::Image(current.0));
            shown = true;
            smol::Timer::at(deadline.min(expires)).await;
            let Ok(next) = receive.recv().await else {
                return;
            };
            current = next;
        }
    };
    smol::future::or(frames, async {
        smol::Timer::at(expires).await;
    })
    .await;
    drop(receive);
    drop(worker);
    if shown && fade_out {
        // Keep the last bitmap in the native window and change only its opacity.
        // 保留原生浮窗中的最后一帧，只更新透明度。
        transition(|hidden| present(FrameUpdate::Opacity(1.0 - hidden))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{EMOTES, PINGS};

    #[test]
    fn text_overlay_anchors_to_all_four_work_area_corners() {
        for (position, expected) in [
            (TextPosition::TopLeft, (24.0, 24.0)),
            (TextPosition::TopRight, (616.0, 24.0)),
            (TextPosition::BottomLeft, (24.0, 656.0)),
            (TextPosition::BottomRight, (616.0, 656.0)),
        ] {
            assert_eq!(text_anchor(position, 640.0, 680.0, 24.0), expected);
            assert_eq!(text_anchor(position, 0.0, 16.0, 24.0), (0.0, 8.0));
        }
    }

    #[test]
    fn text_slide_moves_toward_its_edge_and_fade_holds_position() {
        for (position, motion, y) in [
            (TextPosition::TopRight, TextMotion::Slide, -10.0),
            (TextPosition::BottomRight, TextMotion::Slide, 10.0),
            (TextPosition::BottomRight, TextMotion::Fade, 0.0),
        ] {
            let frame = basic_text_frame(position, motion, 0.5);
            assert_eq!(
                (frame.opacity, frame.offset_x, frame.offset_y),
                (0.5, 0.0, y)
            );
        }
    }

    #[test]
    fn text_presets_move_distinctly_and_settle_on_the_anchor() {
        let right = TextPosition::BottomRight;
        let left = TextPosition::TopLeft;
        let fly_from_left = TextEntrance::FlyIn.frame(right, 0.25);
        let fly_from_right = TextEntrance::FlyIn.frame(left, 0.25);
        assert!(fly_from_left.offset_x < -45.0);
        assert_eq!(fly_from_left.offset_x, -fly_from_right.offset_x);
        assert!(TextEntrance::Shake.frame(right, 0.08).offset_x > 10.0);
        assert!(TextEntrance::Shake.frame(right, 0.25).offset_x < -10.0);
        assert!(TextEntrance::Bounce.frame(left, 0.25).offset_y > 20.0);
        assert!(TextEntrance::Bounce.frame(right, 0.25).offset_y < -20.0);
        for preset in [
            TextEntrance::FlyIn,
            TextEntrance::Shake,
            TextEntrance::Bounce,
        ] {
            let frame = preset.frame(right, 1.0);
            assert_eq!(
                (frame.opacity, frame.offset_x, frame.offset_y),
                (1.0, 0.0, 0.0)
            );
        }
    }

    #[test]
    fn text_uses_the_configured_hold_with_reduced_motion() {
        let settings = TextOverlaySettings {
            timeout_ms: 40,
            ..Default::default()
        };
        let mut presented = None;
        smol::block_on(smol::future::or(
            play_text_overlay(settings, TextEffect::Shake, true, |frame| {
                assert_eq!(
                    (frame.opacity, frame.offset_x, frame.offset_y),
                    (1.0, 0.0, 0.0)
                );
                assert!(presented.replace(Instant::now()).is_none());
            }),
            async {
                smol::Timer::after(Duration::from_secs(1)).await;
                panic!("text overlay did not use the configured hold");
            },
        ));
        assert!(presented.unwrap().elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn concurrent_pings_and_emotes_finish_on_independent_clocks() {
        let executor = smol::LocalExecutor::new();
        smol::block_on(executor.run(async {
            let requested = Instant::now();
            let tasks: Vec<_> = (0..8)
                .map(|i| {
                    executor.spawn(async move {
                        let clip = if i % 2 == 0 { PINGS[i] } else { EMOTES[i] };
                        let animation =
                            Animation::webp(clip.animation, 110, Duration::from_millis(400));
                        let mut frames = 0;
                        let mut first = None;
                        run(animation, None, false, |update| {
                            if matches!(update, FrameUpdate::Image(_)) {
                                first.get_or_insert_with(|| requested.elapsed());
                                frames += 1;
                            }
                        })
                        .await;
                        (first.unwrap(), frames, requested.elapsed())
                    })
                })
                .collect();
            for task in tasks {
                let (first, frames, elapsed) = task.await;
                assert!(frames > 2);
                assert!(elapsed >= Duration::from_millis(400));
                eprintln!("first frame {first:?}, presented {frames}, completed {elapsed:?}");
            }
        }));
    }

    #[test]
    fn still_emote_preserves_aspect_and_fades_the_same_frame() {
        let source = RgbaImage::from_pixel(8, 4, image::Rgba([255, 128, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .unwrap();
        let animation =
            Animation::webp(bytes.into_inner(), 16, Duration::from_millis(100)).with_fade_out();
        let owner = Rc::new(());
        let presenter = owner.clone();
        let mut frames = 0;
        let mut opacities = Vec::new();
        let count = &mut frames;
        let opacity_values = &mut opacities;
        smol::block_on(run(animation, Some(16), false, move |update| {
            let _keep_alive = &presenter;
            match update {
                FrameUpdate::Image(image) => {
                    assert!(opacity_values.is_empty());
                    assert_eq!(image.dimensions(), (16, 16));
                    assert_eq!(image.get_pixel(8, 0)[3], 0);
                    assert_eq!(image.get_pixel(8, 8)[3], 255);
                    *count += 1;
                }
                FrameUpdate::Opacity(opacity) => opacity_values.push(opacity),
            }
        }));
        assert_eq!(frames, 1);
        assert!(opacities.first().unwrap() > &0.9);
        assert_eq!(opacities.last(), Some(&0.0));
        assert!(opacities.windows(2).all(|pair| pair[0] >= pair[1]));
        assert_eq!(Rc::strong_count(&owner), 1);
    }

    #[test]
    fn ping_and_emote_restart_without_transparent_padding() {
        for clip in [PINGS[3], EMOTES[0]] {
            let animation = Animation::webp(clip.animation, 110, Duration::from_secs(8));
            smol::block_on(async {
                let (send, receive) = async_channel::bounded(2);
                let _worker = smol::unblock(move || decode(&animation, None, send));
                for _ in 0..2 {
                    let frames = WebPDecoder::new(Cursor::new(clip.animation))
                        .unwrap()
                        .into_frames()
                        .map(Result::unwrap)
                        .filter(|frame| has_visible_pixels(frame.buffer()));
                    for frame in frames {
                        let (image, delay) = receive.recv().await.unwrap();
                        let (n, d) = frame.delay().numer_denom_ms();
                        assert_eq!(image, prepare(frame.buffer().clone(), None));
                        assert_eq!(
                            delay,
                            Duration::from_secs_f64(f64::from(n) / f64::from(d) / 1000.0)
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn invalid_webp_delivers_one_poster() {
        let animation = Animation::webp(&b"broken"[..], 110, Duration::from_millis(100))
            .with_poster(PINGS[0].image);
        let mut frames = 0;
        smol::block_on(run(animation, Some(110), false, |update| {
            let FrameUpdate::Image(image) = update else {
                panic!("pings do not fade");
            };
            assert_eq!(image.dimensions(), (110, 110));
            frames += 1;
        }));
        assert_eq!(frames, 1);
    }

    #[test]
    fn imported_gif_plays_both_frames_and_loops_from_the_cache() {
        use image::{Delay, Frame, Rgba, codecs::gif::GifEncoder};

        let folder =
            std::env::temp_dir().join(format!("pingpingping-gif-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("test.gif");
        let first = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
        let second = RgbaImage::from_pixel(4, 4, Rgba([0, 255, 0, 255]));
        {
            let mut encoder = GifEncoder::new(std::fs::File::create(&path).unwrap());
            for image in [first.clone(), second.clone()] {
                encoder
                    .encode_frame(Frame::from_parts(
                        image,
                        0,
                        0,
                        Delay::from_numer_denom_ms(50, 1),
                    ))
                    .unwrap();
            }
        }
        let store = EmoteStore::new(&folder);
        let emote = store.import(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let animation = Animation::custom(store, emote.id(), 4, Duration::from_secs(1));
        smol::block_on(async {
            let (send, receive) = async_channel::bounded(2);
            let worker = smol::unblock(move || decode(&animation, None, send));
            for image in [first.clone(), second, first] {
                let (frame, delay) = receive.recv().await.unwrap();
                assert_eq!(frame, prepare(image, None));
                assert_eq!(delay, Duration::from_millis(50));
            }
            receive.close();
            worker.await;
        });
        std::fs::remove_dir_all(folder).unwrap();
    }
}
