//! Direct TCP rooms on local IPv4 endpoints, with one host forwarding room messages.
//! 使用局域网 IPv4 地址直连的 TCP 房间，由一台主机转发房间消息。

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use async_channel::{Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use smol::future::race;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::{TcpListener, TcpStream};
use smol::{Task, Timer};
use socket2::{SockRef, TcpKeepalive};
use uuid::Uuid;

use crate::emotes::{EmoteStore, MAX_EMOTE_BYTES};
use crate::model::{Body, Message, valid_nickname};

pub(crate) const DEFAULT_PORT: u16 = 30086;
const MAX_FRAME: usize = 4096;
const MAX_PEERS: usize = 16;
// The greeting identifies the wire format. It does not authenticate peers.
// 握手标记用于识别协议版本，不验证对端身份。
const LEGACY_HELLO: [u8; 4] = *b"PPP\x03";
const HELLO: [u8; 4] = *b"PPP\x04";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const CACHE_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const MESSAGE: u8 = 0;
const CACHED: u8 = 1;
const REQUEST_IMAGE: u8 = 2;
const IMAGE: u8 = 3;
const NAME: u8 = 4;
const MEMBERS: u8 = 5;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum RoomConfig {
    Host(SocketAddrV4),
    Join(SocketAddrV4),
}

impl RoomConfig {
    pub(crate) fn parse(host: bool, ip: &str, port: &str) -> Result<Self> {
        let ip: Ipv4Addr = ip.trim().parse().context("请输入局域网 IPv4 地址")?;
        let port: u16 = port.trim().parse().context("请输入 1–65535 之间的端口")?;
        ensure!(port != 0, "请输入 1–65535 之间的端口");
        ensure!(local(ip), "请输入私有、链路本地或回环 IPv4 地址");
        let address = SocketAddrV4::new(ip, port);
        Ok(if host {
            Self::Host(address)
        } else {
            Self::Join(address)
        })
    }

    pub(crate) fn address(self) -> SocketAddrV4 {
        match self {
            Self::Host(address) | Self::Join(address) => address,
        }
    }
}

fn local(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

fn local_peer(address: SocketAddr) -> bool {
    matches!(address, SocketAddr::V4(address) if local(*address.ip()))
}

pub(crate) enum RoomEvent {
    Message(Uuid, Message),
    Members(Uuid, Vec<RoomMember>),
    Closed(Uuid, String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RoomMember {
    id: Uuid,
    name: Option<String>,
}

impl RoomMember {
    pub(crate) fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

pub(crate) struct Room {
    id: Uuid,
    config: RoomConfig,
    outbox: Sender<Message>,
    presence: Option<(Uuid, Sender<Presence>)>,
    members: Vec<RoomMember>,
    activate: Option<Sender<()>>,
    _task: Task<()>,
}

impl Room {
    pub(crate) async fn open(
        config: RoomConfig,
        name: String,
        incoming: Sender<RoomEvent>,
        emotes: EmoteStore,
    ) -> Result<Self> {
        let address = config.address();
        ensure!(
            local(*address.ip()) && address.port() != 0,
            "无效的局域网地址或端口"
        );
        ensure!(valid_nickname(&name), "请输入昵称（最多 64 字节）");
        let id = Uuid::new_v4();
        let (outbox, outgoing) = async_channel::bounded(64);
        let (presence, updates) = async_channel::bounded(1);
        // Forward events only after the application has adopted this session.
        // 应用接管此会话后才开始转发事件。
        let (activate, ready) = async_channel::bounded(1);
        let (member_id, task) = match config {
            RoomConfig::Host(address) => {
                let listener = TcpListener::bind(address)
                    .await
                    .context("无法监听该地址。请确认 IP 属于这台电脑，端口未被占用")?;
                let task = smol::spawn(async move {
                    if ready.recv().await.is_err() {
                        return;
                    }
                    let member = RoomMember {
                        id,
                        name: Some(name),
                    };
                    if let Err(error) =
                        host(member, listener, outgoing, updates, &incoming, emotes).await
                    {
                        let _ = incoming
                            .send(RoomEvent::Closed(id, format!("监听已停止：{error}")))
                            .await;
                    }
                });
                (Some(id), task)
            }
            RoomConfig::Join(address) => {
                let (stream, role) = race(join(address, &name), async {
                    Timer::after(CONNECT_TIMEOUT).await;
                    Err(io::Error::new(io::ErrorKind::TimedOut, "连接超时"))
                })
                .await
                .context("无法连接主机。请检查 IP、端口和防火墙")?;
                let member_id = match role {
                    ConnectionRole::Guest(member_id) => Some(member_id),
                    _ => None,
                };
                let task = smol::spawn(async move {
                    if ready.recv().await.is_err() {
                        return;
                    }
                    let report = |packet| async {
                        let event = match packet {
                            Packet::Message(message) => RoomEvent::Message(id, message),
                            Packet::Presence(Presence::Members(members)) => {
                                RoomEvent::Members(id, members)
                            }
                            Packet::Presence(Presence::Name(_)) => return Err(invalid_packet()),
                        };
                        report_event(&incoming, event).await
                    };
                    let reason = connection(stream, outgoing, updates, role, emotes, report)
                        .await
                        .map_or_else(
                            |error| {
                                if error.kind() == io::ErrorKind::UnexpectedEof {
                                    "主机已断开连接".into()
                                } else {
                                    format!("已断开：{error}")
                                }
                            },
                            |()| "已断开连接".into(),
                        );
                    let _ = incoming.send(RoomEvent::Closed(id, reason)).await;
                });
                (member_id, task)
            }
        };
        Ok(Self {
            id,
            config,
            outbox,
            presence: member_id.map(|member_id| (member_id, presence)),
            members: Vec::new(),
            activate: Some(activate),
            _task: task,
        })
    }

    pub(crate) fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn config(&self) -> RoomConfig {
        self.config
    }

    pub(crate) fn members(&self) -> Option<(Uuid, &[RoomMember])> {
        self.presence
            .as_ref()
            .map(|(member_id, _)| (*member_id, self.members.as_slice()))
    }

    pub(crate) fn set_members(&mut self, members: Vec<RoomMember>) -> bool {
        if self.members == members {
            return false;
        }
        self.members = members;
        true
    }

    pub(crate) fn set_name(&self, name: String) {
        if let Some((_, presence)) = &self.presence {
            let _ = presence.force_send(Presence::Name(name));
        }
    }

    pub(crate) fn activate(&mut self) {
        if let Some(activate) = self.activate.take() {
            let _ = activate.try_send(());
        }
    }

    pub(crate) fn send(&self, message: Message) -> Result<()> {
        self.outbox.try_send(message).map_err(|error| match error {
            TrySendError::Full(_) => anyhow::anyhow!("发送队列已满，请稍后重试"),
            TrySendError::Closed(_) => anyhow::anyhow!("连接已断开，请重新连接"),
        })
    }
}

#[derive(Clone, Copy)]
enum ConnectionRole {
    Legacy,
    Host,
    Guest(Uuid),
}

async fn join(address: SocketAddrV4, name: &str) -> io::Result<(TcpStream, ConnectionRole)> {
    let mut stream = TcpStream::connect(address).await?;
    stream.set_nodelay(true)?;
    stream.write_all(&HELLO).await?;
    let mut hello = [0; HELLO.len()];
    match stream.read_exact(&mut hello).await {
        Ok(_) if hello == HELLO => {
            write_frame(&mut stream, NAME, &name).await?;
            let mut id = [0; 16];
            stream.read_exact(&mut id).await?;
            let id = Uuid::from_bytes(id);
            if id.is_nil() {
                return Err(invalid_packet());
            }
            Ok((stream, ConnectionRole::Guest(id)))
        }
        // Version 3 hosts close the first connection when they see a newer greeting.
        // 第 3 版主机收到新版握手标记时会关闭第一次连接。
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            ) =>
        {
            drop(stream);
            let mut stream = TcpStream::connect(address).await?;
            stream.set_nodelay(true)?;
            stream.write_all(&LEGACY_HELLO).await?;
            stream.read_exact(&mut hello).await?;
            if hello != LEGACY_HELLO {
                return Err(protocol_mismatch());
            }
            Ok((stream, ConnectionRole::Legacy))
        }
        Ok(_) => Err(protocol_mismatch()),
        Err(error) => Err(error),
    }
}

async fn accept_member(stream: &mut TcpStream, id: Uuid) -> io::Result<Option<String>> {
    stream.set_nodelay(true)?;
    let mut hello = [0; HELLO.len()];
    stream.read_exact(&mut hello).await?;
    match hello {
        HELLO => {
            stream.write_all(&HELLO).await?;
            let mut tag = [0];
            stream.read_exact(&mut tag).await?;
            if tag[0] != NAME {
                return Err(invalid_packet());
            }
            let name = read_name(stream).await?;
            stream.write_all(id.as_bytes()).await?;
            Ok(Some(name))
        }
        LEGACY_HELLO => {
            stream.write_all(&LEGACY_HELLO).await?;
            Ok(None)
        }
        _ => Err(protocol_mismatch()),
    }
}

fn protocol_mismatch() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "房间协议版本不匹配")
}

// A type byte precedes each packet. Retain bounded JSON messages and transfer image bytes separately.
// 每个数据包以类型字节开头。消息仍使用长度受限的 JSON，图片字节单独传输。
async fn write_frame(stream: &mut TcpStream, tag: u8, value: &impl Serialize) -> io::Result<()> {
    let mut frame = vec![tag, 0, 0];
    serde_json::to_writer(&mut frame, value)?;
    let length = frame.len() - 3;
    if length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "消息过长"));
    }
    frame[1..3].copy_from_slice(&(length as u16).to_be_bytes());
    stream.write_all(&frame).await
}

