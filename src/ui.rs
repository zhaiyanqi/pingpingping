//! GPUI Kit chat, history, and settings windows.
//! 使用 GPUI Kit 实现的聊天、历史和设置窗口。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

use async_channel::Sender;
use chrono::{DateTime, Local};
use gpui_kit::component::{
    ActiveTheme, Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    switch::Switch,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::assets::{EMOTE_WHEEL, EMOTES, PINGS};
use crate::gesture::{GestureEvent, Wheel};
use crate::model::{Body, Message, Model, Profile};
use crate::platform::{self, InputMonitor, Overlay};
use crate::room::{DEFAULT_PORT, Room, RoomConfig, RoomEvent};

gpui_kit::actions!([OpenChat, OpenHistory, OpenSettings, Quit]);

// The GPUI thread owns window and overlay state. Workers send typed events.
// GPUI 线程管理窗口与悬浮层状态，后台任务通过带类型的事件与其通信。
struct Desktop {
    model: Entity<Model>,
    incoming: Sender<RoomEvent>,
    gestures: Sender<GestureEvent>,
    enabled: Arc<AtomicBool>,
    monitor: Option<InputMonitor>,
    overlay: Overlay,
    chat: Option<AnyWindowHandle>,
    history: Option<AnyWindowHandle>,
    settings: Option<AnyWindowHandle>,
    emotes: [Arc<Image>; EMOTES.len()],
    pings: [Arc<Image>; 9],
}

impl Global for Desktop {}

pub(crate) fn start(cx: &mut App) {
    gpui_kit::init(cx);
    let (incoming, receiver) = async_channel::bounded(128);
    let (gestures, gesture_receiver) = async_channel::unbounded();
    let data_dir = std::env::var_os("PINGPINGPING_DATA_DIR")
        .map(PathBuf::from)
        .or_else(default_data_dir);
    let result = data_dir
        .ok_or_else(|| anyhow::anyhow!("找不到用户主目录"))
        .and_then(Model::load);
    let mut model = match result {
        Ok(model) => model,
        Err(error) => {
            let error = format!("{error:#}");
            let _ = gpui_kit::open_window(options("PingPingPing", 440., 210., cx), cx, |_, cx| {
                cx.new(|_| StartupError(error))
            });
            return;
        }
    };
    let enabled = Arc::new(AtomicBool::new(model.profile.gestures));
    let monitor = InputMonitor::start(gestures.clone(), enabled.clone());
    model.input_ready = monitor.is_some();
    let model = cx.new(|_| model);
    let desktop = Desktop {
        model,
        incoming,
        gestures,
        enabled,
        monitor,
        overlay: Overlay::new(),
        chat: None,
        history: None,
        settings: None,
        emotes: EMOTES
            .map(|clip| Arc::new(Image::from_bytes(ImageFormat::Png, clip.image.to_vec()))),
        pings: PINGS.map(|clip| Arc::new(Image::from_bytes(ImageFormat::Png, clip.image.to_vec()))),
    };
    cx.set_global(desktop);
    cx.on_app_quit(|cx| {
        let model = cx.global::<Desktop>().model.clone();
        let writer = model.update(cx, |model, _| model.finish_writes());
        cx.background_executor().spawn(async move {
            if let Some(writer) = writer {
                let _ = writer.join();
            }
        })
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(event) = receiver.recv().await {
            cx.update(|cx| {
                let model = cx.global::<Desktop>().model.clone();
                match event {
                    RoomEvent::Message(id, message) => {
                        let body = message.body.clone();
                        if model.update(cx, |model, cx| {
                            if model.receive(id, message) {
                                cx.notify();
                                model.profile.remote_overlay
                            } else {
                                false
                            }
                        }) {
                            play(&body, cx);
                        }
                    }
                    RoomEvent::Closed(id, reason) => model.update(cx, |model, cx| {
                        if model.closed(id, reason) {
                            cx.notify();
                        }
                    }),
                }
            });
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(event) = gesture_receiver.recv().await {
            cx.update(|cx| handle_gesture(event, cx));
        }
    })
    .detach();
    cx.on_action(|_: &OpenChat, cx| open_chat(cx));
    cx.on_action(|_: &OpenHistory, cx| open_history(cx));
    cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new(shortcut("1").as_str(), OpenChat, None),
        KeyBinding::new(shortcut("2").as_str(), OpenHistory, None),
        KeyBinding::new(shortcut(",").as_str(), OpenSettings, None),
        KeyBinding::new(shortcut("q").as_str(), Quit, None),
    ]);
    #[cfg(target_os = "macos")]
    cx.set_menus([Menu {
        name: "PingPingPing".into(),
        disabled: false,
        items: vec![
            MenuItem::action("聊天", OpenChat),
            MenuItem::action("历史记录…", OpenHistory),
            MenuItem::action("设置…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("退出", Quit),
        ],
    }]);
    open_chat(cx);
}

fn shortcut(key: &str) -> String {
    let modifier = if cfg!(target_os = "windows") {
        "ctrl"
    } else {
        "cmd"
    };
    format!("{modifier}-{key}")
}

fn default_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let (base, relative) = ("LOCALAPPDATA", "PingPingPing");
    #[cfg(target_os = "macos")]
    let (base, relative) = ("HOME", "Library/Application Support/PingPingPing");
    std::env::var_os(base).map(|path| PathBuf::from(path).join(relative))
}

