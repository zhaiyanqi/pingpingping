//! Platform selection and shared media decoding and audio playback.
//! 平台后端选择，以及共用的媒体解码与音频播放。

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, Player};

use crate::assets::{EMOTES, PINGS, WHEEL_TICK};
use crate::emotes::{EmoteId, EmoteStore};
use crate::gesture::Wheel;
use crate::model::{EmotePage, OverlaySettings, WheelEmote};

mod playback;
pub(crate) use playback::{Animation, AnimationOverlay};
mod wheel;
pub(crate) use wheel::{WheelFrame, WheelPreview, editor_sector, load_wheel_posters, slot_center};

pub(crate) enum WheelContents {
    Ping,
    Emote(Vec<EmotePage>),
}

impl WheelContents {
    fn kind(&self) -> Wheel {
        match self {
            Self::Ping => Wheel::Ping,
            Self::Emote(_) => Wheel::Emote,
        }
    }
}

enum AudioCue {
    Clip(Wheel, usize, f32),
    Tick(f32),
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::*;

impl Overlay {
    pub(crate) fn set_wheel_posters(
        &mut self,
        posters: HashMap<EmoteId, tiny_skia::Pixmap>,
        cx: &gpui_kit::App,
    ) {
        self.wheel_view.set_posters(posters, cx);
    }

    pub(crate) fn turn_wheel_page(
        &mut self,
        steps: i32,
        selected: Option<usize>,
        tick_volume: Option<f32>,
        cx: &gpui_kit::App,
    ) {
        if self.wheel_view.turn_page(steps, selected, cx)
            && let Some(volume) = tick_volume
        {
            let _ = self.audio.try_send(AudioCue::Tick(volume));
        }
    }

    pub(crate) fn wheel_emote(&self, slot: usize) -> Option<WheelEmote> {
        self.wheel_view.emote(slot)
    }

    pub(crate) fn play_custom(
        &mut self,
        store: EmoteStore,
        id: EmoteId,
        local: Option<crate::gesture::Position>,
        settings: OverlaySettings,
        cx: &gpui_kit::App,
    ) {
        let timeout = Duration::from_millis(u64::from(settings.emote_timeout_ms));
        let animation = Animation::custom(store, id, settings.emote_size, timeout);
        self.animations.play(animation, local, None, cx);
    }

    pub(crate) fn play(
        &mut self,
        kind: Wheel,
        index: usize,
        local: Option<crate::gesture::Position>,
        location: Option<crate::model::Location>,
        settings: OverlaySettings,
        cx: &gpui_kit::App,
    ) {
        let (clip, size, timeout_ms) = match kind {
            Wheel::Ping => (PINGS[index], settings.ping_size, settings.ping_timeout_ms),
            Wheel::Emote => (
                EMOTES[index],
                settings.emote_size,
                settings.emote_timeout_ms,
            ),
        };
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        // The source canvases include room for expanding rings and emote effects.
        // 素材画布为扩散光环和表情特效预留了空间。
        let canvas = match kind {
            Wheel::Ping => (size * 256).div_ceil(110),
            Wheel::Emote => (size * 5).div_ceil(4),
        };
        let animation = Animation::webp(clip.animation, canvas, timeout).with_poster(clip.image);
        self.animations.play(animation, local, location, cx);
        if let Some(volume) = settings
            .volume()
            .filter(|_| kind != Wheel::Emote || settings.emote_sound)
        {
            let _ = self.audio.try_send(AudioCue::Clip(kind, index, volume));
        }
    }
}

// Open the device for an audible burst; after silence the worker sleeps on the channel.
// 收到音效再打开设备，静音后关闭设备，线程阻塞等待下一次请求。
fn start_audio() -> SyncSender<AudioCue> {
    let (sender, receiver) = mpsc::sync_channel::<AudioCue>(32);
    let _ = std::thread::Builder::new()
        .name("native-audio".into())
        .spawn(move || {
            while let Ok(mut cue) = receiver.recv() {
                let mut output = match DeviceSinkBuilder::open_default_sink() {
                    Ok(output) => output,
                    Err(error) => {
                        eprintln!("无法打开默认音频输出：{error}");
                        continue;
                    }
                };
                output.log_on_drop(false);
                let ticks = Player::connect_new(output.mixer());
                let mut voices = std::collections::VecDeque::<Player>::new();
                'burst: loop {
                    let (voice, bytes, volume) = match cue {
                        AudioCue::Clip(kind, index, volume) => {
                            voices.retain(|voice| !voice.empty());
                            if voices.len() == playback::MAX_ACTIVE {
                                voices.pop_front();
                            }
                            voices.push_back(Player::connect_new(output.mixer()));
                            let bytes = match kind {
                                Wheel::Ping => PINGS[index].sound,
                                Wheel::Emote => EMOTES[index].sound,
                            };
                            (voices.back().unwrap(), bytes, volume)
                        }
                        AudioCue::Tick(volume) => {
                            ticks.clear();
                            (&ticks, WHEEL_TICK, volume * 0.5)
                        }
                    };
                    if let Ok(source) = Decoder::try_from(Cursor::new(bytes)) {
                        voice.set_volume(volume);
                        voice.append(source);
                        voice.play();
                    }
                    loop {
                        match receiver.recv_timeout(Duration::from_secs(2)) {
                            Ok(next) => {
                                cue = next;
                                break;
                            }
                            Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout)
                                if ticks.empty() && voices.iter().all(Player::empty) =>
                            {
                                break 'burst;
                            }
                            Err(RecvTimeoutError::Timeout) => continue,
                        }
                    }
                }
            }
        });
    sender
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{AnimationDecoder, codecs::webp::WebPDecoder};

    #[test]
    fn every_effect_has_audio_and_distinct_animation_frames() {
        for clip in PINGS.iter().chain(&EMOTES) {
            let mut audio = Decoder::try_from(Cursor::new(clip.sound)).unwrap();
            assert!(audio.any(|sample| sample.abs() > 0.001), "{}", clip.name);
            let decoder = WebPDecoder::new(Cursor::new(clip.animation)).unwrap();
            let mut frames = decoder.into_frames();
            let first = frames.next().unwrap().unwrap().into_buffer();
            assert!(
                frames
                    .take(8)
                    .map(Result::unwrap)
                    .any(|f| f.buffer() != &first),
                "{}",
                clip.name
            );
        }
    }
}