async fn read_frame<T: DeserializeOwned>(stream: &mut TcpStream) -> io::Result<T> {
    let mut length = [0; 2];
    stream.read_exact(&mut length).await?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "无效的消息长度"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn read_message(stream: &mut TcpStream) -> io::Result<Message> {
    let message: Message = read_frame(stream).await?;
    if !message.valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "无效的房间消息"));
    }
    Ok(message)
}

async fn read_name(stream: &mut TcpStream) -> io::Result<String> {
    let name: String = read_frame(stream).await?;
    if !valid_nickname(&name) {
        return Err(invalid_packet());
    }
    Ok(name)
}

async fn read_members(stream: &mut TcpStream, local_id: Uuid) -> io::Result<Vec<RoomMember>> {
    let members: Vec<RoomMember> = read_frame(stream).await?;
    let mut ids = HashSet::new();
    // The host is first, and an updated guest must be listed with its own nickname.
    // 主机位于名单首位，新版访客也必须有自己的昵称条目。
    if members.len() > MAX_PEERS + 1
        || !matches!(members.first(), Some(host) if host.id != local_id && host.name.is_some())
        || !members
            .iter()
            .any(|member| member.id == local_id && member.name.is_some())
        || !members.iter().all(|member| {
            !member.id.is_nil() && ids.insert(member.id) && member.name().is_none_or(valid_nickname)
        })
    {
        return Err(invalid_packet());
    }
    Ok(members)
}