struct StartupError(String);

impl Render for StartupError {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("无法启动 PingPingPing"),
            )
            .child(self.0.clone())
            .child(
                Button::new("quit")
                    .label("退出")
                    .on_click(|_, _, cx| cx.quit()),
            )
    }
}

fn play(body: &Body, cx: &mut App) {
    let (kind, index) = match body {
        Body::Emote { index, .. } => (Wheel::Emote, *index),
        Body::Ping { index, .. } => (Wheel::Ping, *index),
        Body::Text(_) => return,
    };
    let sound = cx.global::<Desktop>().model.read(cx).profile.sound;
    cx.update_global::<Desktop, _>(|desktop, cx| {
        desktop
            .overlay
            .play(kind, index, None, body.location(), sound, cx);
    });
}

fn handle_gesture(event: GestureEvent, cx: &mut App) {
    let profile = &cx.global::<Desktop>().model.read(cx).profile;
    let (enabled, sound) = (profile.gestures, profile.sound);
    cx.update_global::<Desktop, _>(|desktop, cx| match event {
        GestureEvent::Open(kind, origin, selected) if enabled => {
            desktop.overlay.open(kind, origin, selected, sound, cx)
        }
        GestureEvent::Select(index) if enabled => desktop.overlay.select(index, sound, cx),
        GestureEvent::Finish(choice) => {
            desktop.overlay.close(cx);
            if let Some((kind, index, origin)) = choice.filter(|_| enabled) {
                let index = if kind == Wheel::Emote {
                    EMOTE_WHEEL[index]
                } else {
                    index
                };
                let location = platform::normalized(origin);
                let body = match kind {
                    Wheel::Ping => Body::Ping { index, location },
                    Wheel::Emote => Body::Emote {
                        index,
                        location: Some(location),
                    },
                };
                desktop
                    .overlay
                    .play(kind, index, Some(origin), Some(location), sound, cx);
                desktop
                    .model
                    .update(cx, |model, cx| match model.send(body) {
                        Ok(()) => cx.notify(),
                        Err(error) => eprintln!("无法记录或发送手势：{error:#}"),
                    });
            }
        }
        _ => {}
    });
}

fn options(title: &str, width: f32, height: f32, cx: &App) -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
        window_min_size: Some(size(px(440.), px(height.min(400.)))),
        ..Default::default()
    }
}

fn focus_window(handle: Option<AnyWindowHandle>, cx: &mut App) -> bool {
    if handle.is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    }) {
        cx.activate(true);
        true
    } else {
        false
    }
}

