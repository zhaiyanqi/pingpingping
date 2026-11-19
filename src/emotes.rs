//! Content-addressed custom emotes and the local import library.
//! 按内容寻址的自定义表情与本机导入库。

use std::fmt;
use std::fs::{self, File};
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
use image::{
    AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageFormat, ImageReader, RgbaImage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const MAX_EMOTE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIDE: u32 = 2048;
const MAX_FRAMES: usize = 240;
const MAX_ANIMATION_PIXELS: u64 = 64 * 1024 * 1024;
const POSTER_SIZE: u32 = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct EmoteId([u8; 32]);

impl EmoteId {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    fn from_filename(name: &str) -> Option<Self> {
        let mut digest = [0; 32];
        let name = name.as_bytes();
        if name.len() != digest.len() * 2 {
            return None;
        }
        for (byte, pair) in digest.iter_mut().zip(name.as_chunks::<2>().0) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(digest))
    }
}

impl fmt::Display for EmoteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct EmoteStore(PathBuf);

pub(crate) struct LibraryEmote {
    id: EmoteId,
    name: String,
}

impl LibraryEmote {
    pub(crate) fn id(&self) -> EmoteId {
        self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl EmoteStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self(data_dir.join("emotes"))
    }

    fn path(&self, id: EmoteId, extension: &str) -> PathBuf {
        self.0.join(format!("{id}.{extension}"))
    }

    pub(crate) fn poster(&self, id: EmoteId) -> PathBuf {
        self.path(id, "png")
    }

    pub(crate) fn read(&self, id: EmoteId) -> Result<Vec<u8>> {
        let bytes = read_image(&self.path(id, "image"))?;
        ensure!(EmoteId::from_bytes(&bytes) == id, "表情校验失败");
        Ok(bytes)
    }

    pub(crate) fn contains(&self, id: EmoteId) -> bool {
        self.poster(id).is_file() && self.read(id).is_ok()
    }

    pub(crate) fn save(&self, id: EmoteId, bytes: &[u8]) -> Result<()> {
        ensure!(bytes.len() <= MAX_EMOTE_BYTES, "表情文件不能超过 8 MiB");
        ensure!(EmoteId::from_bytes(bytes) == id, "表情校验失败");
        if self.contains(id) {
            return Ok(());
        }
        let image = poster(bytes).context("无法读取表情图片")?;
        let image = DynamicImage::ImageRgba8(image).thumbnail(POSTER_SIZE, POSTER_SIZE);
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png)?;
        fs::create_dir_all(&self.0).context("无法创建表情目录")?;
        // Commit the original last so a cache hit always has a completed preview.
        // 最后提交原图，确保命中缓存时已有完整的预览图。
        write_atomic(&self.poster(id), &png.into_inner()).context("无法保存表情预览")?;
        write_atomic(&self.path(id, "image"), bytes).context("无法保存表情图片")
    }

    pub(crate) fn import(&self, path: &Path) -> Result<LibraryEmote> {
        let bytes = read_image(path)?;
        let id = EmoteId::from_bytes(&bytes);
        self.save(id, &bytes)?;
        let mut name: String = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .filter(|character| !character.is_control())
            .take(64)
            .collect();
        if name.trim().is_empty() {
            name = "自定义表情".into();
        }
        write_atomic(&self.path(id, "name"), name.as_bytes()).context("无法保存表情名称")?;
        Ok(LibraryEmote { id, name })
    }

    pub(crate) fn library(&self) -> Result<Vec<LibraryEmote>> {
        let entries = match fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("无法读取自定义表情"),
        };
        let mut emotes = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "name") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(EmoteId::from_filename)
            else {
                continue;
            };
            if self.path(id, "image").is_file() && self.poster(id).is_file() {
                emotes.push(LibraryEmote {
                    id,
                    name: fs::read_to_string(path)?,
                });
            }
        }
        emotes.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(emotes)
    }
}