enum Presence {
    Name(String),
    Members(Vec<RoomMember>),
}

enum Packet {
    Message(Message),
    Presence(Presence),
}

async fn report_event(incoming: &Sender<RoomEvent>, event: RoomEvent) -> io::Result<()> {
    incoming
        .send(event)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
}

async fn connection<F, Fut>(
    mut reader: TcpStream,
    outgoing: Receiver<Message>,
    updates: Receiver<Presence>,
    role: ConnectionRole,
    emotes: EmoteStore,
    mut accept: F,
) -> io::Result<()>
where
    F: FnMut(Packet) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    // Detect silent network loss even when nobody in the room is sending messages.
    // 即使房间里没人发消息，也能检测网络中断。
    SockRef::from(&reader).set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(5)),
    )?;
    let mut writer = reader.clone();
    let (reply, replies) = async_channel::bounded(1);
    let (status, statuses) = async_channel::bounded(1);
    let awaiting_cache_reply = AtomicBool::new(false);
    race(
        async {
            let mut pending = None;
            loop {
                let mut tag = [0];
                reader.read_exact(&mut tag).await?;
                match tag[0] {
                    MESSAGE if pending.is_none() => {
                        let message = read_message(&mut reader).await?;
                        if let Body::CustomEmote(id) = message.body {
                            let emotes = emotes.clone();
                            let cached = smol::unblock(move || emotes.contains(id)).await;
                            reply
                                .send(if cached { CACHED } else { REQUEST_IMAGE })
                                .await
                                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                            if !cached {
                                pending = Some((id, message));
                                continue;
                            }
                        }
                        accept(Packet::Message(message)).await?;
                    }
                    IMAGE => {
                        let Some((id, message)) = pending.take() else {
                            return Err(invalid_packet());
                        };
                        let mut length = [0; 4];
                        reader.read_exact(&mut length).await?;
                        let length = u32::from_be_bytes(length) as usize;
                        if length == 0 || length > MAX_EMOTE_BYTES {
                            return Err(invalid_packet());
                        }
                        let mut bytes = vec![0; length];
                        reader.read_exact(&mut bytes).await?;
                        let emotes = emotes.clone();
                        smol::unblock(move || emotes.save(id, &bytes))
                            .await
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        accept(Packet::Message(message)).await?;
                    }
                    NAME if pending.is_none() && matches!(role, ConnectionRole::Host) => {
                        let name = read_name(&mut reader).await?;
                        accept(Packet::Presence(Presence::Name(name))).await?;
                    }
                    MEMBERS if pending.is_none() => {
                        let ConnectionRole::Guest(id) = role else {
                            return Err(invalid_packet());
                        };
                        let members = read_members(&mut reader, id).await?;
                        accept(Packet::Presence(Presence::Members(members))).await?;
                    }
                    CACHED | REQUEST_IMAGE
                        if awaiting_cache_reply.swap(false, Ordering::Relaxed) =>
                    {
                        status.try_send(tag[0]).map_err(|_| invalid_packet())?;
                    }
                    _ => return Err(invalid_packet()),
                }
            }
        },
        async {
            loop {
                let message = race(
                    async { replies.recv().await.map(WriteEvent::Reply) },
                    race(
                        async { outgoing.recv().await.map(WriteEvent::Message) },
                        async {
                            if matches!(role, ConnectionRole::Legacy) {
                                std::future::pending().await
                            } else {
                                updates.recv().await.map(WriteEvent::Presence)
                            }
                        },
                    ),
                )
                .await;
                match message {
                    Ok(WriteEvent::Reply(tag)) => writer.write_all(&[tag]).await?,
                    Ok(WriteEvent::Presence(Presence::Name(name))) => {
                        write_frame(&mut writer, NAME, &name).await?;
                    }
                    Ok(WriteEvent::Presence(Presence::Members(members))) => {
                        write_frame(&mut writer, MEMBERS, &members).await?;
                    }
                    Ok(WriteEvent::Message(message)) => {
                        let custom = match message.body {
                            Body::CustomEmote(id) => Some(id),
                            _ => None,
                        };
                        awaiting_cache_reply.store(custom.is_some(), Ordering::Relaxed);
                        write_frame(&mut writer, MESSAGE, &message).await?;
                        if let Some(id) = custom
                            && wait_for_cache_reply(&mut writer, &replies, &statuses).await?
                                == REQUEST_IMAGE
                        {
                            let emotes = emotes.clone();
                            let bytes = smol::unblock(move || emotes.read(id))
                                .await
                                .map_err(io::Error::other)?;
                            writer.write_all(&[IMAGE]).await?;
                            writer
                                .write_all(&(bytes.len() as u32).to_be_bytes())
                                .await?;
                            writer.write_all(&bytes).await?;
                        }
                    }
                    Err(_) => return Ok(()),
                }
            }
        },
    )
    .await
}