pub(crate) fn open_chat(cx: &mut App) {
    if !cx.has_global::<Desktop>() || focus_window(cx.global::<Desktop>().chat, cx) {
        return;
    }
    if let Ok((handle, _)) = gpui_kit::open_window(
        options("PingPingPing · 聊天", 720., 650., cx),
        cx,
        |window, cx| cx.new(|cx| Conversation::new(false, window, cx)),
    ) {
        cx.global_mut::<Desktop>().chat = Some(handle);
        cx.activate(true);
    }
}

fn open_history(cx: &mut App) {
    if focus_window(cx.global::<Desktop>().history, cx) {
        return;
    }
    if let Ok((handle, _)) = gpui_kit::open_window(
        options("PingPingPing · 历史记录", 620., 580., cx),
        cx,
        |window, cx| cx.new(|cx| Conversation::new(true, window, cx)),
    ) {
        cx.global_mut::<Desktop>().history = Some(handle);
        cx.activate(true);
    }
}

fn open_settings(cx: &mut App) {
    if focus_window(cx.global::<Desktop>().settings, cx) {
        return;
    }
    if let Ok((handle, _)) = gpui_kit::open_window(
        options("PingPingPing · 设置", 560., 655., cx),
        cx,
        |window, cx| cx.new(|cx| Settings::new(window, cx)),
    ) {
        cx.global_mut::<Desktop>().settings = Some(handle);
        cx.activate(true);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Emotes,
}

struct Conversation {
    model: Entity<Model>,
    input: Option<Entity<InputState>>,
    history: bool,
    all_rooms: bool,
    room_key: String,
    filter: Filter,
    palette: bool,
    messages: ListState,
    rows: Vec<(usize, Uuid)>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Conversation {
    fn new(history: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            (!history).then(|| cx.new(|cx| InputState::new(window, cx).placeholder("输入消息")));
        let model = cx.global::<Desktop>().model.clone();
        let messages = ListState::new(0, ListAlignment::Bottom, px(180.));
        messages.set_follow_mode(FollowMode::Tail);
        let room_key = model.read(cx).room_key().to_owned();
        let mut subscriptions = vec![cx.observe_in(&model, window, |this, model, window, cx| {
            let room_key = model.read(cx).room_key();
            if this.room_key != room_key {
                this.room_key = room_key.to_owned();
                this.error = None;
                this.palette = false;
                if let Some(input) = &this.input {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
            }
            this.refresh_messages(cx);
            cx.notify();
        })];
        if let Some(input) = &input {
            subscriptions.push(
                cx.subscribe_in(input, window, |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => this.send_text(window, cx),
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                }),
            );
        }
        let mut conversation = Self {
            model,
            input,
            history,
            all_rooms: false,
            room_key,
            filter: Filter::All,
            palette: false,
            messages,
            rows: Vec::new(),
            error: None,
            _subscriptions: subscriptions,
        };
        conversation.refresh_messages(cx);
        conversation
    }

    // Keep surviving rows anchored by message ID when old history is trimmed.
    // 历史记录裁剪后，按消息 ID 保留仍存在的行及滚动位置。
    fn refresh_messages(&mut self, cx: &App) {
        let model = self.model.read(cx);
        let rows: Vec<_> = model
            .archive()
            .enumerate()
            .filter(|(_, (key, message))| {
                (self.all_rooms || *key == model.room_key())
                    && (self.filter == Filter::All || matches!(message.body, Body::Emote { .. }))
            })
            .map(|(index, (_, message))| (index, message.id))
            .collect();
        let first = rows
            .first()
            .and_then(|(_, id)| self.rows.iter().position(|(_, previous)| previous == id));
        if let Some(first) = first {
            let remaining = &self.rows[first..];
            if remaining.len() <= rows.len()
                && remaining.iter().zip(&rows).all(|(old, new)| old.1 == new.1)
            {
                self.messages.splice(0..first, 0);
                self.messages.splice(
                    remaining.len()..remaining.len(),
                    rows.len() - remaining.len(),
                );
            } else {
                self.messages.reset(rows.len());
                self.messages.set_follow_mode(FollowMode::Tail);
            }
        } else {
            self.messages.reset(rows.len());
            self.messages.set_follow_mode(FollowMode::Tail);
        }
        self.rows = rows;
    }

    fn send_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        let value = input.read(cx).value().trim().to_owned();
        if value.is_empty() {
            return;
        }
        if self.send(Body::Text(value), cx) {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    fn send(&mut self, body: Body, cx: &mut Context<Self>) -> bool {
        let result = self.model.update(cx, |model, cx| {
            let result = model.send(body.clone());
            if result.is_ok() {
                cx.notify();
            }
            result
        });
        let sent = result.is_ok();
        self.error = result.err().map(|error| format!("{error:#}"));
        if sent {
            self.messages.set_follow_mode(FollowMode::Tail);
            self.palette = false;
            play(&body, cx);
        }
        cx.notify();
        sent
    }
}

fn bubble(message: &Message, mine: bool, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let desktop = cx.global::<Desktop>();
    let clock = DateTime::from_timestamp(message.time as i64, 0)
        .map(|time| time.with_timezone(&Local).format("%m/%d %H:%M").to_string())
        .unwrap_or_default();
    let content = match &message.body {
        Body::Text(text) => div().child(text.clone()).into_any_element(),
        Body::Emote { index, .. } => div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(img(desktop.emotes[*index].clone()).size(px(72.)))
            .child(div().text_xs().child(EMOTES[*index].name))
            .into_any_element(),
        Body::Ping { index, .. } => div()
            .flex()
            .items_center()
            .gap_2()
            .child(img(desktop.pings[*index].clone()).size_6())
            .child(PINGS[*index].name)
            .into_any_element(),
    };
    let initial = message.name.chars().next().unwrap_or('·').to_string();
    div()
        .flex()
        .gap_2()
        .items_start()
        .when(mine, |row| row.flex_row_reverse())
        .child(
            div()
                .flex()
                .size_8()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(theme.accent)
                .text_color(theme.accent_foreground)
                .text_sm()
                .child(initial),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .max_w(relative(0.75))
                .min_w_0()
                .when(mine, |column| column.items_end())
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(if mine {
                            "我".into()
                        } else {
                            message.name.clone()
                        }))
                        .child(clock),
                )
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(if mine { theme.accent } else { theme.secondary })
                        .text_color(if mine {
                            theme.accent_foreground
                        } else {
                            theme.secondary_foreground
                        })
                        .child(content),
                ),
        )
}

