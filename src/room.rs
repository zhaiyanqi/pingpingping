//! Direct TCP rooms on local IPv4 endpoints, with one host forwarding room messages.
//! 使用局域网 IPv4 地址直连的 TCP 房间，由一台主机转发房间消息。

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use async_channel::{Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use smol::future::race;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::{TcpListener, TcpStream};
use smol::{Task, Timer};
use uuid::Uuid;

use crate::model::Message;

pub(crate) const DEFAULT_PORT: u16 = 49378;
const MAX_FRAME: usize = 4096;
const MAX_PEERS: usize = 16;
// The greeting identifies the wire format. It does not authenticate peers.
// 握手标记用于识别协议版本，不验证对端身份。
const HELLO: &[u8; 4] = b"PPP\x02";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

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
    Closed(Uuid, String),
}

pub(crate) struct Room {
    id: Uuid,
    config: RoomConfig,
    outbox: Sender<Message>,
    activate: Option<Sender<()>>,
    _task: Task<()>,
}

impl Room {
    pub(crate) async fn open(config: RoomConfig, incoming: Sender<RoomEvent>) -> Result<Self> {
        let address = config.address();
        ensure!(
            local(*address.ip()) && address.port() != 0,
            "无效的局域网地址或端口"
        );
        let id = Uuid::new_v4();
        let (outbox, outgoing) = async_channel::bounded(64);
        // Forward events only after the application has adopted this session.
        // 应用接管此会话后才开始转发事件。
        let (activate, ready) = async_channel::bounded(1);
        let task = match config {
            RoomConfig::Host(address) => {
                let listener = TcpListener::bind(address)
                    .await
                    .context("无法监听该地址。请确认 IP 属于这台电脑，端口未被占用")?;
                smol::spawn(async move {
                    if ready.recv().await.is_err() {
                        return;
                    }
                    if let Err(error) = host(id, listener, outgoing, &incoming).await {
                        let _ = incoming
                            .send(RoomEvent::Closed(id, format!("监听已停止：{error}")))
                            .await;
                    }
                })
            }
            RoomConfig::Join(address) => {
                let stream = race(
                    async {
                        let mut stream = TcpStream::connect(address).await?;
                        stream.set_nodelay(true)?;
                        handshake(&mut stream, false).await?;
                        Ok(stream)
                    },
                    async {
                        Timer::after(CONNECT_TIMEOUT).await;
                        Err(io::Error::new(io::ErrorKind::TimedOut, "连接超时"))
                    },
                )
                .await
                .context("无法连接主机。请检查 IP、端口和防火墙")?;
                smol::spawn(async move {
                    if ready.recv().await.is_err() {
                        return;
                    }
                    let report = |message| async {
                        incoming
                            .send(RoomEvent::Message(id, message))
                            .await
                            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
                    };
                    let reason = connection(stream, outgoing, report).await.map_or_else(
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
                })
            }
        };
        Ok(Self {
            id,
            config,
            outbox,
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

async fn handshake(stream: &mut TcpStream, server: bool) -> io::Result<()> {
    let mut hello = [0; HELLO.len()];
    if !server {
        stream.write_all(HELLO).await?;
    }
    stream.read_exact(&mut hello).await?;
    if &hello != HELLO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "房间协议版本不匹配",
        ));
    }
    if server {
        stream.write_all(HELLO).await?;
    }
    Ok(())
}

// Each TCP message is a two-byte big-endian length followed by a bounded JSON payload.
// 每条 TCP 消息由两个字节的大端长度及其后长度受限的 JSON 载荷组成。
async fn write_message(stream: &mut TcpStream, message: Message) -> io::Result<()> {
    let mut frame = vec![0; 2];
    serde_json::to_writer(&mut frame, &message)?;
    let length = frame.len() - 2;
    if length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "消息过长"));
    }
    frame[..2].copy_from_slice(&(length as u16).to_be_bytes());
    stream.write_all(&frame).await
}

async fn read_message(stream: &mut TcpStream) -> io::Result<Message> {
    let mut length = [0; 2];
    stream.read_exact(&mut length).await?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "无效的消息长度"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    let message: Message = serde_json::from_slice(&bytes)?;
    if !message.valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "无效的房间消息"));
    }
    Ok(message)
}

