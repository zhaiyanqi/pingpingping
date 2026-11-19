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
use crate::gesture::Wheel;
use crate::model::{TextEffect, TextMotion, TextOverlaySettings, TextPosition};

use super::{AnimationPosition, animation_presenter, system_reduces_motion, text_presenter};

pub(super) const MAX_ACTIVE: usize = 16;
const TRANSITION_DURATION: Duration = Duration::from_millis(180);
const CUSTOM_EMOTE_FADE_DURATION: Duration = Duration::from_millis(320);
const TRANSITION_FRAME: Duration = Duration::from_millis(16);
pub(super) const TEXT_WIDTH: u32 = 360;
pub(super) const TEXT_PADDING: u32 = 14;
pub(super) const TEXT_RADIUS: u32 = 12;
pub(super) const TEXT_FONT_SIZE: u32 = 16;
pub(super) const TEXT_MAX_LINES: u32 = 6;
pub(super) const TEXT_MARGIN: u32 = 24;
pub(super) const TAG_WIDTH: u32 = 220;
pub(super) const TAG_HEIGHT: u32 = 20;
pub(super) const TAG_PADDING: u32 = 7;
pub(super) const TAG_RADIUS: u32 = 4;
pub(super) const TAG_FONT_SIZE: u32 = 13;
pub(super) const TAG_FOREGROUND: u32 = 0x4fc3f7ff;
pub(super) const TAG_BACKGROUND: u32 = 0x080c109e;

pub(super) struct SenderTag<'a> {
    pub(super) name: &'a str,
    pub(super) offset: f64,
}

impl<'a> SenderTag<'a> {
    pub(super) fn new(name: &'a str, art_size: u32, kind: Wheel) -> Self {
        // Match lolPing's placement against the art size, excluding the padded animation canvas.
        // 按 lolPing 的位置以图案尺寸为基准，不计动画画布的留白。
        let gap = match kind {
            Wheel::Ping => 0.0,
            Wheel::Emote => 8.0,
        };
        Self {
            name,
            offset: f64::from(art_size) / 2.0 + gap,
        }
    }
}

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

#[derive(Clone, Copy)]
pub(super) enum FrameSize {
    #[cfg(any(target_os = "windows", test))]
    Square(u32),
    #[cfg(any(target_os = "macos", test))]
    FitWithin(u32),
    #[cfg(any(target_os = "macos", test))]
    Original,
}

impl FrameSize {
    fn prepare(self, image: RgbaImage) -> RgbaImage {
        match self {
            #[cfg(any(target_os = "windows", test))]
            Self::Square(side) => prepare(image, Some(side)),
            #[cfg(any(target_os = "macos", test))]
            Self::FitWithin(side) if image.width().max(image.height()) > side => {
                prepare(downsize(image, side), None)
            }
            #[cfg(any(target_os = "macos", test))]
            Self::FitWithin(_) | Self::Original => prepare(image, None),
        }
    }
}