impl Render for Conversation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let joined = model.joined();
        let error = self.error.clone().or_else(|| model.room_error.clone());
        let count = self.rows.len();
        let theme = cx.theme().clone();
        let feed =
            if count == 0 {
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(
                        if self.filter == Filter::Emotes {
                            "暂无表情"
                        } else {
                            "暂无消息"
                        },
                    ))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(
                        if self.history {
                            "当前会话中的文字、信号和表情会保存在本机。"
                        } else if joined {
                            "同一局域网的好友连接主机的 IP 和端口后，即可聊天。"
                        } else {
                            "在设置中创建房间，或输入主机的 IP 和端口连接。"
                        },
                    ))
                    .into_any_element()
            } else {
                let model_view = self.model.clone();
                let rows = self.rows.clone();
                let all_rooms = self.all_rooms;
                list(self.messages.clone(), move |index, _, cx| {
                    let model = model_view.read(cx);
                    let Some((key, message)) =
                        rows.get(index).and_then(|(index, _)| model.entry(*index))
                    else {
                        return div().into_any_element();
                    };
                    div()
                        .id(SharedString::from(format!("{key}/{}", message.id)))
                        .px_5()
                        .py_2()
                        .when(all_rooms, |row| {
                            let label = if key.is_empty() {
                                "本机".into()
                            } else if let Ok(endpoint) = key.parse::<std::net::SocketAddrV4>() {
                                format!("房间 {endpoint}")
                            } else {
                                format!("房间 {}", key.chars().take(6).collect::<String>())
                            };
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .mb_1()
                                    .child(label),
                            )
                        })
                        .child(bubble(message, message.sender == model.profile.id, cx))
                        .into_any_element()
                })
                .flex_1()
                .min_h_0()
                .py_2()
                .into_any_element()
            };
        let mut view = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .px_5()
                    .py_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(
                                if self.history {
                                    "历史记录"
                                } else {
                                    "局域网聊天"
                                },
                            ))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                if self.history {
                                    format!(
                                        "{} · {count} 条记录",
                                        if self.all_rooms {
                                            "所有会话"
                                        } else {
                                            "当前会话"
                                        }
                                    )
                                } else if let Some(connection) = model.connection() {
                                    format!(
                                        "{} · {}",
                                        match connection {
                                            RoomConfig::Host(_) => "正在监听",
                                            RoomConfig::Join(_) => "已连接",
                                        },
                                        connection.address()
                                    )
                                } else {
                                    "未加入房间".into()
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!self.history, |row| {
                                row.child(
                                    Button::new("history")
                                        .ghost()
                                        .label("历史记录…")
                                        .on_click(|_, _, cx| open_history(cx)),
                                )
                            })
                            .child(
                                Button::new("settings")
                                    .ghost()
                                    .label("设置…")
                                    .on_click(|_, _, cx| open_settings(cx)),
                            ),
                    ),
            );
        if self.history {
            view = view.child(
                div().flex().px_5().pt_2().child(
                    Button::new("room-scope")
                        .ghost()
                        .label(if self.all_rooms {
                            "仅看当前会话"
                        } else {
                            "查看所有会话"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.all_rooms = !this.all_rooms;
                            this.refresh_messages(cx);
                            this.messages.set_follow_mode(FollowMode::Tail);
                            cx.notify();
                        })),
                ),
            );
            view = view.child(
                div()
                    .flex()
                    .gap_2()
                    .px_5()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .children(
                        [
                            (Filter::All, "all", "全部"),
                            (Filter::Emotes, "emotes", "仅表情"),
                        ]
                        .map(|(filter, id, label)| {
                            Button::new(id)
                                .label(label)
                                .when(self.filter == filter, |button| button.primary())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.filter = filter;
                                    this.refresh_messages(cx);
                                    this.messages.set_follow_mode(FollowMode::Tail);
                                    cx.notify();
                                }))
                        }),
                    ),
            );
        }
        view = view.child(feed);
        if let Some(input) = &self.input {
            let emote_images = cx.global::<Desktop>().emotes.clone();
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .when_some(error, |footer, error| {
                        footer.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .when(self.palette, |footer| {
                        footer.child(div().flex().flex_wrap().gap_1().children(
                            emote_images.into_iter().enumerate().map(|(index, image)| {
                                Button::new(("emote", index))
                                    .ghost()
                                    .tooltip(EMOTES[index].name)
                                    .child(img(image).size_6())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send(
                                            Body::Emote {
                                                index,
                                                location: None,
                                            },
                                            cx,
                                        );
                                    }))
                            }),
                        ))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Button::new("toggle-emotes").label("表情").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.palette = !this.palette;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(input).disabled(!joined)),
                            )
                            .child(if joined {
                                Button::new("send").primary().label("发送").on_click(
                                    cx.listener(|this, _, window, cx| this.send_text(window, cx)),
                                )
                            } else {
                                Button::new("join")
                                    .primary()
                                    .label("连接房间…")
                                    .on_click(|_, _, cx| open_settings(cx))
                            }),
                    ),
            );
        }
        view
    }
}

