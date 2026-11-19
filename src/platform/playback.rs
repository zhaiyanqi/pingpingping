//! On-demand desktop animation playback, shared by both native backends.
//! 两个原生后端共用的按需桌面动画播放器。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{App, Task};
use image::{AnimationDecoder, DynamicImage, ImageResult, RgbaImage, codecs::webp::WebPDecoder};

use crate::emotes::{self, EmoteId, EmoteImage, EmoteStore, has_visible_pixels};
use crate::gesture::Position;
use crate::model::Location;

use super::animation_presenter;

pub(super) const MAX_ACTIVE: usize = 16;

pub(crate) struct Animation {
    size: u32,
    source: AnimationSource,
    poster: Option<&'static [u8]>,
    timeout: Duration,
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
        }
    }

    pub(super) fn custom(store: EmoteStore, id: EmoteId, size: u32, timeout: Duration) -> Self {
        Self {
            size: size.clamp(1, 1024),
            source: AnimationSource::Custom(store, id),
            poster: None,
            timeout,
        }
    }

    pub(super) fn with_poster(mut self, png: &'static [u8]) -> Self {
        self.poster = Some(png);
        self
    }
}

#[derive(Default)]
pub(crate) struct AnimationOverlay {
    next: u64,
    active: Rc<RefCell<BTreeMap<u64, Task<()>>>>,
}

impl AnimationOverlay {
    pub(crate) fn play(
        &mut self,
        animation: Animation,
        local: Option<Position>,
        location: Option<Location>,
        cx: &App,
    ) {
        let Some((output_pixels, present)) = animation_presenter(animation.size, local, location)
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
        let task = cx.spawn(async move |_| {
            run(animation, output_pixels, present).await;
            if let Some(registry) = registry.upgrade()
                && let Some(task) = registry.borrow_mut().remove(&id)
            {
                task.detach();
            }
        });
        active.insert(id, task);
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

async fn run(animation: Animation, size: Option<u32>, mut present: impl FnMut(RgbaImage)) {
    let expires = Instant::now() + animation.timeout;
    let (send, receive) = async_channel::bounded(2);
    let _worker = smol::unblock(move || decode(&animation, size, send));
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
            present(current.0);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{EMOTES, PINGS};

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
                        run(animation, None, |_| {
                            first.get_or_insert_with(|| requested.elapsed());
                            frames += 1;
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
    fn owned_webp_preserves_aspect_and_releases_the_presenter() {
        let source = RgbaImage::from_pixel(8, 4, image::Rgba([255, 128, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .unwrap();
        let animation = Animation::webp(bytes.into_inner(), 16, Duration::from_millis(100));
        let owner = Rc::new(());
        let presenter = owner.clone();
        let mut frames = 0;
        let count = &mut frames;
        smol::block_on(run(animation, Some(16), move |image| {
            let _keep_alive = &presenter;
            assert_eq!(image.dimensions(), (16, 16));
            assert_eq!(image.get_pixel(8, 0)[3], 0);
            assert_eq!(image.get_pixel(8, 8)[3], 255);
            *count += 1;
        }));
        assert_eq!(frames, 1);
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
        smol::block_on(run(animation, Some(110), |image| {
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