fn invalid_packet() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "无效的房间数据包")
}

enum WriteEvent {
    Reply(u8),
    Message(Message),
    Presence(Presence),
}

// Process both directions while waiting for a cache reply so simultaneous sends can make progress.
// 等待缓存回复时仍处理对端的查询，让双方同时发送表情时也能继续传输。
async fn wait_for_cache_reply(
    writer: &mut TcpStream,
    replies: &Receiver<u8>,
    statuses: &Receiver<u8>,
) -> io::Result<u8> {
    race(
        async {
            loop {
                match race(async { replies.recv().await.map(Ok) }, async {
                    statuses.recv().await.map(Err)
                })
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?
                {
                    Ok(tag) => writer.write_all(&[tag]).await?,
                    Err(status) => return Ok(status),
                }
            }
        },
        async {
            Timer::after(CACHE_REPLY_TIMEOUT).await;
            Err(io::Error::new(io::ErrorKind::TimedOut, "表情下载请求超时"))
        },
    )
    .await
}

enum HostEvent {
    Joined(Uuid, Option<String>),
    Incoming(Uuid, Packet),
    Disconnected(Uuid),
}

enum HostWake {
    Accepted(TcpStream, SocketAddr),
    Peer(HostEvent),
    Local(Message),
    Presence(Presence),
    Stop,
}