fn read_image(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).context("无法打开表情图片")?;
    ensure!(file.metadata()?.is_file(), "请选择图片文件");
    let mut bytes = Vec::new();
    file.take((MAX_EMOTE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_EMOTE_BYTES, "表情文件不能超过 8 MiB");
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) enum EmoteImage<'a> {
    Still(RgbaImage),
    Animated(Frames<'a>),
}

pub(crate) fn decode(bytes: &[u8]) -> Result<EmoteImage<'_>> {
    let cursor = Cursor::new(bytes);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    match image::guess_format(bytes) {
        Ok(ImageFormat::Gif) => {
            let mut decoder = GifDecoder::new(cursor)?;
            decoder.set_limits(limits)?;
            Ok(EmoteImage::Animated(decoder.into_frames()))
        }
        Ok(ImageFormat::WebP) => {
            let decoder = WebPDecoder::new(cursor)?;
            let (width, height) = decoder.dimensions();
            limits.check_dimensions(width, height)?;
            if decoder.has_animation() {
                Ok(EmoteImage::Animated(decoder.into_frames()))
            } else {
                Ok(EmoteImage::Still(
                    DynamicImage::from_decoder(decoder)?.into_rgba8(),
                ))
            }
        }
        Ok(ImageFormat::Png) => {
            let decoder = PngDecoder::with_limits(cursor, limits)?;
            if decoder.is_apng()? {
                Ok(EmoteImage::Animated(decoder.apng()?.into_frames()))
            } else {
                Ok(EmoteImage::Still(
                    DynamicImage::from_decoder(decoder)?.into_rgba8(),
                ))
            }
        }
        Ok(ImageFormat::Jpeg) => {
            let mut reader = ImageReader::with_format(cursor, ImageFormat::Jpeg);
            reader.limits(limits);
            Ok(EmoteImage::Still(reader.decode()?.into_rgba8()))
        }
        _ => anyhow::bail!("请选择 GIF、PNG、JPEG 或 WebP 图片"),
    }
}

fn poster(bytes: &[u8]) -> Result<RgbaImage> {
    let frames = match decode(bytes)? {
        EmoteImage::Still(image) => return Ok(image),
        EmoteImage::Animated(frames) => frames,
    };
    let mut poster = None;
    let mut pixels = 0;
    // Read the entire animation before publishing it, with a bound on cumulative decode work.
    // 发布前验证完整动画，并限制累计解码量。
    for (index, frame) in frames.enumerate() {
        let image = frame?.into_buffer();
        pixels += u64::from(image.width()) * u64::from(image.height());
        ensure!(
            index < MAX_FRAMES && pixels <= MAX_ANIMATION_PIXELS,
            "表情动画过长或尺寸过大"
        );
        if poster
            .as_ref()
            .is_none_or(|first: &RgbaImage| !has_visible_pixels(first))
        {
            poster = Some(image);
        }
    }
    poster.context("表情动画没有可播放的帧")
}

pub(crate) fn has_visible_pixels(image: &RgbaImage) -> bool {
    image
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::EMOTES;

    #[test]
    fn cache_rejects_invalid_data_and_keeps_received_emotes_out_of_the_library() {
        let data = std::env::temp_dir().join(format!("pingpingping-emotes-{}", Uuid::new_v4()));
        let store = EmoteStore::new(&data);
        let bytes = EMOTES[0].image;
        let id = EmoteId::from_bytes(bytes);
        let broken = b"not an image";
        assert!(store.save(id, broken).is_err());
        assert!(store.save(EmoteId::from_bytes(broken), broken).is_err());
        assert!(!store.contains(id));

        store.save(id, bytes).unwrap();
        assert!(store.contains(id));
        assert_eq!(store.read(id).unwrap(), bytes);
        assert!(store.library().unwrap().is_empty());

        let path = data.join("表情.png");
        fs::write(&path, bytes).unwrap();
        assert_eq!(store.import(&path).unwrap().id(), id);
        fs::remove_file(path).unwrap();
        let library = EmoteStore::new(&data).library().unwrap();
        assert_eq!(library.len(), 1);
        assert_eq!(library[0].id(), id);
        assert_eq!(library[0].name(), "表情");
        fs::remove_dir_all(data).unwrap();
    }
}