async fn connection<F, Fut>(
    mut reader: TcpStream,
    outgoing: Receiver<Message>,
    mut accept: F,
) -> io::Result<()>
where
    F: FnMut(Message) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let mut writer = reader.clone();
    race(
        async {
            loop {
                accept(read_message(&mut reader).await?).await?;
            }
        },
        async {
            while let Ok(message) = outgoing.recv().await {
                write_message(&mut writer, message).await?;
            }
            Ok(())
        },
    )
    .await
}

enum HostEvent {
    Incoming(usize, Message),
    Disconnected(usize),
}

enum HostWake {
    Accepted(TcpStream, SocketAddr),
    Peer(HostEvent),
    Local(Message),
    Stop,
}

async fn host(
    id: Uuid,
    listener: TcpListener,
    local_messages: Receiver<Message>,
    incoming: &Sender<RoomEvent>,
) -> io::Result<()> {
    let (send_event, receive_event) = async_channel::bounded(64);
    let mut peers = HashMap::new();
    let mut next_peer = 0usize;
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
                async {
                    Ok(local_messages
                        .recv()
                        .await
                        .map_or(HostWake::Stop, HostWake::Local))
                },
            ),
        )
        .await?;
        match wake {
            HostWake::Accepted(mut stream, address)
                if local_peer(address) && peers.len() < MAX_PEERS =>
            {
                let peer = next_peer;
                next_peer = next_peer.wrapping_add(1);
                let (outgoing, messages) = async_channel::bounded(32);
                let events = send_event.clone();
                let task = smol::spawn(async move {
                    let ready = race(
                        async {
                            stream.set_nodelay(true)?;
                            handshake(&mut stream, true).await
                        },
                        async {
                            Timer::after(CONNECT_TIMEOUT).await;
                            Err(io::Error::from(io::ErrorKind::TimedOut))
                        },
                    )
                    .await;
                    if ready.is_ok() {
                        let _ = connection(stream, messages, |message| async {
                            events
                                .send(HostEvent::Incoming(peer, message))
                                .await
                                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
                        })
                        .await;
                    }
                    let _ = events.send(HostEvent::Disconnected(peer)).await;
                });
                peers.insert(peer, (outgoing, task));
            }
            HostWake::Accepted(..) => {}
            HostWake::Peer(HostEvent::Disconnected(peer)) => {
                peers.remove(&peer);
            }
            HostWake::Peer(HostEvent::Incoming(peer, message)) => {
                if peers.contains_key(&peer) {
                    broadcast(&mut peers, Some(peer), message.clone());
                    incoming
                        .send(RoomEvent::Message(id, message))
                        .await
                        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                }
            }
            HostWake::Local(message) => broadcast(&mut peers, None, message),
            HostWake::Stop => return Ok(()),
        }
    }
}

// Drop peers whose queues are full so a slow reader cannot block the rest of the room.
// 断开发送队列已满的对端，避免一个读取缓慢的客户端拖住整个房间。
fn broadcast(
    peers: &mut HashMap<usize, (Sender<Message>, Task<()>)>,
    origin: Option<usize>,
    message: Message,
) {
    peers.retain(|peer, (sender, _)| {
        Some(*peer) == origin || sender.try_send(message.clone()).is_ok()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Body;

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
    fn clients_join_by_endpoint_exchange_messages_and_notice_disconnect() {
        smol::block_on(race(
            async {
                let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let address = SocketAddrV4::new(
                    Ipv4Addr::LOCALHOST,
                    reservation.local_addr().unwrap().port(),
                );
                drop(reservation);
                let (host_events, host_inbox) = async_channel::bounded(8);
                let mut host = Room::open(RoomConfig::Host(address), host_events)
                    .await
                    .unwrap();
                host.activate();
                let (first_events, _first_inbox) = async_channel::bounded(8);
                let mut first = Room::open(RoomConfig::Join(address), first_events)
                    .await
                    .unwrap();
                first.activate();
                let (second_events, second_inbox) = async_channel::bounded(8);
                let mut second = Room::open(RoomConfig::Join(address), second_events)
                    .await
                    .unwrap();
                second.activate();
                let text = Message::new(Uuid::new_v4(), "甲".into(), Body::Text("你好".into()));
                first.send(text.clone()).unwrap();
                assert!(
                    matches!(host_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id)
                );
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Message(_, received) if received.id == text.id)
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
                drop(host);
                assert!(
                    matches!(second_inbox.recv().await.unwrap(), RoomEvent::Closed(id, _) if id == second.id())
                );
            },
            async {
                Timer::after(Duration::from_secs(5)).await;
                panic!("TCP room did not complete");
            },
        ));
    }
}