struct Peer {
    outgoing: Sender<Message>,
    presence: Sender<Presence>,
    member: Option<RoomMember>,
    has_members: bool,
    _task: Task<()>,
}

async fn host(
    mut host: RoomMember,
    listener: TcpListener,
    local_messages: Receiver<Message>,
    local_presence: Receiver<Presence>,
    incoming: &Sender<RoomEvent>,
    emotes: EmoteStore,
) -> io::Result<()> {
    let (send_event, receive_event) = async_channel::bounded(64);
    let mut peers: HashMap<Uuid, Peer> = HashMap::new();
    publish_members(&host, &peers, incoming).await?;
    loop {
        let wake = race(
            async {
                listener
                    .accept()
                    .await
                    .map(|(stream, address)| HostWake::Accepted(stream, address))
            },
            race(
                async {
                    Ok(receive_event
                        .recv()
                        .await
                        .map_or(HostWake::Stop, HostWake::Peer))
                },
                race(
                    async {
                        Ok(local_messages
                            .recv()
                            .await
                            .map_or(HostWake::Stop, HostWake::Local))
                    },
                    async {
                        Ok(local_presence
                            .recv()
                            .await
                            .map_or(HostWake::Stop, HostWake::Presence))
                    },
                ),
            ),
        )
        .await?;
        let members_changed = match wake {
            HostWake::Accepted(mut stream, address)
                if local_peer(address) && peers.len() < MAX_PEERS =>
            {
                let peer = Uuid::new_v4();
                let (outgoing, messages) = async_channel::bounded(32);
                let (presence, updates) = async_channel::bounded(1);
                let events = send_event.clone();
                let emotes = emotes.clone();
                let task = smol::spawn(async move {
                    let ready = race(accept_member(&mut stream, peer), async {
                        Timer::after(CONNECT_TIMEOUT).await;
                        Err(io::Error::from(io::ErrorKind::TimedOut))
                    })
                    .await;
                    if let Ok(name) = ready {
                        let role = if name.is_some() {
                            ConnectionRole::Host
                        } else {
                            ConnectionRole::Legacy
                        };
                        if events.send(HostEvent::Joined(peer, name)).await.is_ok() {
                            let _ = connection(
                                stream,
                                messages,
                                updates,
                                role,
                                emotes,
                                |packet| async {
                                    events
                                        .send(HostEvent::Incoming(peer, packet))
                                        .await
                                        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
                                },
                            )
                            .await;
                        }
                    }
                    let _ = events.send(HostEvent::Disconnected(peer)).await;
                });
                peers.insert(
                    peer,
                    Peer {
                        outgoing,
                        presence,
                        member: None,
                        has_members: false,
                        _task: task,
                    },
                );
                false
            }
            HostWake::Accepted(..) => false,
            HostWake::Peer(HostEvent::Joined(id, name)) => {
                let Some(peer) = peers.get_mut(&id) else {
                    continue;
                };
                peer.has_members = name.is_some();
                peer.member = Some(RoomMember { id, name });
                true
            }
            HostWake::Peer(HostEvent::Disconnected(peer)) => peers
                .remove(&peer)
                .is_some_and(|peer| peer.member.is_some()),
            HostWake::Peer(HostEvent::Incoming(id, Packet::Message(message))) => {
                let Some(peer) = peers.get_mut(&id) else {
                    continue;
                };
                // Older guests first reveal their nickname when they send a message.
                // 旧版访客首次发送消息时才会公开昵称。
                let renamed = !peer.has_members
                    && peer
                        .member
                        .as_mut()
                        .is_some_and(|member| update_name(member, &message.name));
                let removed = broadcast(&mut peers, Some(id), message.clone());
                report_event(incoming, RoomEvent::Message(host.id, message)).await?;
                renamed || removed
            }
            HostWake::Peer(HostEvent::Incoming(id, Packet::Presence(Presence::Name(name)))) => {
                peers
                    .get_mut(&id)
                    .and_then(|peer| peer.member.as_mut())
                    .is_some_and(|member| update_name(member, &name))
            }
            HostWake::Presence(Presence::Name(name)) => update_name(&mut host, &name),
            HostWake::Presence(Presence::Members(_))
            | HostWake::Peer(HostEvent::Incoming(_, Packet::Presence(Presence::Members(_)))) => {
                return Err(invalid_packet());
            }
            HostWake::Local(message) => broadcast(&mut peers, None, message),
            HostWake::Stop => return Ok(()),
        };
        if members_changed {
            publish_members(&host, &peers, incoming).await?;
        }
    }
}