pub(crate) struct Animation {
    size: u32,
    source: AnimationSource,
    poster: Option<&'static [u8]>,
    timeout: Duration,
    fade_out: Option<Duration>,
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
            fade_out: None,
        }
    }

    pub(super) fn custom(store: EmoteStore, id: EmoteId, size: u32, timeout: Duration) -> Self {
        Self {
            size: size.clamp(1, 1024),
            source: AnimationSource::Custom(store, id),
            poster: None,
            timeout,
            fade_out: Some(CUSTOM_EMOTE_FADE_DURATION),
        }
    }

    pub(super) fn with_poster(mut self, png: &'static [u8]) -> Self {
        self.poster = Some(png);
        self
    }

    pub(super) fn with_fade_out(mut self) -> Self {
        self.fade_out = Some(TRANSITION_DURATION);
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
    pub(super) fn play(
        &mut self,
        animation: Animation,
        tag: SenderTag<'_>,
        position: AnimationPosition,
        cx: &App,
    ) {
        let Some((output_pixels, mut present)) = animation_presenter(animation.size, tag, position)
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
        let reduce_motion =
            animation.fade_out.is_some() && (cx.reduce_motion() || system_reduces_motion());
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
        transition(TRANSITION_DURATION, |hidden| {
            present(basic_text_frame(settings.position, exit, hidden))
        })
        .await;
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

async fn transition(duration: Duration, mut present: impl FnMut(f32)) {
    animate(duration, |progress| present(progress.powi(2))).await;
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

struct TimedFrame {
    image: RgbaImage,
    delay: Duration,
    visible: bool,
}

impl TimedFrame {
    fn new(image: RgbaImage, delay: Duration) -> Self {
        Self {
            visible: has_visible_pixels(&image),
            image,
            delay,
        }
    }
}

fn premultiply(value: u8, alpha: u8) -> u8 {
    ((u16::from(value) * u16::from(alpha) + 127) / 255) as u8
}

#[cfg(any(target_os = "macos", test))]
fn downsize(mut image: RgbaImage, size: u32) -> RgbaImage {
    // Filter premultiplied colors to keep transparent pixels from darkening the edges.
    // 用预乘颜色缩放，避免透明像素压暗图像边缘。
    for [r, g, b, alpha] in image.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
        if *alpha != 255 {
            [*r, *g, *b] = [*r, *g, *b].map(|value| premultiply(value, *alpha));
        }
    }
    let mut fitted = DynamicImage::ImageRgba8(image)
        .resize(size, size, image::imageops::FilterType::Triangle)
        .into_rgba8();
    for [r, g, b, alpha] in fitted.as_flat_samples_mut().samples.as_chunks_mut::<4>().0 {
        if *alpha > 0 && *alpha < 255 {
            let alpha = u16::from(*alpha);
            let restore = |value| ((u16::from(value) * 255 + alpha / 2) / alpha).min(255) as u8;
            [*r, *g, *b] = [*r, *g, *b].map(restore);
        }
    }
    fitted
}

pub(super) fn prepare(image: RgbaImage, size: Option<u32>) -> RgbaImage {
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
            *pixel = [premultiply(b, a), premultiply(g, a), premultiply(r, a), a];
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

fn decode(animation: &Animation, size: FrameSize, send: async_channel::Sender<TimedFrame>) {
    let (bytes, custom) = match &animation.source {
        AnimationSource::WebP(bytes) => (Cow::Borrowed(bytes.as_ref()), false),
        AnimationSource::Custom(store, id) => {
            let Ok(bytes) = store.read(*id) else { return };
            (Cow::Owned(bytes), true)
        }
    };
    // Built-in art already has small canvases. Bound custom frames before queuing them for AppKit.
    // 内置素材的画布已经较小，只在帧进入 AppKit 队列前缩限自定义表情。
    #[cfg(any(target_os = "macos", test))]
    let size = match size {
        FrameSize::FitWithin(_) if !custom => FrameSize::Original,
        size => size,
    };
    let send_frame = |mut frame: TimedFrame| {
        frame.image = size.prepare(frame.image);
        send.send_blocking(frame).is_ok()
    };
    let mut sent = false;
    let mut pending_blank: Option<TimedFrame> = None;
    'cycles: while !send.is_closed() {
        let source = if custom {
            emotes::decode(&bytes)
        } else {
            webp(&bytes).map_err(Into::into)
        };
        let frames = match source {
            Ok(EmoteImage::Still(image)) => {
                send_frame(TimedFrame::new(image, animation.timeout));
                return;
            }
            Ok(EmoteImage::Animated(frames)) => frames,
            Err(_) => break,
        };

        let mut visible = false;
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
            let frame = TimedFrame::new(frame.into_buffer(), delay);
            // Merge adjacent blanks, including across loops for custom images that keep their source timing.
            // 合并相邻透明帧，自定义图片保留原始时长，可以跨循环合并。
            if !frame.visible {
                if let Some(blank) = &mut pending_blank {
                    blank.delay += frame.delay;
                } else if custom || visible {
                    pending_blank = Some(frame);
                }
                continue;
            }
            if let Some(blank) = pending_blank.take()
                && !send_frame(blank)
            {
                return;
            }
            if !send_frame(frame) {
                return;
            }
            visible = true;
            sent = true;
        }
        if !visible {
            break;
        }
        if !custom {
            pending_blank = None;
        }
    }
    if !sent
        && !send.is_closed()
        && let Some(poster) = animation.poster
        && let Ok(image) = image::load_from_memory_with_format(poster, image::ImageFormat::Png)
    {
        send_frame(TimedFrame::new(image.into_rgba8(), animation.timeout));
    }
}

async fn run(
    animation: Animation,
    size: FrameSize,
    reduce_motion: bool,
    mut present: impl FnMut(FrameUpdate),
) {
    let expires = Instant::now() + animation.timeout;
    let fade_out = animation.fade_out.filter(|_| !reduce_motion);
    let keep_visible_at_exit =
        fade_out.is_some() && matches!(animation.source, AnimationSource::Custom(..));
    let (send, receive) = async_channel::bounded(2);
    let worker = smol::unblock(move || decode(&animation, size, send));
    let mut visible = false;
    let frames = async {
        let Ok(mut current) = receive.recv().await else {
            return;
        };
        let mut deadline = Instant::now();
        loop {
            deadline += current.delay;
            // Coalesce late frames while preserving the source clock across loop boundaries.
            // 合并过期帧，循环衔接时仍沿用素材的时间轴。
            while deadline <= Instant::now() {
                let Ok(next) = receive.try_recv() else { break };
                current = next;
                deadline += current.delay;
            }
            if Instant::now() >= expires {
                return;
            }
            // Keep the picture on screen if a blank run would otherwise hide it through the exit.
            // 如果透明帧会一直持续到退场，就保留屏幕上当前的画面。
            let blank_at_exit =
                keep_visible_at_exit && visible && !current.visible && deadline >= expires;
            if !blank_at_exit {
                visible = current.visible;
                present(FrameUpdate::Image(current.image));
            }
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
    if let Some(duration) = fade_out.filter(|_| visible) {
        // Keep the last bitmap in the native window and change only its opacity.
        // 保留原生浮窗中的最后一帧，只更新透明度。
        transition(duration, |hidden| {
            present(FrameUpdate::Opacity(1.0 - hidden))
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{EMOTES, PINGS};

    #[test]
    fn sender_tag_follows_the_art_size() {
        let ping = SenderTag::new("队友", 110, Wheel::Ping);
        let emote = SenderTag::new("队友", 150, Wheel::Emote);
        assert_eq!(ping.offset, 55.0);
        assert_eq!(emote.offset, 83.0);
    }

    #[test]
    fn custom_frames_fit_the_display_without_upscaling_or_padding() {
        let color = image::Rgba([255, 128, 0, 255]);
        let frame_size = FrameSize::FitWithin(300);
        let large = frame_size.prepare(RgbaImage::from_pixel(2048, 1024, color));
        assert_eq!(large.dimensions(), (300, 150));
        assert_eq!(large.get_pixel(0, 0)[3], 255);

        let small = frame_size.prepare(RgbaImage::from_pixel(80, 40, color));
        assert_eq!(small.dimensions(), (80, 40));

        let mut edge = RgbaImage::new(2, 1);
        edge.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        let fitted = downsize(edge, 1);
        let [red, _, _, alpha] = fitted.get_pixel(0, 0).0;
        assert_eq!(red, 255);
        assert!((1..255).contains(&alpha));
    }

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
                        run(animation, FrameSize::Original, false, |update| {
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
        let folder =
            std::env::temp_dir().join(format!("pingpingping-png-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("test.png");
        source.save(&path).unwrap();
        let store = EmoteStore::new(&folder);
        let emote = store.import(&path).unwrap();
        let timeout = Duration::from_millis(100);
        let animation = Animation::custom(store, emote.id(), 16, timeout);
        let owner = Rc::new(());
        let presenter = owner.clone();
        let mut frames = 0;
        let mut opacities = Vec::new();
        let count = &mut frames;
        let opacity_values = &mut opacities;
        let start = Instant::now();
        smol::block_on(run(
            animation,
            FrameSize::Square(16),
            false,
            move |update| {
                let _keep_alive = &presenter;
                match update {
                    FrameUpdate::Image(image) => {
                        assert!(opacity_values.is_empty());
                        assert_eq!(image.dimensions(), (16, 16));
                        assert_eq!(image.get_pixel(8, 0)[3], 0);
                        assert_eq!(image.get_pixel(8, 8)[3], 255);
                        *count += 1;
                    }
                    FrameUpdate::Opacity(opacity) => {
                        assert!(start.elapsed() >= timeout);
                        opacity_values.push(opacity);
                    }
                }
            },
        ));
        std::fs::remove_dir_all(folder).unwrap();
        assert_eq!(frames, 1);
        assert!(opacities.first().unwrap() > &0.9);
        assert!(
            opacities
                .iter()
                .any(|&opacity| opacity > 0.0 && opacity < 0.9)
        );
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
                let _worker = smol::unblock(move || decode(&animation, FrameSize::Original, send));
                for _ in 0..2 {
                    let frames = WebPDecoder::new(Cursor::new(clip.animation))
                        .unwrap()
                        .into_frames()
                        .map(Result::unwrap)
                        .filter(|frame| has_visible_pixels(frame.buffer()));
                    for frame in frames {
                        let TimedFrame { image, delay, .. } = receive.recv().await.unwrap();
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
        smol::block_on(run(animation, FrameSize::Square(110), false, |update| {
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
            let worker = smol::unblock(move || decode(&animation, FrameSize::Original, send));
            for image in [first.clone(), second, first] {
                let frame = receive.recv().await.unwrap();
                assert_eq!(frame.image, prepare(image, None));
                assert_eq!(frame.delay, Duration::from_millis(50));
            }
            receive.close();
            worker.await;
        });
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn imported_gif_fades_the_visible_frame_when_a_blank_run_spans_the_timeout() {
        use image::{Delay, Frame, Rgba, codecs::gif::GifEncoder};

        let folder =
            std::env::temp_dir().join(format!("pingpingping-gif-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("transparent-tail.gif");
        let visible = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
        let blank = RgbaImage::new(4, 4);
        GifEncoder::new(std::fs::File::create(&path).unwrap())
            .encode_frames(
                [(visible.clone(), 150), (blank.clone(), 80), (blank, 400)].map(
                    |(image, delay)| {
                        Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(delay, 1))
                    },
                ),
            )
            .unwrap();

        let store = EmoteStore::new(&folder);
        let emote = store.import(&path).unwrap();
        let timeout = Duration::from_millis(350);
        let animation = Animation::custom(store, emote.id(), 4, timeout);
        let start = Instant::now();
        let mut last_image = None;
        let mut opacities = Vec::new();
        smol::block_on(run(
            animation,
            FrameSize::Square(4),
            false,
            |update| match update {
                FrameUpdate::Image(image) => last_image = Some(image),
                FrameUpdate::Opacity(opacity) => {
                    assert!(start.elapsed() >= timeout);
                    opacities.push(opacity);
                }
            },
        ));
        std::fs::remove_dir_all(folder).unwrap();
        assert_eq!(last_image, Some(prepare(visible, None)));
        assert!(
            opacities
                .iter()
                .any(|&opacity| opacity > 0.0 && opacity < 0.9)
        );
        assert_eq!(opacities.last(), Some(&0.0));
    }
}
