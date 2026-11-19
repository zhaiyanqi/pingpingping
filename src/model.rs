//! Application state, endpoint preferences, and local conversation history.
//! 应用状态、连接地址设置和本地聊天历史。

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::assets::{Clip, EMOTES, PINGS};
use crate::room::{Room, RoomConfig};

const HISTORY_LIMIT: usize = 600;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct Location {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Body {
    Text(String),
    Emote {
        index: usize,
        location: Option<Location>,
    },
    Ping {
        index: usize,
        location: Location,
    },
}

impl Body {
    pub(crate) fn clip(&self) -> Option<&'static Clip> {
        match *self {
            Self::Emote { index, .. } => EMOTES.get(index),
            Self::Ping { index, .. } => PINGS.get(index),
            Self::Text(_) => None,
        }
    }

    pub(crate) fn location(&self) -> Option<Location> {
        match *self {
            Self::Emote { location, .. } => location,
            Self::Ping { location, .. } => Some(location),
            Self::Text(_) => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Message {
    pub(crate) id: Uuid,
    pub(crate) sender: Uuid,
    pub(crate) name: String,
    pub(crate) time: u64,
    pub(crate) body: Body,
}

impl Message {
    pub(crate) fn new(sender: Uuid, name: String, body: Body) -> Self {
        Self {
            id: Uuid::new_v4(),
            sender,
            name,
            time: now(),
            body,
        }
    }

    pub(crate) fn valid(&self) -> bool {
        i64::try_from(self.time).is_ok()
            && !self.name.is_empty()
            && self.name.len() <= 64
            && self.name.chars().all(|character| !character.is_control())
            && match &self.body {
                Body::Text(text) => !text.trim().is_empty() && text.len() <= 600,
                _ => {
                    self.body.clip().is_some()
                        && self.body.location().is_none_or(|location| {
                            (0.0..=1.0).contains(&location.x) && (0.0..=1.0).contains(&location.y)
                        })
                }
            }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) const PING_SIZE: RangeInclusive<u32> = 60..=220;
pub(crate) const EMOTE_SIZE: RangeInclusive<u32> = 80..=300;
pub(crate) const ANIMATION_TIMEOUT: RangeInclusive<u32> = 1000..=8000;
pub(crate) const SOUND_VOLUME: RangeInclusive<u32> = 0..=100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub(crate) struct OverlaySettings {
    pub(crate) ping_size: u32,
    pub(crate) emote_size: u32,
    pub(crate) ping_timeout_ms: u32,
    pub(crate) emote_timeout_ms: u32,
    pub(crate) volume: u32,
    pub(crate) emote_sound: bool,
    pub(crate) wheel_sound: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            ping_size: 110,
            emote_size: 150,
            ping_timeout_ms: 3200,
            emote_timeout_ms: 3000,
            volume: 70,
            emote_sound: true,
            wheel_sound: true,
        }
    }
}

impl OverlaySettings {
    fn normalize(&mut self) {
        self.ping_size = self.ping_size.clamp(*PING_SIZE.start(), *PING_SIZE.end());
        self.emote_size = self
            .emote_size
            .clamp(*EMOTE_SIZE.start(), *EMOTE_SIZE.end());
        self.ping_timeout_ms = self
            .ping_timeout_ms
            .clamp(*ANIMATION_TIMEOUT.start(), *ANIMATION_TIMEOUT.end());
        self.emote_timeout_ms = self
            .emote_timeout_ms
            .clamp(*ANIMATION_TIMEOUT.start(), *ANIMATION_TIMEOUT.end());
        self.volume = self
            .volume
            .clamp(*SOUND_VOLUME.start(), *SOUND_VOLUME.end());
    }

    pub(crate) fn with_sound(mut self, enabled: bool) -> Self {
        if !enabled {
            self.volume = 0;
        }
        self
    }

    pub(crate) fn volume(self) -> Option<f32> {
        (self.volume > 0).then_some(self.volume as f32 / 100.0)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Profile {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) endpoint: Option<RoomConfig>,
    pub(crate) sound: bool,
    pub(crate) gestures: bool,
    pub(crate) remote_overlay: bool,
    #[serde(default)]
    pub(crate) overlay: OverlaySettings,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "桌面好友".into(),
            endpoint: None,
            sound: true,
            gestures: true,
            remote_overlay: true,
            overlay: OverlaySettings::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    room_key: String,
    message: Message,
}

pub(crate) struct Model {
    pub(crate) profile: Profile,
    pub(crate) room_error: Option<String>,
    pub(crate) input_ready: bool,
    data_dir: PathBuf,
    room: Option<Room>,
    room_key: String,
    history: VecDeque<Entry>,
    disk: Option<mpsc::Sender<Entry>>,
    disk_thread: Option<std::thread::JoinHandle<()>>,
}

fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

// Replace a complete snapshot after flushing a temporary file in the same directory.
// 先写入并刷盘同目录下的临时文件，再用完整快照替换旧文件。
fn save_json(path: &Path, data: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, data)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn append(history: &mut VecDeque<Entry>, entry: Entry) {
    while history.len() >= HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(entry);
}

impl Model {
    pub(crate) fn load(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir).context("无法创建应用数据目录")?;
        let profile_path = data_dir.join("settings.json");
        let history_path = data_dir.join("history.json");
        let mut profile: Profile = match load_json(&profile_path).context("无法读取设置")? {
            Some(profile) => profile,
            None => {
                let profile = Profile::default();
                save_json(&profile_path, &profile).context("无法保存初始设置")?;
                profile
            }
        };
        profile.overlay.normalize();
        let mut history: VecDeque<Entry> = load_json(&history_path)
            .context("无法读取历史记录")?
            .unwrap_or_default();
        history.retain(|entry| {
            matches!(&entry.message.body, Body::Text(_)) || entry.message.body.clip().is_some()
        });
        while history.len() > HISTORY_LIMIT {
            history.pop_front();
        }
        let mut snapshot = history.clone();
        // Batch pending history writes on a worker to keep disk I/O out of gesture handling.
        // 由后台线程批量写入历史，避免磁盘 I/O 阻塞手势处理。
        let (disk, writes) = mpsc::channel();
        let disk_thread = std::thread::Builder::new()
            .name("history-storage".into())
            .spawn(move || {
                while let Ok(entry) = writes.recv() {
                    append(&mut snapshot, entry);
                    for pending in writes.try_iter() {
                        append(&mut snapshot, pending);
                    }
                    if let Err(error) = save_json(&history_path, &snapshot) {
                        eprintln!("无法保存聊天记录：{error:#}");
                    }
                }
            })?;
        Ok(Self {
            data_dir,
            room: None,
            room_key: String::new(),
            room_error: None,
            input_ready: false,
            profile,
            history,
            disk: Some(disk),
            disk_thread: Some(disk_thread),
        })
    }

    pub(crate) fn messages(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.history
            .iter()
            .filter(|entry| entry.room_key == self.room_key)
            .map(|entry| &entry.message)
    }

    pub(crate) fn joined(&self) -> bool {
        self.room.is_some()
    }

    pub(crate) fn connection(&self) -> Option<RoomConfig> {
        self.room.as_ref().map(Room::config)
    }

    pub(crate) fn room_key(&self) -> &str {
        &self.room_key
    }

    pub(crate) fn archive(&self) -> impl Iterator<Item = (&str, &Message)> {
        self.history
            .iter()
            .map(|entry| (entry.room_key.as_str(), &entry.message))
    }

    pub(crate) fn entry(&self, index: usize) -> Option<(&str, &Message)> {
        self.history
            .get(index)
            .map(|entry| (entry.room_key.as_str(), &entry.message))
    }

    pub(crate) fn finish_writes(&mut self) -> Option<std::thread::JoinHandle<()>> {
        self.disk.take();
        self.disk_thread.take()
    }

    pub(crate) fn apply(&mut self, mut profile: Profile) -> Result<()> {
        ensure!(
            !profile.name.is_empty()
                && profile.name.len() <= 64
                && profile
                    .name
                    .chars()
                    .all(|character| !character.is_control()),
            "请输入昵称（最多 64 字节）"
        );
        profile.overlay.normalize();
        save_json(&self.data_dir.join("settings.json"), &profile).context("无法保存设置")?;
        self.profile = profile;
        Ok(())
    }

    pub(crate) fn connect(&mut self, mut room: Room) -> Result<()> {
        let mut profile = self.profile.clone();
        let config = room.config();
        profile.endpoint = Some(config);
        self.apply(profile)?;
        self.room_key = config.address().to_string();
        self.room_error = None;
        room.activate();
        self.room = Some(room);
        Ok(())
    }

    pub(crate) fn disconnect(&mut self) {
        self.room = None;
        self.room_key.clear();
        self.room_error = None;
    }

    pub(crate) fn closed(&mut self, id: Uuid, reason: String) -> bool {
        if self.room.as_ref().map(Room::id) != Some(id) {
            return false;
        }
        self.disconnect();
        self.room_error = Some(reason);
        true
    }

    pub(crate) fn send(&mut self, body: Body) -> Result<()> {
        let message = Message::new(self.profile.id, self.profile.name.clone(), body);
        ensure!(message.valid(), "请发送非空消息，最多 600 字节");
        if let Some(room) = &self.room {
            room.send(message.clone()).context("未能发送到局域网")?;
        } else {
            ensure!(
                !matches!(message.body, Body::Text(_)),
                "请先创建房间或连接主机"
            );
        }
        self.record(message);
        Ok(())
    }

    // Events from a closed session may still be queued. Only accept the current session's events.
    // 已关闭会话的事件可能仍在队列里，只接收当前会话的事件。
    pub(crate) fn receive(&mut self, id: Uuid, message: Message) -> bool {
        if self.room.as_ref().map(Room::id) != Some(id)
            || message.sender == self.profile.id
            || self.messages().any(|entry| entry.id == message.id)
        {
            return false;
        }
        self.record(message);
        true
    }

    fn record(&mut self, message: Message) {
        let entry = Entry {
            room_key: self.room_key.clone(),
            message,
        };
        if let Some(disk) = &self.disk {
            let _ = disk.send(entry.clone());
        }
        append(&mut self.history, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_profiles_use_defaults_for_overlay_settings() {
        let profile = Profile {
            sound: false,
            ..Profile::default()
        };
        let mut legacy = serde_json::to_value(&profile).unwrap();
        legacy.as_object_mut().unwrap().remove("overlay");
        let loaded: Profile = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.overlay, OverlaySettings::default());
        assert!(loaded.overlay.with_sound(loaded.sound).volume().is_none());
    }

    #[test]
    fn emote_history_and_identity_survive_reopening() {
        let folder = std::env::temp_dir().join(format!("pingpingping-history-{}", Uuid::new_v4()));
        let mut original = Model::load(folder.clone()).unwrap();
        original
            .send(Body::Emote {
                index: 3,
                location: None,
            })
            .unwrap();
        let message = original.messages().next().unwrap().clone();
        original.finish_writes().unwrap().join().unwrap();
        let mut restored = Model::load(folder.clone()).unwrap();
        assert_eq!(restored.profile.id, message.sender);
        assert_eq!(restored.messages().next().unwrap().id, message.id);
        restored.finish_writes().unwrap().join().unwrap();
        fs::remove_dir_all(folder).unwrap();
    }
}