fn update_name(member: &mut RoomMember, name: &str) -> bool {
    if member.name() == Some(name) {
        return false;
    }
    member.name = Some(name.to_owned());
    true
}

async fn publish_members(
    host: &RoomMember,
    peers: &HashMap<Uuid, Peer>,
    incoming: &Sender<RoomEvent>,
) -> io::Result<()> {
    let mut members = vec![host.clone()];
    members.extend(peers.values().filter_map(|peer| peer.member.clone()));
    members[1..].sort_unstable_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    // Replace queued snapshots so membership changes cannot fill the chat queue.
    // 替换队列中的旧名单，避免成员变动占满聊天队列。
    for peer in peers.values().filter(|peer| peer.has_members) {
        let _ = peer.presence.force_send(Presence::Members(members.clone()));
    }
    report_event(incoming, RoomEvent::Members(host.id, members)).await
}

// Drop peers whose queues are full so a slow reader cannot block the rest of the room.
// 断开发送队列已满的对端，避免一个读取缓慢的客户端拖住整个房间。
fn broadcast(peers: &mut HashMap<Uuid, Peer>, origin: Option<Uuid>, message: Message) -> bool {
    let count = peers.len();
    peers.retain(|id, peer| Some(*id) == origin || peer.outgoing.try_send(message.clone()).is_ok());
    peers.len() != count
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::assets::{EMOTES, PINGS};
    use crate::model::TextEffect;

    async fn expect_members(inbox: &Receiver<RoomEvent>, room: &Room, names: &[&str]) {
        let RoomEvent::Members(id, members) = inbox.recv().await.unwrap() else {
            panic!("expected a room member list");
        };
        assert_eq!(id, room.id());
        let (local_id, _) = room.members().unwrap();
        assert!(members.iter().any(|member| member.id() == local_id));
        assert_eq!(
            members
                .iter()
                .map(|member| member.name())
                .collect::<Vec<_>>(),
            names.iter().copied().map(Some).collect::<Vec<_>>()
        );
    }

    fn loopback_address(address: SocketAddr) -> SocketAddrV4 {
        let SocketAddr::V4(address) = address else {
            panic!("expected an IPv4 address");
        };
        address
    }

    #[test]
    fn endpoints_require_a_local_ipv4_and_nonzero_port() {
        assert!(RoomConfig::parse(false, "192.168.1.20", "49378").is_ok());
        for ip in ["8.8.8.8", "0.0.0.0", "224.0.0.1", "example.com", "::1"] {
            assert!(RoomConfig::parse(false, ip, "49378").is_err());
        }
        for port in ["0", "65536", "abc"] {
            assert!(RoomConfig::parse(true, "127.0.0.1", port).is_err());
        }
    }

    #[test]
    #[ignore = "requires TCP loopback access"]
    fn legacy_guests_appear_in_the_list_and_keep_exchanging_messages() {
        smol::block_on(race(
            async {
                let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let address = loopback_address(reservation.local_addr().unwrap());
                drop(reservation);
                let (events, inbox) = async_channel::bounded(8);
                let mut host = Room::open(
                    RoomConfig::Host(address),
                    "主机".into(),
                    events,
                    EmoteStore::new(&std::env::temp_dir()),
                )
                .await
                .unwrap();
                host.activate();
                expect_members(&inbox, &host, &["主机"]).await;

                let mut guest = TcpStream::connect(address).await.unwrap();
                guest.write_all(&LEGACY_HELLO).await.unwrap();
                let mut hello = [0; 4];
                guest.read_exact(&mut hello).await.unwrap();
                assert_eq!(hello, LEGACY_HELLO);
                let RoomEvent::Members(_, members) = inbox.recv().await.unwrap() else {
                    panic!("expected a room member list");
                };
                assert_eq!(members.len(), 2);
                assert_eq!(members[1].name(), None);

                let text = Message::new(Uuid::new_v4(), "旧版".into(), Body::Text("你好".into()));
                write_frame(&mut guest, MESSAGE, &text).await.unwrap();
                assert!(
                    matches!(inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id)
                );
                expect_members(&inbox, &host, &["主机", "旧版"]).await;

                host.send(text.clone()).unwrap();
                let mut tag = [0];
                guest.read_exact(&mut tag).await.unwrap();
                assert_eq!(tag[0], MESSAGE);
                assert_eq!(read_message(&mut guest).await.unwrap().id, text.id);
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                panic!("legacy guest could not exchange messages");
            },
        ));
    }

    #[test]
    #[ignore = "requires TCP loopback access"]
    fn updated_guests_fall_back_to_a_legacy_host() {
        smol::block_on(race(
            async {
                let host = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                let address = loopback_address(host.local_addr().unwrap());
                let serve = smol::spawn(async move {
                    let mut hello = [0; 4];
                    let (mut first, _) = host.accept().await.unwrap();
                    first.read_exact(&mut hello).await.unwrap();
                    assert_eq!(hello, HELLO);
                    drop(first);
                    let (mut guest, _) = host.accept().await.unwrap();
                    guest.read_exact(&mut hello).await.unwrap();
                    assert_eq!(hello, LEGACY_HELLO);
                    guest.write_all(&LEGACY_HELLO).await.unwrap();
                    let mut tag = [0];
                    guest.read_exact(&mut tag).await.unwrap();
                    assert_eq!(tag[0], MESSAGE);
                    let text = read_message(&mut guest).await.unwrap();
                    write_frame(&mut guest, MESSAGE, &text).await.unwrap();
                });
                let (events, inbox) = async_channel::bounded(8);
                let mut guest = Room::open(
                    RoomConfig::Join(address),
                    "访客".into(),
                    events,
                    EmoteStore::new(&std::env::temp_dir()),
                )
                .await
                .unwrap();
                guest.activate();
                assert!(guest.members().is_none());
                guest.set_name("新昵称".into());
                let text = Message::new(Uuid::new_v4(), "新昵称".into(), Body::Text("你好".into()));
                guest.send(text.clone()).unwrap();
                assert!(
                    matches!(inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id)
                );
                serve.await;
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                panic!("legacy host could not exchange messages");
            },
        ));
    }

    #[test]
    #[ignore = "requires TCP loopback access"]
    fn clients_join_by_endpoint_exchange_messages_and_notice_disconnect() {
        smol::block_on(race(
            async {
                let directory =
                    std::env::temp_dir().join(format!("pingpingping-room-{}", Uuid::new_v4()));
                fs::create_dir_all(&directory).unwrap();
                let host_data = directory.join("host");
                let first_data = directory.join("first");
                let host_store = EmoteStore::new(&host_data);
                let first_store = EmoteStore::new(&first_data);
                let second_store = EmoteStore::new(&directory.join("second"));
                let local_image = directory.join("host.png");
                let guest_image = directory.join("guest.png");
                fs::write(&local_image, PINGS[0].image).unwrap();
                fs::write(&guest_image, EMOTES[0].image).unwrap();
                let local_id = host_store.import(&local_image).unwrap().id();
                let guest_id = first_store.import(&guest_image).unwrap().id();

                let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let address = loopback_address(reservation.local_addr().unwrap());
                drop(reservation);
                let (host_events, host_inbox) = async_channel::bounded(8);
                let mut host = Room::open(
                    RoomConfig::Host(address),
                    "主机".into(),
                    host_events,
                    host_store.clone(),
                )
                .await
                .unwrap();
                host.activate();
                expect_members(&host_inbox, &host, &["主机"]).await;
                let (first_events, first_inbox) = async_channel::bounded(8);
                let mut first = Room::open(
                    RoomConfig::Join(address),
                    "访客 A".into(),
                    first_events,
                    first_store.clone(),
                )
                .await
                .unwrap();
                first.activate();
                expect_members(&host_inbox, &host, &["主机", "访客 A"]).await;
                expect_members(&first_inbox, &first, &["主机", "访客 A"]).await;
                let (second_events, second_inbox) = async_channel::bounded(8);
                let mut second = Room::open(
                    RoomConfig::Join(address),
                    "访客 B".into(),
                    second_events,
                    second_store.clone(),
                )
                .await
                .unwrap();
                second.activate();
                let names = ["主机", "访客 A", "访客 B"];
                expect_members(&host_inbox, &host, &names).await;
                expect_members(&first_inbox, &first, &names).await;
                expect_members(&second_inbox, &second, &names).await;
                first.set_name("访客 C".into());
                let names = ["主机", "访客 B", "访客 C"];
                expect_members(&host_inbox, &host, &names).await;
                expect_members(&first_inbox, &first, &names).await;
                expect_members(&second_inbox, &second, &names).await;
                let mut text = Message::new(Uuid::new_v4(), "甲".into(), Body::Text("你好".into()));
                text.text_effect = TextEffect::Shake;
                first.send(text.clone()).unwrap();
                assert!(
                    matches!(host_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id && received.text_effect == TextEffect::Shake)
                );
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id && received.text_effect == TextEffect::Shake)
                );
                let emote = Message::new(
                    Uuid::new_v4(),
                    "乙".into(),
                    Body::Emote {
                        index: 0,
                        location: None,
                    },
                );
                host.send(emote.clone()).unwrap();
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == emote.id)
                );

                let local_emote =
                    Message::new(Uuid::new_v4(), "主机".into(), Body::CustomEmote(local_id));
                let guest_emote =
                    Message::new(Uuid::new_v4(), "访客".into(), Body::CustomEmote(guest_id));
                host.send(local_emote.clone()).unwrap();
                first.send(guest_emote.clone()).unwrap();
                assert!(
                    matches!(first_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == emote.id)
                );
                assert!(
                    matches!(first_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == local_emote.id)
                );
                assert!(
                    matches!(host_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == guest_emote.id)
                );
                let mut forwarded = Vec::new();
                for _ in 0..2 {
                    let RoomEvent::Message(_, message) = second_inbox.recv().await.unwrap() else {
                        panic!("expected a forwarded custom emote");
                    };
                    forwarded.push(message.id);
                }
                assert!(forwarded.contains(&local_emote.id) && forwarded.contains(&guest_emote.id));
                assert_eq!(host_store.read(guest_id).unwrap(), EMOTES[0].image);
                assert_eq!(first_store.read(local_id).unwrap(), PINGS[0].image);
                assert_eq!(second_store.read(guest_id).unwrap(), EMOTES[0].image);
                assert!(second_store.library().unwrap().is_empty());

                // With no original left on the sender, delivery proves the receiver did not request bytes.
                // 删除发送端的原图后仍能送达，说明接收端命中缓存并跳过了下载。
                fs::remove_dir_all(first_data).unwrap();
                let cached =
                    Message::new(Uuid::new_v4(), "访客".into(), Body::CustomEmote(guest_id));
                first.send(cached.clone()).unwrap();
                assert!(
                    matches!(host_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == cached.id)
                );
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == cached.id)
                );
                drop(first);
                expect_members(&host_inbox, &host, &["主机", "访客 B"]).await;
                expect_members(&second_inbox, &second, &["主机", "访客 B"]).await;
                fs::remove_dir_all(host_data).unwrap();
                let cached =
                    Message::new(Uuid::new_v4(), "主机".into(), Body::CustomEmote(local_id));
                host.send(cached.clone()).unwrap();
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == cached.id)
                );
                drop(host);
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Closed(id, _) if id == second.id())
                );
                fs::remove_dir_all(directory).unwrap();
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                panic!("TCP room did not complete");
            },
        ));
    }
}
