//! Platform selection and shared media decoding and audio playback.
//! 平台后端选择，以及共用的媒体解码与音频播放。

use std::io::Cursor;
use std::time::Duration;

use async_channel::Sender;
use image::{AnimationDecoder, RgbaImage, codecs::webp::WebPDecoder, imageops};
use rodio::{Decoder, DeviceSinkBuilder, Player, Source, buffer::SamplesBuffer};

use crate::assets::{Clip, EMOTES, PINGS};
use crate::gesture::Wheel;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::*;

fn decode_animation(bytes: &'static [u8], size: u32) -> Vec<(RgbaImage, Duration)> {
    let Ok(decoder) = WebPDecoder::new(Cursor::new(bytes)) else {
        return Vec::new();
    };
    decoder
        .into_frames()
        .take(80)
        .map_while(Result::ok)
        .map(|frame| {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = (u64::from(numerator) / u64::from(denominator.max(1))).clamp(16, 220);
            let scaled =
                imageops::resize(frame.buffer(), size, size, imageops::FilterType::Triangle);
            (scaled, Duration::from_millis(delay))
        })
        .collect()
}

// Predecode bundled clips on a worker so playback can enqueue them immediately.
// 在后台预解码内置音效，播放时可立即加入音频队列。
fn start_audio() -> Sender<(Wheel, usize)> {
    let (sender, receiver) = async_channel::bounded::<(Wheel, usize)>(12);
    let _ = std::thread::Builder::new()
        .name("native-audio".into())
        .spawn(move || {
            let output = match DeviceSinkBuilder::open_default_sink() {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("无法打开默认音频输出：{error}");
                    return;
                }
            };
            let decode = |clip: Clip| {
                let decoder = Decoder::try_from(Cursor::new(clip.sound)).ok()?;
                let (channels, rate) = (decoder.channels(), decoder.sample_rate());
                Some(SamplesBuffer::new(
                    channels,
                    rate,
                    decoder.collect::<Vec<_>>(),
                ))
            };
            let pings = PINGS.map(decode);
            let emotes = EMOTES.map(decode);
            let players: [Player; 3] = std::array::from_fn(|_| Player::connect_new(output.mixer()));
            let sounds = std::iter::from_fn(|| receiver.recv_blocking().ok());
            for (slot, (wheel, index)) in (0..players.len()).cycle().zip(sounds) {
                let sound = match wheel {
                    Wheel::Ping => &pings[index],
                    Wheel::Emote => &emotes[index],
                };
                if let Some(sound) = sound {
                    players[slot].clear();
                    players[slot].append(sound.clone());
                    players[slot].play();
                }
            }
        });
    sender
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_effect_has_decodable_audio_and_emotes_have_frames() {
        for clip in PINGS.iter().chain(&EMOTES) {
            let mut audio = Decoder::try_from(Cursor::new(clip.sound)).unwrap();
            assert!(audio.any(|sample| sample.abs() > 0.001), "{}", clip.name);
            if let Some(bytes) = clip.animation {
                let decoder = WebPDecoder::new(Cursor::new(bytes)).unwrap();
                let mut frames = decoder.into_frames();
                assert!(
                    matches!((frames.next(), frames.next()), (Some(Ok(_)), Some(Ok(_)))),
                    "{}",
                    clip.name
                );
            }
        }
    }
}