struct Settings {
    model: Entity<Model>,
    name: Entity<InputState>,
    ip: Entity<InputState>,
    port: Entity<InputState>,
    sound: bool,
    gestures: bool,
    remote_overlay: bool,
    feedback: Option<(bool, String)>,
    connection_error: Option<String>,
    connecting: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Settings {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.global::<Desktop>().model.clone();
        let Profile {
            name,
            endpoint,
            sound,
            gestures,
            remote_overlay,
            ..
        } = model.read(cx).profile.clone();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let address = endpoint.map(RoomConfig::address);
        let ip = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("192.168.1.20")
                .default_value(
                    address
                        .map(|address| address.ip().to_string())
                        .unwrap_or_default(),
                )
        });
        let port = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                address
                    .map_or(DEFAULT_PORT, |address| address.port())
                    .to_string(),
            )
        });
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe_in(&ip, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.connect(false, cx);
                }
            }),
            cx.subscribe_in(&port, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.connect(false, cx);
                }
            }),
        ];
        Self {
            model,
            name,
            ip,
            port,
            sound,
            gestures,
            remote_overlay,
            feedback: None,
            connection_error: None,
            connecting: None,
            _subscriptions: subscriptions,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut profile = self.model.read(cx).profile.clone();
        profile.name = self.name.read(cx).value().trim().to_owned();
        profile.sound = self.sound;
        profile.gestures = self.gestures;
        profile.remote_overlay = self.remote_overlay;
        let result = self.model.update(cx, |model, cx| {
            model.apply(profile.clone())?;
            cx.notify();
            Ok::<_, anyhow::Error>(())
        });
        self.feedback = Some(match result {
            Ok(()) => {
                cx.update_global::<Desktop, _>(|desktop, _| {
                    desktop.enabled.store(profile.gestures, Ordering::Relaxed);
                    if !profile.gestures {
                        desktop.overlay.hide();
                    }
                });
                (true, "设置已保存".into())
            }
            Err(error) => (false, format!("{error:#}")),
        });
        cx.notify();
    }

    fn connect(&mut self, host: bool, cx: &mut Context<Self>) {
        if self.connecting.is_some() || self.model.read(cx).joined() {
            return;
        }
        let config =
            match RoomConfig::parse(host, &self.ip.read(cx).value(), &self.port.read(cx).value()) {
                Ok(config) => config,
                Err(error) => {
                    self.connection_error = Some(format!("{error:#}"));
                    cx.notify();
                    return;
                }
            };
        self.connection_error = None;
        let incoming = cx.global::<Desktop>().incoming.clone();
        self.connecting = Some(cx.spawn(async move |weak, cx| {
            let room = Room::open(config, incoming).await;
            let _ = weak.update(cx, |this, cx| {
                this.connecting.take();
                let result = room.and_then(|room| {
                    this.model.update(cx, |model, cx| {
                        model.connect(room)?;
                        cx.notify();
                        Ok(())
                    })
                });
                this.connection_error = result.err().map(|error| format!("{error:#}"));
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.connecting.take();
        self.model.update(cx, |model, cx| {
            model.disconnect();
            cx.notify();
        });
        self.connection_error = None;
        cx.notify();
    }

    fn retry_input(&mut self, cx: &mut Context<Self>) {
        let ready = cx.update_global::<Desktop, _>(|desktop, _| {
            if desktop.monitor.is_none() {
                desktop.monitor =
                    InputMonitor::start(desktop.gestures.clone(), desktop.enabled.clone());
            }
            desktop.monitor.is_some()
        });
        self.model.update(cx, |model, cx| {
            model.input_ready = ready;
            cx.notify();
        });
        self.feedback =
            (!ready).then(|| (false, "无法启动全局输入监听，请重新打开应用后重试。".into()));
        cx.notify();
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let connection = model.connection();
        let busy = self.connecting.is_some();
        let room_error = self
            .connection_error
            .clone()
            .or_else(|| (!busy).then(|| model.room_error.clone()).flatten());
        let active = model.input_ready;
        let theme = cx.theme().clone();
        div().id("settings-scroll").size_full().overflow_y_scroll().p_5().flex().flex_col().gap_5()
            .bg(theme.background).text_color(theme.foreground)
            .child(div().flex().flex_col().gap_2()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("昵称"))
                .child(Input::new(&self.name)))
            .child(div().flex().flex_col().gap_2().border_t_1().border_color(theme.border).pt_4()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("局域网房间"))
                .child(div().text_sm().text_color(theme.muted_foreground).child(if cfg!(target_os = "windows") {
                    "主机用 ipconfig 查找本机 IPv4，填入地址和端口后创建房间。好友用相同地址和端口连接。"
                } else {
                    "主机填写本机局域网 IPv4 和端口后创建房间。好友用相同地址和端口连接。"
                }))
                .child(div().flex().gap_3()
                    .child(div().flex_1().min_w_0().flex().flex_col().gap_1()
                        .child(div().text_sm().child("IPv4 地址"))
                        .child(Input::new(&self.ip).disabled(connection.is_some() || busy)))
                    .child(div().w(rems(7.)).flex().flex_col().gap_1()
                        .child(div().text_sm().child("TCP 端口"))
                        .child(Input::new(&self.port).disabled(connection.is_some() || busy))))
                .child(div().flex().flex_wrap().gap_2()
                    .when(connection.is_none() && !busy, |row| row
                        .child(Button::new("join-room").primary().label("连接").on_click(cx.listener(|this, _, _, cx| this.connect(false, cx))))
                        .child(Button::new("host-room").label("创建房间").on_click(cx.listener(|this, _, _, cx| this.connect(true, cx)))))
                    .when(busy, |row| row
                        .child(Button::new("pending-room").label("正在建立连接…").disabled(true))
                        .child(Button::new("cancel-room").ghost().label("取消").on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)))))
                    .when_some(connection, |row, config| row
                        .child(Button::new("copy-address").label("复制地址").on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(config.address().to_string()));
                        }))
                        .child(Button::new("disconnect-room").ghost()
                            .label(if matches!(config, RoomConfig::Host(_)) { "停止监听" } else { "断开连接" })
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))))))
                .when_some(connection, |group, config| group
                    .child(div().text_sm().text_color(theme.muted_foreground).child(format!("{} · {}", if matches!(config, RoomConfig::Host(_)) { "正在监听" } else { "已连接" }, config.address()))))
                .when_some(room_error, |group, error| group
                    .child(div().text_sm().text_color(theme.danger).child(error))))
            .child(div().flex().flex_col().gap_3().border_t_1().border_color(theme.border).pt_4()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("桌面轮盘"))
                .child(div().text_sm().text_color(theme.muted_foreground).child(if cfg!(target_os = "windows") {
                    "按住 Alt 或 Ctrl 移动鼠标，分别打开信号或表情轮盘，松开按键确认。Alt + 单击发送普通标记。"
                } else {
                    "按住 ⌥ 或 ⌃ 移动鼠标，分别打开信号或表情轮盘，松开按键确认。⌥ 单击发送普通标记。"
                }))
                .child(Switch::new("gestures").label("启用全局轮盘").checked(self.gestures)
                    .on_change(cx.listener(|this, value, _, cx| { this.gestures = *value; cx.notify(); })))
                .child(Switch::new("sound").label("播放音效").checked(self.sound)
                    .on_change(cx.listener(|this, value, _, cx| { this.sound = *value; cx.notify(); })))
                .child(Switch::new("room-overlay").label("在桌面播放房间信号和表情").checked(self.remote_overlay)
                    .on_change(cx.listener(|this, value, _, cx| { this.remote_overlay = *value; cx.notify(); })))
                .when(!active, |group| group
                    .child(div().text_sm().text_color(theme.muted_foreground).child(if cfg!(target_os = "macos") {
                        "请在系统设置的「隐私与安全性 → 输入监控」中授权，再重试。"
                    } else { "全局输入监听未启动。" }))
                    .child({
                        let row = div().flex().gap_2();
                        #[cfg(target_os = "macos")]
                        let row = row.child(Button::new("permission").label("请求输入监控权限…").on_click(|_, _, _| platform::request_input_access()));
                        row.child(Button::new("retry").ghost().label("重新检测").on_click(cx.listener(|this, _, _, cx| this.retry_input(cx))))
                    })))
            .child(div().flex().items_center().justify_between().gap_3().border_t_1().border_color(theme.border).pt_4()
                .child(div().flex_1().text_sm().when_some(self.feedback.clone(), |label, (ok, message)| {
                    label.text_color(if ok { theme.muted_foreground } else { theme.danger }).child(message)
                }))
                .child(Button::new("save").primary().label("保存设置")
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx)))))
    }
}
