//! GPUI Kit chat, history, and settings windows.
//! 使用 GPUI Kit 实现的聊天、历史和设置窗口。

use std::cell::{Cell, LazyCell, RefCell};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

use async_channel::Sender;
use chrono::{DateTime, Local};
use gpui_kit::assets::IconName;
use gpui_kit::base::{Button as BaseButton, Root};
use gpui_kit::component::{
    ActiveTheme, Disableable, IndexPath, Selectable, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    popover::{Popover, PopoverState},
    scroll::ScrollableElement,
    select::{Select, SelectItem, SelectState},
    slider::{Slider, SliderState},
    switch::Switch,
    tooltip::Tooltip,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::assets::{EMOTES, PINGS};
use crate::emotes::{EmoteId, EmoteStore, LibraryEmote};
use crate::gesture::{GestureEvent, TriggerKey, Triggers, Wheel};
use crate::model::{
    ANIMATION_TIMEOUT, Body, EMOTE_SIZE, EmotePage, Location, Message, Model, OverlaySettings,
    PING_SIZE, Profile, SOUND_VOLUME, TextEffect, TextMotion, TextOverlaySettings, TextPosition,
    WheelEmote, append_wheel_emote, wheel_has_emote,
};
use crate::platform::{
    self, AnimationPosition, InputMonitor, Overlay, WheelContents, WheelFrame, WheelPreview,
};
use crate::room::{DEFAULT_PORT, Room, RoomConfig, RoomEvent};

gpui_kit::actions!([OpenChat, OpenHistory, OpenSettings, CloseWindow, Quit]);

struct WheelEmoteAdded(WheelEmote);

impl EventEmitter<WheelEmoteAdded> for Model {}

// The GPUI thread owns window and overlay state. Workers send typed events.
// GPUI 线程管理窗口与悬浮层状态，后台任务通过带类型的事件与其通信。
struct Desktop {
    tray: Option<crate::tray::Tray>,
    model: Entity<Model>,
    library: Entity<LocalEmotes>,
    incoming: Sender<RoomEvent>,
    capture_paused: Arc<AtomicBool>,
    monitor: Option<(InputMonitor, Task<()>)>,
    overlay: LazyCell<Overlay>,
    wheel_posters: Option<Task<()>>,
    chat: Option<AnyWindowHandle>,
    history: Option<AnyWindowHandle>,
    settings: Option<AnyWindowHandle>,
    emotes: [Arc<Image>; EMOTES.len()],
    pings: [Arc<Image>; PINGS.len()],
}

impl Global for Desktop {}

pub(crate) fn start(cx: &mut App) {
    gpui_kit::init(cx);
    let (incoming, receiver) = async_channel::bounded(128);
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
    let capture_paused = Arc::new(AtomicBool::new(false));
    let monitor = start_input(&model.profile, capture_paused.clone(), cx);
    model.input_ready = monitor.is_some();
    let mut desktop = Desktop::new(model, incoming, capture_paused, cx);
    desktop.monitor = monitor;
    cx.set_global(desktop);
    refresh_wheel_posters(cx);
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
                        if model.update(cx, |model, cx| {
                            if model.receive(id, &message) {
                                cx.notify();
                                model.profile.plays_incoming_effects()
                            } else {
                                false
                            }
                        }) {
                            play(
                                &message.body,
                                message.text_effect,
                                Some(&message.name),
                                message.location(),
                                cx,
                            );
                        }
                    }
                    RoomEvent::Members(id, members) => model.update(cx, |model, cx| {
                        if model.update_room_members(id, members) {
                            cx.notify();
                        }
                    }),
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
    register_window_actions(cx);
    let (sender, commands) = async_channel::unbounded();
    let muted = cx
        .global::<Desktop>()
        .model
        .read(cx)
        .profile
        .incoming_muted();
    match crate::tray::install(sender, muted) {
        Ok(tray) => {
            cx.global_mut::<Desktop>().tray = Some(tray);
            #[cfg(target_os = "macos")]
            cx.set_activation_policy(ActivationPolicy::Accessory);
            cx.set_quit_mode(QuitMode::Explicit);
            cx.spawn(async move |cx| {
                while let Ok(command) = commands.recv().await {
                    cx.update(|cx| match command {
                        crate::tray::Command::Chat => open_chat(cx),
                        crate::tray::Command::History => open_history(cx),
                        crate::tray::Command::Settings => open_settings(cx),
                        crate::tray::Command::ToggleMute => {
                            let muted = cx
                                .global::<Desktop>()
                                .model
                                .read(cx)
                                .profile
                                .incoming_muted();
                            if let Err(error) = set_incoming_muted(!muted, cx) {
                                eprintln!("无法切换静音：{error:#}");
                            }
                        }
                        crate::tray::Command::Quit => cx.quit(),
                    });
                }
            })
            .detach();
        }
        Err(error) => {
            eprintln!("托盘不可用：{error:#}");
            open_chat(cx);
        }
    }
}

fn register_window_actions(cx: &mut App) {
    cx.on_action(|_: &OpenChat, cx| open_chat(cx));
    cx.on_action(|_: &OpenHistory, cx| open_history(cx));
    cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
    cx.on_action(|_: &CloseWindow, cx| close_active_window(cx));
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new(shortcut("1").as_str(), OpenChat, None),
        KeyBinding::new(shortcut("2").as_str(), OpenHistory, None),
        KeyBinding::new(shortcut(",").as_str(), OpenSettings, None),
        KeyBinding::new(shortcut("w").as_str(), CloseWindow, None),
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
            MenuItem::action("关闭窗口", CloseWindow),
            MenuItem::separator(),
            MenuItem::action("退出", Quit),
        ],
    }]);
}

fn set_incoming_muted(muted: bool, cx: &mut App) -> anyhow::Result<()> {
    let model = cx.global::<Desktop>().model.clone();
    let result = model.update(cx, |model, cx| {
        model.set_incoming_muted(muted)?;
        cx.notify();
        Ok(())
    });
    // Native check items toggle on selection. Restore the saved check if the write fails.
    // 原生菜单项会在选中时切换勾选。保存失败时恢复已保存的状态。
    if let Some(tray) = &cx.global::<Desktop>().tray {
        tray.set_incoming_muted(model.read(cx).profile.incoming_muted());
    }
    result
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

fn play(
    body: &Body,
    text_effect: TextEffect,
    sender: Option<&str>,
    location: Option<Location>,
    cx: &mut App,
) {
    cx.update_global::<Desktop, _>(|desktop, cx| {
        let position = AnimationPosition::Relative(location.unwrap_or(Location::CENTER));
        desktop.play(body, text_effect, sender, position, cx);
    });
}

impl Desktop {
    fn new(
        model: Model,
        incoming: Sender<RoomEvent>,
        capture_paused: Arc<AtomicBool>,
        cx: &mut App,
    ) -> Self {
        let library = cx.new(|cx| LocalEmotes::new(model.emotes(), cx));
        Self {
            tray: None,
            model: cx.new(|_| model),
            library,
            incoming,
            capture_paused,
            monitor: None,
            overlay: LazyCell::new(Overlay::new),
            wheel_posters: None,
            chat: None,
            history: None,
            settings: None,
            emotes: EMOTES
                .map(|clip| Arc::new(Image::from_bytes(ImageFormat::Png, clip.image.to_vec()))),
            pings: PINGS
                .map(|clip| Arc::new(Image::from_bytes(ImageFormat::Png, clip.image.to_vec()))),
        }
    }

    fn play(
        &mut self,
        body: &Body,
        text_effect: TextEffect,
        sender: Option<&str>,
        position: AnimationPosition,
        cx: &App,
    ) {
        let model = self.model.read(cx);
        let settings = model.profile.overlay.with_sound(model.profile.sound);
        match body {
            Body::Text(text) => self.overlay.play_text(text, settings.text, text_effect, cx),
            _ => self.overlay.play(
                body,
                sender.unwrap_or(&model.profile.name),
                position,
                &model.emotes(),
                settings,
                cx,
            ),
        }
    }
}

fn refresh_wheel_posters(cx: &mut App) {
    let model = cx.global::<Desktop>().model.read(cx);
    let store = model.emotes();
    let pages = model.profile.emote_pages.clone();
    let task = cx.spawn(async move |cx| {
        let posters = smol::unblock(move || platform::load_wheel_posters(&store, &pages)).await;
        cx.update(|cx| {
            cx.update_global::<Desktop, _>(|desktop, cx| {
                desktop.overlay.set_wheel_posters(posters, cx)
            });
        });
    });
    cx.global_mut::<Desktop>().wheel_posters = Some(task);
}

fn start_input(
    profile: &Profile,
    paused: Arc<AtomicBool>,
    cx: &mut App,
) -> Option<(InputMonitor, Task<()>)> {
    let (sender, receiver) = async_channel::unbounded();
    let monitor = InputMonitor::start(
        sender,
        profile.gestures.then(|| profile.triggers.clone()),
        paused,
    )?;
    let task = cx.spawn(async move |cx| {
        while let Ok(event) = receiver.recv().await {
            cx.update(|cx| handle_gesture(event, cx));
        }
    });
    Some((monitor, task))
}

fn handle_gesture(event: GestureEvent, cx: &mut App) {
    let profile = &cx.global::<Desktop>().model.read(cx).profile;
    let enabled = profile.gestures;
    let settings = profile.overlay.with_sound(profile.sound);
    let tick_volume = settings.volume().filter(|_| settings.wheel_sound);
    let contents = match event {
        GestureEvent::Open(Wheel::Emote, ..) => WheelContents::Emote(profile.emote_pages.clone()),
        _ => WheelContents::Ping,
    };
    cx.update_global::<Desktop, _>(|desktop, cx| match event {
        GestureEvent::Open(_, origin, selected) if enabled => {
            desktop
                .overlay
                .open(contents, origin, selected, tick_volume, cx)
        }
        GestureEvent::Select(index) if enabled => desktop.overlay.select(index, tick_volume, cx),
        GestureEvent::Page(steps, selected) if enabled => {
            desktop
                .overlay
                .turn_wheel_page(steps, selected, tick_volume, cx)
        }
        GestureEvent::Finish(choice) => {
            // Resolve the slot against the page shown by this gesture before closing it.
            // 关闭轮盘前，按本次手势显示的当前页解析槽位。
            let message = choice.filter(|_| enabled).and_then(|(kind, slot, origin)| {
                let body = match kind {
                    Wheel::Ping => Body::Ping {
                        index: slot,
                        location: None,
                    },
                    Wheel::Emote => match desktop.overlay.wheel_emote(slot)? {
                        WheelEmote::BuiltIn(index) => Body::Emote {
                            index,
                            location: None,
                        },
                        WheelEmote::Custom(id) => Body::CustomEmote(id),
                    },
                };
                Some((body, origin))
            });
            desktop.overlay.close(cx);
            if let Some((body, origin)) = message {
                let location = platform::normalized(origin);
                desktop.play(
                    &body,
                    TextEffect::Default,
                    None,
                    AnimationPosition::Local(origin),
                    cx,
                );
                desktop.model.update(cx, |model, cx| {
                    match model.send(body, TextEffect::Default, Some(location)) {
                        Ok(()) => cx.notify(),
                        Err(error) => eprintln!("无法记录或发送手势：{error:#}"),
                    }
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

fn should_close_window(window: &Window, cx: &mut App) -> bool {
    let desktop = cx.global::<Desktop>();
    let has_tray = desktop.tray.is_some();
    if desktop.settings == Some(window.window_handle())
        && let Ok(settings) = Root::read(window, cx).view().clone().downcast::<Settings>()
    {
        // A quick reopen can happen before the visibility observer sees the hide.
        // 快速重新打开时，窗口可见性监听可能还未收到隐藏事件。
        settings.update(cx, Settings::end_capture);
    }
    !has_tray || !crate::tray::hide_window(window)
}

fn close_to_tray(window: &Window, cx: &App) {
    window.on_window_should_close(cx, |window, cx| should_close_window(window, cx));
}

fn close_window(window: &mut Window, cx: &mut App) {
    if should_close_window(window, cx) {
        window.remove_window();
    }
}

fn close_active_window(cx: &mut App) {
    let desktop = cx.global::<Desktop>();
    let Some(handle) = cx.active_window().filter(|handle| {
        [desktop.chat, desktop.history, desktop.settings].contains(&Some(*handle))
    }) else {
        cx.propagate();
        return;
    };
    // A key action may already be updating this window.
    // 快捷键触发时，当前窗口可能仍处于更新过程中。
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| close_window(window, cx));
    });
}

fn focus_window(handle: Option<AnyWindowHandle>, cx: &mut App) -> bool {
    if handle.is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| crate::tray::restore_window(window))
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
        WindowOptions {
            window_min_size: Some(size(px(560.), px(400.))),
            ..options("PingPingPing · 设置", 700., 655., cx)
        },
        cx,
        |window, cx| cx.new(|cx| Settings::new(window, cx)),
    ) {
        cx.global_mut::<Desktop>().settings = Some(handle);
        cx.activate(true);
    }
}

struct LocalEmotes {
    store: EmoteStore,
    entries: Vec<LibraryEmote>,
    task: Option<Task<()>>,
    error: Option<String>,
}

impl LocalEmotes {
    fn new(store: EmoteStore, cx: &mut Context<Self>) -> Self {
        let local = store.clone();
        let task = cx.spawn(async move |weak: WeakEntity<Self>, cx| {
            let result = smol::unblock(move || local.library()).await;
            let _ = weak.update(cx, |this, cx| {
                this.task.take();
                match result {
                    Ok(entries) => this.entries = entries,
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        });
        Self {
            store,
            entries: Vec::new(),
            task: Some(task),
            error: None,
        }
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        self.import_image(None, |_, _| {}, cx);
    }

    fn contains(&self, id: EmoteId) -> bool {
        self.entries.iter().any(|emote| emote.id() == id)
    }

    fn remember(&mut self, emote: LibraryEmote) -> EmoteId {
        let id = emote.id();
        self.entries.retain(|known| known.id() != id);
        self.entries.insert(0, emote);
        id
    }

    fn import_image(
        &mut self,
        path: Option<PathBuf>,
        completed: impl FnOnce(Option<EmoteId>, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.task.is_some() {
            return false;
        }
        self.error = None;
        let prompt = path.is_none().then(|| {
            cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("选择 GIF、PNG、JPEG 或 WebP 图片".into()),
            })
        });
        let store = self.store.clone();
        self.task = Some(cx.spawn(async move |weak, cx| {
            let result = async {
                let path = match prompt {
                    Some(prompt) => prompt.await??.and_then(|paths| paths.into_iter().next()),
                    None => path,
                };
                let Some(path) = path else {
                    return Ok(None);
                };
                smol::unblock(move || store.import(&path)).await.map(Some)
            }
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.task.take();
                let id = match result {
                    Ok(emote) => emote.map(|emote| this.remember(emote)),
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        None
                    }
                };
                completed(id, cx);
                cx.notify();
            });
        }));
        cx.notify();
        true
    }

    fn save_received(
        &mut self,
        emote: WheelEmote,
        completed: impl FnOnce(anyhow::Result<()>, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.task.is_some() {
            return false;
        }
        let store = self.store.clone();
        self.task = Some(cx.spawn(async move |weak, cx| {
            let result = match emote {
                WheelEmote::Custom(id) => smol::unblock(move || store.save_received(id))
                    .await
                    .map(Some),
                WheelEmote::BuiltIn(_) => Ok(None),
            };
            let _ = weak.update(cx, |this, cx| {
                this.task.take();
                let result = result.and_then(|entry| {
                    let new_entry = entry.filter(|entry| !this.contains(entry.id()));
                    let library_changed = new_entry.is_some();
                    if let Some(entry) = new_entry {
                        this.remember(entry);
                    }
                    let model = cx.global::<Desktop>().model.clone();
                    let added = model.update(cx, |model, cx| {
                        let added = model.add_wheel_emote(emote)?;
                        if added {
                            cx.emit(WheelEmoteAdded(emote));
                            cx.notify();
                        }
                        Ok::<_, anyhow::Error>(added)
                    })?;
                    if added || library_changed {
                        refresh_wheel_posters(cx);
                    }
                    Ok(())
                });
                completed(result, cx);
                cx.notify();
            });
        }));
        cx.notify();
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Emotes,
}

#[derive(Clone)]
enum EmoteSaveStatus {
    Saving,
    Saved,
    Failed(String),
}

struct Conversation {
    model: Entity<Model>,
    input: Option<Entity<TextareaState>>,
    text_effect: Option<Entity<OverlaySelect<TextEffect>>>,
    history: bool,
    all_rooms: bool,
    room_key: String,
    filter: Filter,
    messages: ListState,
    rows: Vec<(usize, Uuid)>,
    error: Option<String>,
    emote_save: Option<(Uuid, EmoteSaveStatus)>,
    _subscriptions: Vec<Subscription>,
}

impl Conversation {
    fn new(history: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        close_to_tray(window, cx);
        let input = (!history).then(|| {
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("输入消息")
                    .auto_grow(2, 4)
                    .submit_on_enter(true)
            })
        });
        let text_effect =
            (!history).then(|| overlay_select(&TEXT_EFFECTS, TextEffect::Default, window, cx));
        let model = cx.global::<Desktop>().model.clone();
        let messages = ListState::new(0, ListAlignment::Bottom, px(180.));
        messages.set_follow_mode(FollowMode::Tail);
        let room_key = model.read(cx).room_key().to_owned();
        let mut subscriptions = vec![cx.observe_in(&model, window, |this, model, window, cx| {
            let room_key = model.read(cx).room_key();
            if this.room_key != room_key {
                this.room_key = room_key.to_owned();
                this.error = None;
                if let Some(input) = &this.input {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
                if let Some(effect) = &this.text_effect {
                    effect.update(cx, |effect, cx| {
                        effect.set_selected_value(&TextEffect::Default, window, cx);
                    });
                }
            }
            this.refresh_messages(cx);
            cx.notify();
        })];
        if !history {
            let library = cx.global::<Desktop>().library.clone();
            subscriptions.push(cx.observe(&library, |this, _, cx| {
                this.messages.remeasure();
                cx.notify();
            }));
        }
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
            text_effect,
            history,
            all_rooms: false,
            room_key,
            filter: Filter::All,
            messages,
            rows: Vec::new(),
            error: None,
            emote_save: None,
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
                    && (self.filter == Filter::All || message.body.is_emote())
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

    fn text_draft(&self, cx: &App) -> Option<(Body, TextEffect)> {
        let value = self.input.as_ref()?.read(cx).value();
        let text = value.trim();
        if text.is_empty() {
            return None;
        }
        let effect = self
            .text_effect
            .as_ref()?
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or_default();
        Some((Body::Text(text.to_owned()), effect))
    }

    fn send_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((body, effect)) = self.text_draft(cx) else {
            return;
        };
        if self.send(body, effect, cx) {
            if let Some(effect) = &self.text_effect {
                effect.update(cx, |effect, cx| {
                    effect.set_selected_value(&TextEffect::Default, window, cx);
                });
            }
            if let Some(input) = &self.input {
                input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
        }
    }

    fn send(&mut self, body: Body, text_effect: TextEffect, cx: &mut Context<Self>) -> bool {
        let result = self.model.update(cx, |model, cx| {
            let result = model.send(body.clone(), text_effect, None);
            if result.is_ok() {
                cx.notify();
            }
            result
        });
        let sent = result.is_ok();
        self.error = result.err().map(|error| format!("{error:#}"));
        if sent {
            self.messages.set_follow_mode(FollowMode::Tail);
            play(&body, text_effect, None, None, cx);
        }
        cx.notify();
        sent
    }

    fn save_received_emote(&mut self, id: Uuid, emote: WheelEmote, cx: &mut Context<Self>) {
        let library = cx.global::<Desktop>().library.clone();
        let owner = cx.weak_entity();
        if library.update(cx, |library, cx| {
            library.save_received(
                emote,
                move |result, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        let status = match result {
                            Ok(()) => EmoteSaveStatus::Saved,
                            Err(error) => {
                                EmoteSaveStatus::Failed(format!("无法添加表情。{error:#}"))
                            }
                        };
                        this.emote_save = Some((id, status));
                        cx.notify();
                    });
                },
                cx,
            )
        }) {
            self.emote_save = Some((id, EmoteSaveStatus::Saving));
            cx.notify();
        }
    }

    fn picker(wheel: Wheel, cx: &Context<Self>) -> Popover {
        let (id, label, icon) = match wheel {
            Wheel::Emote => ("emote-picker", "表情", IconName::ThumbsUp),
            Wheel::Ping => ("ping-picker", "信号", IconName::Bell),
        };
        let owner = cx.weak_entity();
        Popover::new(id)
            .anchor(Anchor::BottomLeft)
            .trigger(Button::new(id).ghost().small().label(label).icon(icon))
            .content(move |_, _, cx| {
                div().children(
                    owner
                        .upgrade()
                        .map(|entity| entity.read(cx).picker_content(wheel, &owner, cx)),
                )
            })
    }

    fn picker_content(
        &self,
        wheel: Wheel,
        owner: &WeakEntity<Self>,
        cx: &Context<PopoverState>,
    ) -> impl IntoElement + use<> {
        let popup = cx.weak_entity();
        let choose = |body: Body| {
            let owner = owner.clone();
            let popup = popup.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                if owner
                    .update(cx, |this, cx| {
                        this.send(body.clone(), TextEffect::Default, cx)
                    })
                    .unwrap_or(false)
                {
                    let _ = popup.update(cx, |state, cx| state.dismiss(window, cx));
                }
            }
        };
        let desktop = cx.global::<Desktop>();
        let (clips, images): (&[_], &[_]) = match wheel {
            Wheel::Emote => (&EMOTES, &desktop.emotes),
            Wheel::Ping => (&PINGS, &desktop.pings),
        };
        let mut panel =
            div().w_72().flex().flex_col().gap_2().child(
                div().flex().flex_wrap().gap_1().children(
                    clips
                        .iter()
                        .zip(images)
                        .enumerate()
                        .map(|(index, (clip, image))| {
                            let body = match wheel {
                                Wheel::Emote => Body::Emote {
                                    index,
                                    location: None,
                                },
                                Wheel::Ping => Body::Ping {
                                    index,
                                    location: None,
                                },
                            };
                            Button::new(("preset", index))
                                .ghost()
                                .accessibility_label(clip.name)
                                .tooltip(clip.name)
                                .child(img(image.clone()).size_6())
                                .on_click(choose(body))
                        }),
                ),
            );
        if wheel == Wheel::Emote {
            let library = desktop.library.read(cx);
            let import = desktop.library.clone();
            let theme = cx.theme();
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_2()
                    .when(!library.entries.is_empty(), |section| {
                        section.child(
                            div()
                                .id("imported-emotes")
                                .max_h(rems(7.))
                                .overflow_y_scroll()
                                .p_1()
                                .child(div().flex().flex_wrap().gap_1().children(
                                    library.entries.iter().map(|emote| {
                                        Button::new(SharedString::from(emote.id().to_string()))
                                            .ghost()
                                            .accessibility_label(emote.name().to_owned())
                                            .tooltip(emote.name().to_owned())
                                            .child(
                                                img(library.store.poster(emote.id()))
                                                    .size_6()
                                                    .object_fit(ObjectFit::Contain),
                                            )
                                            .on_click(choose(Body::CustomEmote(emote.id())))
                                    }),
                                )),
                        )
                    })
                    .when_some(library.error.clone(), |section, error| {
                        section.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .child(
                        div().child(
                            Button::new("import-emote")
                                .small()
                                .ghost()
                                .icon(IconName::Plus)
                                .label("导入图片…")
                                .loading(library.task.is_some())
                                .on_click(move |_, _, cx| {
                                    import.update(cx, LocalEmotes::import);
                                }),
                        ),
                    ),
            );
        }
        panel
    }
}

fn received_emote_action(
    message: &Message,
    owner: &WeakEntity<Conversation>,
    status: Option<&EmoteSaveStatus>,
    cx: &App,
) -> Option<Div> {
    let emote = match message.body {
        Body::Emote { index, .. } => WheelEmote::BuiltIn(index),
        Body::CustomEmote(id) => WheelEmote::Custom(id),
        _ => return None,
    };
    let desktop = cx.global::<Desktop>();
    let library = desktop.library.read(cx);
    let in_library = match emote {
        WheelEmote::BuiltIn(_) => true,
        WheelEmote::Custom(id) => library.contains(id),
    };
    let on_wheel = wheel_has_emote(&desktop.model.read(cx).profile.emote_pages, emote);
    let done = in_library && on_wheel;
    if done && !matches!(status, Some(EmoteSaveStatus::Saved)) {
        return None;
    }
    let label = if done {
        "已添加"
    } else if in_library {
        "添加到轮盘"
    } else {
        "添加到表情库"
    };
    let saving = matches!(status, Some(EmoteSaveStatus::Saving));
    let id = message.id;
    let owner = owner.clone();
    let mut action = div().flex().flex_col().items_start().child(
        Button::new("save-received-emote")
            .ghost()
            .small()
            .label(label)
            .loading(saving)
            .disabled(done || (library.task.is_some() && !saving))
            .on_click(move |_, _, cx| {
                let _ = owner.update(cx, |this, cx| this.save_received_emote(id, emote, cx));
            }),
    );
    if let Some(EmoteSaveStatus::Failed(error)) = status {
        action = action.child(
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(error.clone()),
        );
    }
    Some(action)
}

fn bubble(message: &Message, mine: bool, action: Option<Div>, cx: &App) -> impl IntoElement {
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
        Body::CustomEmote(id) => {
            let id = *id;
            let name = message.name.clone();
            let store = desktop.model.read(cx).emotes();
            Button::new("play-custom")
                .ghost()
                .h_auto()
                .accessibility_label("播放自定义表情")
                .tooltip("播放自定义表情")
                .child(
                    img(store.poster(id))
                        .size_20()
                        .object_fit(ObjectFit::Contain)
                        .with_fallback(|| div().text_xs().child("图片不可用").into_any_element()),
                )
                .on_click(move |_, _, cx| {
                    play(
                        &Body::CustomEmote(id),
                        TextEffect::Default,
                        Some(&name),
                        None,
                        cx,
                    )
                })
                .into_any_element()
        }
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
                )
                .children(action),
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
                            "消息保存在本机。加入房间后即可和好友聊天。"
                        },
                    ))
                    .into_any_element()
            } else {
                let model_view = self.model.clone();
                let rows = self.rows.clone();
                let all_rooms = self.all_rooms;
                let save_owner = (!self.history).then(|| cx.weak_entity());
                let emote_save = self.emote_save.clone();
                list(self.messages.clone(), move |index, _, cx| {
                    let model = model_view.read(cx);
                    let Some((key, message)) =
                        rows.get(index).and_then(|(index, _)| model.entry(*index))
                    else {
                        return div().into_any_element();
                    };
                    let mine = message.sender == model.profile.id;
                    let action = save_owner.as_ref().filter(|_| !mine).and_then(|owner| {
                        let status = emote_save
                            .as_ref()
                            .filter(|(id, _)| *id == message.id)
                            .map(|(_, status)| status);
                        received_emote_action(message, owner, status, cx)
                    });
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
                        .child(bubble(message, mine, action, cx))
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
                                    "本机 · 未加入房间".into()
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
        if let (Some(input), Some(effect)) = (&self.input, &self.text_effect) {
            let empty = input.read(cx).value().trim().is_empty();
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(Self::picker(Wheel::Emote, cx))
                            .child(Self::picker(Wheel::Ping, cx)),
                    )
                    .child(Textarea::new(input).aria_label("输入消息"))
                    .when_some(error, |footer, error| {
                        footer.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .min_w_0()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            // Select fills its parent even when its inner trigger has a set width.
                            // 即使给触发器指定宽度，Select 根元素仍会填满父容器。
                            .child(
                                div().w(rems(10.)).min_w_0().child(
                                    Select::new(effect)
                                        .accessibility_label("文字动效")
                                        .title_prefix("动效 · ")
                                        .w_full(),
                                ),
                            )
                            .child(
                                Button::new("preview-text-draft")
                                    .ghost()
                                    .label("预览")
                                    .accessibility_label("预览文字动效")
                                    .disabled(empty)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some((body, effect)) = this.text_draft(cx) {
                                            play(&body, effect, None, None, cx);
                                        }
                                    })),
                            )
                            .child(
                                Button::new("send")
                                    .primary()
                                    .label("发送")
                                    .tooltip("Enter 发送 · Shift+Enter 换行")
                                    .disabled(empty)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_text(window, cx)
                                    })),
                            ),
                    ),
            );
        }
        view
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    General,
    Room,
    Wheel,
    Animation,
    Sound,
}

const SLOT_DIRECTIONS: [&str; 8] = ["上", "右上", "右", "右下", "下", "左下", "左", "左上"];

#[derive(Clone, Copy, PartialEq, Eq)]
struct WheelSlot {
    page: usize,
    sector: usize,
}

impl WheelSlot {
    fn after_page_removed(self, removed: usize) -> Option<Self> {
        (self.page != removed).then(|| Self {
            page: self.page - usize::from(self.page > removed),
            ..self
        })
    }
}

struct EmoteDragPreview {
    image: ImageSource,
    offset: Point<Pixels>,
}

impl Render for EmoteDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let half = window.rem_size();
        div()
            .relative()
            .left(self.offset.x - half)
            .top(self.offset.y - half)
            .size_8()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(
                img(self.image.clone())
                    .size_6()
                    .object_fit(ObjectFit::Contain),
            )
    }
}

fn draggable_emote<T: 'static, B: InteractiveElement + Styled>(
    mut button: B,
    value: T,
    image: ImageSource,
) -> B {
    button
        .interactivity()
        .on_drag(value, move |_, offset, _, cx| {
            cx.new(|_| EmoteDragPreview {
                image: image.clone(),
                offset,
            })
        });
    button.cursor_grab()
}

fn wheel_sector_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<usize> {
    let offset = position - bounds.center();
    platform::editor_sector(
        f64::from(offset.x),
        f64::from(offset.y),
        f64::from(bounds.size.width),
    )
}

impl SettingsCategory {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Room,
        Self::Wheel,
        Self::Animation,
        Self::Sound,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "常规",
            Self::Room => "局域网房间",
            Self::Wheel => "桌面轮盘",
            Self::Animation => "桌面动画",
            Self::Sound => "音效",
        }
    }
}

const TEXT_POSITIONS: [(TextPosition, &str); 4] = [
    (TextPosition::TopLeft, "左上角"),
    (TextPosition::TopRight, "右上角"),
    (TextPosition::BottomLeft, "左下角"),
    (TextPosition::BottomRight, "右下角"),
];
const TEXT_ENTRANCE_MOTIONS: [(TextMotion, &str); 3] = [
    (TextMotion::None, "无动效"),
    (TextMotion::Fade, "淡入"),
    (TextMotion::Slide, "滑入"),
];
const TEXT_EXIT_MOTIONS: [(TextMotion, &str); 3] = [
    (TextMotion::None, "无动效"),
    (TextMotion::Fade, "淡出"),
    (TextMotion::Slide, "滑出"),
];
const TEXT_EFFECTS: [(TextEffect, &str); 7] = [
    (TextEffect::Default, "跟随设置"),
    (TextEffect::Shake, "爆炸抖动"),
    (TextEffect::FlyIn, "飞入"),
    (TextEffect::Bounce, "弹跳"),
    (TextEffect::Fade, "淡入淡出"),
    (TextEffect::Slide, "滑入滑出"),
    (TextEffect::None, "无动效"),
];

#[derive(Clone)]
struct OverlayChoice<T>(T, &'static str);

impl<T: Copy + PartialEq> SelectItem for OverlayChoice<T> {
    type Value = T;

    fn title(&self) -> SharedString {
        self.1.into()
    }

    fn value(&self) -> &T {
        &self.0
    }
}

type OverlaySelect<T> = SelectState<Vec<OverlayChoice<T>>>;

fn overlay_select<T: Copy + PartialEq + 'static>(
    choices: &[(T, &'static str)],
    value: T,
    window: &mut Window,
    cx: &mut App,
) -> Entity<OverlaySelect<T>> {
    let selected = choices
        .iter()
        .position(|(choice, _)| *choice == value)
        .map(IndexPath::new);
    let choices = choices
        .iter()
        .map(|&(value, title)| OverlayChoice(value, title))
        .collect();
    cx.new(|cx| SelectState::new(choices, selected, window, cx))
}

fn overlay_select_field<T: Copy + PartialEq + 'static>(
    label: &'static str,
    state: &Entity<OverlaySelect<T>>,
) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(Select::new(state).accessibility_label(label).w_full())
}

struct Settings {
    model: Entity<Model>,
    category: SettingsCategory,
    name: Entity<InputState>,
    ip: Entity<InputState>,
    port: Entity<InputState>,
    sound: bool,
    gestures: bool,
    triggers: Triggers,
    emote_pages: Vec<EmotePage>,
    wheel_page: usize,
    wheel_slot: usize,
    wheel_preview: Rc<RefCell<WheelPreview>>,
    swap_source: Option<WheelSlot>,
    pending_image: Option<WheelSlot>,
    recording: Option<Wheel>,
    capture_focus: FocusHandle,
    capture_paused: Arc<AtomicBool>,
    remote_overlay: bool,
    ping_size: Entity<SliderState>,
    emote_size: Entity<SliderState>,
    ping_timeout: Entity<SliderState>,
    emote_timeout: Entity<SliderState>,
    text_timeout: Entity<SliderState>,
    text_position: Entity<OverlaySelect<TextPosition>>,
    text_enter: Entity<OverlaySelect<TextMotion>>,
    text_exit: Entity<OverlaySelect<TextMotion>>,
    volume: Entity<SliderState>,
    emote_sound: bool,
    wheel_sound: bool,
    feedback: Option<(bool, String)>,
    connection_error: Option<String>,
    connecting: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for Settings {
    fn drop(&mut self) {
        self.capture_paused.store(false, Ordering::Relaxed);
    }
}

impl Settings {
    fn show_wheel_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.wheel_page = page;
        cx.notify();
    }

    fn selected_slot(&self) -> WheelSlot {
        WheelSlot {
            page: self.wheel_page,
            sector: self.wheel_slot,
        }
    }

    fn select_wheel_slot(&mut self, target: WheelSlot, cx: &mut Context<Self>) {
        if let Some(source) = self.swap_source.take() {
            self.swap_wheel_slots(source, target);
        }
        self.wheel_slot = target.sector;
        cx.notify();
    }

    fn swap_wheel_slots(&mut self, source: WheelSlot, target: WheelSlot) {
        if source == target || source.page >= self.emote_pages.len() {
            return;
        }
        if self
            .pending_image
            .is_some_and(|slot| slot == source || slot == target)
        {
            self.pending_image = None;
        }
        let emote = self.emote_pages[source.page][source.sector].take();
        let displaced = std::mem::replace(&mut self.emote_pages[target.page][target.sector], emote);
        self.emote_pages[source.page][source.sector] = displaced;
        self.feedback = None;
    }

    fn set_wheel_slot(
        &mut self,
        target: WheelSlot,
        emote: Option<WheelEmote>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_image == Some(target) {
            self.pending_image = None;
        }
        self.emote_pages[target.page][target.sector] = emote;
        self.swap_source = None;
        self.feedback = None;
        cx.notify();
    }

    fn drop_wheel_image(
        &mut self,
        target: WheelSlot,
        paths: &ExternalPaths,
        cx: &mut Context<Self>,
    ) {
        let [path] = paths.paths() else {
            self.feedback = Some((false, "请一次拖入一张图片。".into()));
            cx.notify();
            return;
        };
        let library = cx.global::<Desktop>().library.clone();
        let owner = cx.weak_entity();
        let started = library.update(cx, |library, cx| {
            library.import_image(
                Some(path.clone()),
                move |id, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        // The target follows page deletion and is cleared if the slot is edited.
                        // 删除页面时会更新目标索引，编辑目标槽位时则会取消自动填入。
                        if let (Some(slot), Some(id)) = (this.pending_image.take(), id) {
                            this.emote_pages[slot.page][slot.sector] = Some(WheelEmote::Custom(id));
                        }
                        cx.notify();
                    });
                },
                cx,
            )
        });
        if started {
            self.pending_image = Some(target);
            self.wheel_slot = target.sector;
            self.swap_source = None;
            self.feedback = None;
        } else {
            self.feedback = Some((false, "表情库正在处理图片，请稍后再拖入。".into()));
        }
        cx.notify();
    }

    fn wheel_layout(&self, cx: &Context<Self>) -> Div {
        let desktop = cx.global::<Desktop>();
        let library = desktop.library.read(cx);
        let theme = cx.theme();
        let page = self.wheel_page;
        let count = self.emote_pages.len();
        let slots = &self.emote_pages[page];
        let direction = SLOT_DIRECTIONS[self.wheel_slot];
        let selected = slots[self.wheel_slot];
        let (name, _) = wheel_emote_preview(selected, desktop, library);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("表情轮盘布局"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("拖动槽位里的表情可换位。可将表情库中的表情或本地图片拖入槽位，也可点选表情填入当前槽位。"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("previous-wheel-page")
                            .small()
                            .label("上一页")
                            .disabled(page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_wheel_page(this.wheel_page - 1, cx)
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(format!("第 {} / {} 页", page + 1, count)),
                    )
                    .child(
                        Button::new("next-wheel-page")
                            .small()
                            .label("下一页")
                            .disabled(page + 1 == count)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_wheel_page(this.wheel_page + 1, cx)
                            })),
                    )
                    .child(
                        Button::new("add-wheel-page")
                            .small()
                            .label("新增页")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.emote_pages.push([None; 8]);
                                this.feedback = None;
                                this.show_wheel_page(this.emote_pages.len() - 1, cx);
                            })),
                    )
                    .child(
                        Button::new("remove-wheel-page")
                            .small()
                            .ghost()
                            .label("移除本页")
                            .disabled(count == 1)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.emote_pages.remove(this.wheel_page);
                                this.pending_image = this
                                    .pending_image
                                    .and_then(|slot| slot.after_page_removed(this.wheel_page));
                                this.swap_source = None;
                                this.feedback = None;
                                this.show_wheel_page(
                                    this.wheel_page.min(this.emote_pages.len() - 1),
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_4()
                    .child(self.wheel_surface(cx))
                    .child(self.wheel_emote_library(selected, cx)),
            )
            .child(div().text_sm().child(if let Some(slot) = self.swap_source {
                format!(
                    "已选中第 {} 页的「{}」，请选择目标槽位。",
                    slot.page + 1,
                    SLOT_DIRECTIONS[slot.sector]
                )
            } else {
                format!("{direction} · {name}")
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("swap-wheel-slots")
                            .small()
                            .ghost()
                            .label(if self.swap_source.is_some() {
                                "取消交换"
                            } else {
                                "交换位置"
                            })
                            .disabled(selected.is_none() && self.swap_source.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.swap_source.take().is_none() {
                                    this.swap_source = Some(this.selected_slot());
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("remove-wheel-emote")
                            .small()
                            .ghost()
                            .label("移出此槽位")
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_wheel_slot(this.selected_slot(), None, cx)
                            })),
                    )
                    .child(
                        Button::new("import-wheel-emote")
                            .small()
                            .ghost()
                            .label("导入图片…")
                            .loading(library.task.is_some())
                            .on_click({
                                let library = desktop.library.clone();
                                move |_, _, cx| library.update(cx, LocalEmotes::import)
                            }),
                    ),
            )
            .when_some(library.error.clone(), |panel, error| {
                panel.child(div().text_sm().text_color(theme.danger).child(error))
            })
    }

    fn wheel_surface(&self, cx: &Context<Self>) -> Stateful<Div> {
        let desktop = cx.global::<Desktop>();
        let library = desktop.library.read(cx);
        let page = self.wheel_page;
        let slots = self.emote_pages[page];
        let wheel = WheelFrame::emote(slots, page, self.emote_pages.len());
        let selection = self.wheel_slot;
        let store = library.store.clone();
        let preview = self.wheel_preview.clone();
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let artwork = canvas(
            {
                let measured = bounds.clone();
                move |bounds, window, cx| {
                    measured.set(bounds);
                    let selected = if cx.has_active_drag()
                        && window
                            .content_mask()
                            .bounds
                            .contains(&window.mouse_position())
                    {
                        wheel_sector_at(bounds, window.mouse_position()).unwrap_or(selection)
                    } else {
                        selection
                    };
                    preview
                        .borrow_mut()
                        .image(wheel, selected, &store, window, cx)
                }
            },
            |bounds, image, window, _| {
                if let Some(image) = image {
                    let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
                }
            },
        )
        .absolute()
        .inset_0()
        .size_full();

        let slot_at = move |position| {
            wheel_sector_at(bounds.get(), position).map(|sector| WheelSlot { page, sector })
        };
        let can_drop = slot_at.clone();
        let swap_target = slot_at.clone();
        let library_target = slot_at.clone();
        let file_target = slot_at.clone();

        // One ring hit area routes drops by angle; the icon buttons supply focus and drag origins.
        // 整个环按角度分配拖放目标，图标按钮处理键盘焦点和拖动起点。
        div()
            .id("wheel-surface")
            .relative()
            .flex_1()
            .min_w_0()
            .max_w(rems(24.))
            .aspect_ratio(1.)
            .child(artwork)
            .children(slots.into_iter().enumerate().map(|(sector, emote)| {
                let preview = wheel_emote_preview(emote, desktop, library);
                self.wheel_slot_button(WheelSlot { page, sector }, preview, cx)
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if let Some(slot) = slot_at(event.position()) {
                    this.select_wheel_slot(slot, cx);
                }
            }))
            .can_drop(move |_, window, _| can_drop(window.mouse_position()).is_some())
            .on_drop(cx.listener(move |this, source: &WheelSlot, window, cx| {
                if let Some(target) = swap_target(window.mouse_position()) {
                    this.swap_source = None;
                    this.swap_wheel_slots(*source, target);
                    this.wheel_slot = target.sector;
                    cx.notify();
                }
            }))
            .on_drop(cx.listener(move |this, emote: &WheelEmote, window, cx| {
                if let Some(target) = library_target(window.mouse_position()) {
                    this.wheel_slot = target.sector;
                    this.set_wheel_slot(target, Some(*emote), cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                if let Some(target) = file_target(window.mouse_position()) {
                    this.drop_wheel_image(target, paths, cx);
                }
            }))
    }

    fn wheel_slot_button(
        &self,
        target: WheelSlot,
        (name, image): (String, Option<ImageSource>),
        cx: &Context<Self>,
    ) -> BaseButton {
        let sector = target.sector;
        let direction = SLOT_DIRECTIONS[sector];
        let label = format!("{direction} · {name}");
        let [x, y] = platform::slot_center(sector);
        let edge = 0.12;
        let theme = cx.theme();
        let focus_ring = theme.ring;
        BaseButton::new(("wheel-slot", sector))
            .absolute()
            .left(relative(x - edge / 2.))
            .top(relative(y - edge / 2.))
            .w(relative(edge))
            .h(relative(edge))
            .rounded_full()
            .border_1()
            .border_color(theme.transparent)
            .focus_visible(move |style| style.border_color(focus_ring))
            .aria_toggled(if self.wheel_slot == sector {
                Toggled::True
            } else {
                Toggled::False
            })
            .accessibility_label(label.clone())
            .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.select_wheel_slot(target, cx);
            }))
            .when_some(image, |button, image| {
                draggable_emote(button, target, image)
            })
    }

    fn wheel_emote_library(&self, selected: Option<WheelEmote>, cx: &Context<Self>) -> Div {
        let desktop = cx.global::<Desktop>();
        let library = desktop.library.read(cx);
        let target = self.selected_slot();
        let button = |emote| {
            let (name, image) = wheel_emote_preview(Some(emote), desktop, library);
            let id: ElementId = match emote {
                WheelEmote::BuiltIn(index) => ("built-in", index).into(),
                WheelEmote::Custom(id) => SharedString::from(id.to_string()).into(),
            };
            Button::new(id)
                .ghost()
                .size_8()
                .p_0()
                .selected(selected == Some(emote))
                .accessibility_label(name.clone())
                .tooltip(name)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_wheel_slot(target, Some(emote), cx)),
                )
                .when_some(image, |button, image| {
                    draggable_emote(
                        button.child(img(image.clone()).size_6().object_fit(ObjectFit::Contain)),
                        emote,
                        image,
                    )
                })
        };
        let preset = (0..EMOTES.len()).map(|index| button(WheelEmote::BuiltIn(index)));
        let custom = library
            .entries
            .iter()
            .map(|emote| button(WheelEmote::Custom(emote.id())));
        div()
            .w(rems(10.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("表情库"),
            )
            .child(
                div()
                    .id("wheel-library")
                    .max_h(rems(10.))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .children(preset.chain(custom)),
                    )
                    .overflow_y_scrollbar(),
            )
    }

    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        close_to_tray(window, cx);
        let model = cx.global::<Desktop>().model.clone();
        let Profile {
            name,
            endpoint,
            sound,
            gestures,
            triggers,
            remote_overlay,
            overlay,
            emote_pages,
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
        let ping_size = cx.new(|_| setting_slider(PING_SIZE, 1, overlay.ping_size));
        let emote_size = cx.new(|_| setting_slider(EMOTE_SIZE, 1, overlay.emote_size));
        let ping_timeout =
            cx.new(|_| setting_slider(ANIMATION_TIMEOUT, 100, overlay.ping_timeout_ms));
        let emote_timeout =
            cx.new(|_| setting_slider(ANIMATION_TIMEOUT, 100, overlay.emote_timeout_ms));
        let text_timeout =
            cx.new(|_| setting_slider(ANIMATION_TIMEOUT, 100, overlay.text.timeout_ms));
        let text_position = overlay_select(&TEXT_POSITIONS, overlay.text.position, window, cx);
        let text_enter = overlay_select(&TEXT_ENTRANCE_MOTIONS, overlay.text.enter, window, cx);
        let text_exit = overlay_select(&TEXT_EXIT_MOTIONS, overlay.text.exit, window, cx);
        let volume = cx.new(|_| setting_slider(SOUND_VOLUME, 1, overlay.volume));
        let capture_focus = cx.focus_handle();
        let library = cx.global::<Desktop>().library.clone();
        let mut subscriptions = vec![
            cx.on_blur(&capture_focus, window, |this: &mut Self, _, cx| {
                this.end_capture(cx)
            }),
            cx.observe_window_visibility(window, |this: &mut Self, visibility, _, cx| {
                if !visibility.is_visible() {
                    this.end_capture(cx);
                }
            }),
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if !window.is_window_active() {
                    this.end_capture(cx);
                }
            }),
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&model, |this, _, added: &WheelEmoteAdded, cx| {
                // Apply the same addition to the settings draft without overwriting local edits.
                // 在设置草稿中同步追加表情，保留用户尚未保存的调整。
                if append_wheel_emote(&mut this.emote_pages, added.0) {
                    cx.notify();
                }
            }),
            cx.observe(&library, |_, _, cx| cx.notify()),
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
        subscriptions.extend(
            [
                &ping_size,
                &emote_size,
                &ping_timeout,
                &emote_timeout,
                &text_timeout,
                &volume,
            ]
            .into_iter()
            .map(|slider| cx.observe(slider, |_, _, cx| cx.notify())),
        );
        Self {
            model,
            category: SettingsCategory::General,
            name,
            ip,
            port,
            sound,
            gestures,
            triggers,
            emote_pages,
            wheel_page: 0,
            wheel_slot: 0,
            wheel_preview: Rc::default(),
            swap_source: None,
            pending_image: None,
            recording: None,
            capture_focus,
            capture_paused: cx.global::<Desktop>().capture_paused.clone(),
            remote_overlay,
            ping_size,
            emote_size,
            ping_timeout,
            emote_timeout,
            text_timeout,
            text_position,
            text_enter,
            text_exit,
            volume,
            emote_sound: overlay.emote_sound,
            wheel_sound: overlay.wheel_sound,
            feedback: None,
            connection_error: None,
            connecting: None,
            _subscriptions: subscriptions,
        }
    }

    fn end_capture(&mut self, cx: &mut Context<Self>) {
        if self.recording.take().is_some() {
            self.capture_paused.store(false, Ordering::Relaxed);
            cx.notify();
        }
    }

    fn record_key(&mut self, key: TriggerKey, cx: &mut Context<Self>) {
        match self.recording {
            Some(Wheel::Ping) => self.triggers.ping_key = key,
            Some(Wheel::Emote) => self.triggers.emote_key = key,
            None => return,
        }
        self.feedback = None;
        self.end_capture(cx);
    }

    fn trigger_row(&self, wheel: Wheel, cx: &Context<Self>) -> impl IntoElement + use<> {
        let (id, label, enabled, selected) = match wheel {
            Wheel::Ping => (
                "ping-trigger",
                "信号快捷键",
                self.triggers.ping_enabled,
                &self.triggers.ping_key,
            ),
            Wheel::Emote => (
                "emote-trigger",
                "表情快捷键",
                self.triggers.emote_enabled,
                &self.triggers.emote_key,
            ),
        };
        let row = div().flex().flex_wrap().items_center().gap_2().child(
            Switch::new(id)
                .label(label)
                .checked(enabled)
                .disabled(!self.gestures)
                .on_change(cx.listener(move |this, value, _, cx| {
                    match wheel {
                        Wheel::Ping => this.triggers.ping_enabled = *value,
                        Wheel::Emote => this.triggers.emote_enabled = *value,
                    }
                    cx.notify();
                })),
        );
        let listening = self.recording == Some(wheel);
        row.child(
            Button::new(SharedString::from(format!("{id}-record")))
                .label(if listening {
                    "请按键或鼠标侧键 · Esc 取消".to_owned()
                } else {
                    format!("{} · 重新录入", selected.label())
                })
                .when(listening, |button| button.primary())
                .disabled(!self.gestures || !enabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.recording = Some(wheel);
                    this.capture_paused.store(true, Ordering::Relaxed);
                    this.capture_focus.focus(window, cx);
                    cx.notify();
                })),
        )
    }

    fn overlay_settings(&self, cx: &App) -> OverlaySettings {
        OverlaySettings {
            ping_size: self.ping_size.read(cx).value().start().round() as u32,
            emote_size: self.emote_size.read(cx).value().start().round() as u32,
            ping_timeout_ms: self.ping_timeout.read(cx).value().start().round() as u32,
            emote_timeout_ms: self.emote_timeout.read(cx).value().start().round() as u32,
            volume: self.volume.read(cx).value().start().round() as u32,
            emote_sound: self.emote_sound,
            wheel_sound: self.wheel_sound,
            text: self.text_overlay_settings(cx),
        }
    }

    fn text_overlay_settings(&self, cx: &App) -> TextOverlaySettings {
        TextOverlaySettings {
            timeout_ms: self.text_timeout.read(cx).value().start().round() as u32,
            position: self
                .text_position
                .read(cx)
                .selected_value()
                .copied()
                .unwrap_or_default(),
            enter: self
                .text_enter
                .read(cx)
                .selected_value()
                .copied()
                .unwrap_or_default(),
            exit: self
                .text_exit
                .read(cx)
                .selected_value()
                .copied()
                .unwrap_or_default(),
        }
    }

    fn preview(&self, kind: Wheel, cx: &mut App) {
        let settings = self.overlay_settings(cx).with_sound(self.sound);
        let body = match kind {
            Wheel::Ping => Body::Ping {
                index: 0,
                location: None,
            },
            Wheel::Emote => Body::Emote {
                index: 0,
                location: None,
            },
        };
        cx.update_global::<Desktop, _>(|desktop, cx| {
            let model = desktop.model.read(cx);
            desktop.overlay.play(
                &body,
                &model.profile.name,
                AnimationPosition::Relative(Location::CENTER),
                &model.emotes(),
                settings,
                cx,
            );
        });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let mut profile = self.model.read(cx).profile.clone();
        profile.name = self.name.read(cx).value().trim().to_owned();
        profile.sound = self.sound;
        let input_changed = profile.gestures != self.gestures || profile.triggers != self.triggers;
        let wheel_changed = profile.emote_pages != self.emote_pages;
        profile.gestures = self.gestures;
        profile.triggers = self.triggers.clone();
        profile.emote_pages.clone_from(&self.emote_pages);
        profile.remote_overlay = self.remote_overlay;
        profile.overlay = self.overlay_settings(cx);
        let result = self.model.update(cx, |model, cx| {
            model.apply(profile.clone())?;
            cx.notify();
            Ok::<_, anyhow::Error>(())
        });
        self.feedback = Some(match result {
            Ok(()) => {
                if wheel_changed {
                    refresh_wheel_posters(cx);
                }
                if input_changed {
                    cx.update_global::<Desktop, _>(|desktop, _| {
                        desktop.monitor = None;
                        desktop.overlay.hide();
                    });
                    self.retry_input(cx);
                    if !self.model.read(cx).input_ready {
                        self.feedback =
                            Some((false, "设置已保存，全局监听未启动，请重新检测。".into()));
                        cx.notify();
                        return;
                    }
                }
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
        let model = self.model.read(cx);
        let name = model.profile.name.clone();
        let emotes = model.emotes();
        self.connecting = Some(cx.spawn(async move |weak, cx| {
            let room = Room::open(config, name, incoming, emotes).await;
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
        let ready = cx.update_global::<Desktop, _>(|desktop, cx| {
            if desktop.monitor.is_none() {
                desktop.monitor = start_input(
                    &desktop.model.read(cx).profile.clone(),
                    desktop.capture_paused.clone(),
                    cx,
                );
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

fn wheel_emote_preview(
    emote: Option<WheelEmote>,
    desktop: &Desktop,
    library: &LocalEmotes,
) -> (String, Option<ImageSource>) {
    match emote {
        Some(WheelEmote::BuiltIn(index)) => (
            EMOTES[index].name.into(),
            Some(desktop.emotes[index].clone().into()),
        ),
        Some(WheelEmote::Custom(id)) => (
            library
                .entries
                .iter()
                .find(|emote| emote.id() == id)
                .map_or("自定义表情", LibraryEmote::name)
                .into(),
            Some(library.store.poster(id).into()),
        ),
        None => ("空槽位".into(), None),
    }
}

fn setting_slider(range: RangeInclusive<u32>, step: u32, value: u32) -> SliderState {
    // Each bound setter clamps the current value, so expand the upper bound first.
    // 每次设置边界都会限制当前值，因此先扩大上限。
    SliderState::new()
        .max(*range.end() as f32)
        .min(*range.start() as f32)
        .step(step as f32)
        .default_value(value as f32)
}

fn slider_field(label: &'static str, value: String, slider: Slider) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .justify_between()
                .gap_2()
                .text_sm()
                .child(label)
                .child(value),
        )
        .child(slider)
}

impl Settings {
    fn room_settings(&self, cx: &Context<Self>) -> Div {
        let model = self.model.read(cx);
        let connection = model.connection();
        let busy = self.connecting.is_some();
        let room_error = self
            .connection_error
            .clone()
            .or_else(|| (!busy).then(|| model.room_error.clone()).flatten());
        let theme = cx.theme();
        div().flex().flex_col().gap_2()
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
                    .child(div().text_sm().text_color(theme.muted_foreground).child(format!("{} · {}", if matches!(config, RoomConfig::Host(_)) { "正在监听" } else { "已连接" }, config.address())))
                    .child(self.online_users(cx)))
                .when_some(room_error, |group, error| group
                    .child(div().text_sm().text_color(theme.danger).child(error)))
    }

    fn online_users(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let section = div()
            .id("online-users")
            .mt_3()
            .pt_4()
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap_2();
        match self.model.read(cx).room_members() {
            Some((local_id, members)) if !members.is_empty() => section
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("在线用户（{}）", members.len())),
                )
                .children(members.iter().enumerate().map(|(index, member)| {
                    let role = match (index == 0, member.id() == local_id) {
                        (true, true) => Some("主机 · 你"),
                        (true, false) => Some("主机"),
                        (false, true) => Some("你"),
                        (false, false) => None,
                    };
                    div()
                        .id(member.id())
                        .flex()
                        .items_start()
                        .gap_3()
                        .text_sm()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .child(member.name().unwrap_or("旧版访客").to_owned()),
                        )
                        .when_some(role, |row, role| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(theme.muted_foreground)
                                    .child(role),
                            )
                        })
                })),
            members => section
                .child(div().font_weight(FontWeight::SEMIBOLD).child("在线用户"))
                .child(div().text_sm().text_color(theme.muted_foreground).child(
                    if members.is_some() {
                        "正在获取在线用户…"
                    } else {
                        "请更新主机版本以查看在线用户。"
                    },
                )),
        }
    }

    fn wheel_shortcuts(&self, cx: &Context<Self>) -> Div {
        let model = self.model.read(cx);
        let active = model.input_ready;
        #[cfg(target_os = "macos")]
        let capture_access = model.profile.gestures && model.profile.triggers.captures_mouse();
        #[cfg(target_os = "macos")]
        let permission_hint = if capture_access {
            "轮盘拦截鼠标事件需要「隐私与安全性 → 辅助功能」权限。授权后请重新检测。"
        } else {
            "请在系统设置的「隐私与安全性 → 输入监控」中授权，再重试。"
        };
        #[cfg(not(target_os = "macos"))]
        let permission_hint = "全局输入监听未启动。";
        let theme = cx.theme();
        div().flex().flex_col().gap_3().border_t_1().border_color(theme.border).pt_4()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("轮盘快捷键"))
                .child(div().text_sm().text_color(theme.muted_foreground).child("键盘触发时，按住快捷键并用左键拖动。鼠标侧键可直接按住拖动，松开确认。拖回中心可取消，表情轮盘打开时可用滚轮翻页。"))
                .child(Switch::new("gestures").label("启用全局轮盘").checked(self.gestures)
                    .on_change(cx.listener(|this, value, _, cx| { this.gestures = *value; cx.notify(); })))
                .child(self.trigger_row(Wheel::Ping, cx))
                .child(self.trigger_row(Wheel::Emote, cx))
                .child(Switch::new("quick-ping").label("单击发送普通标记（侧键绑定时单击侧键）").checked(self.triggers.click_ping)
                    .disabled(!self.gestures || !self.triggers.ping_enabled)
                    .on_change(cx.listener(|this, value, _, cx| { this.triggers.click_ping = *value; cx.notify(); })))
                .when(!active, |group| group
                    .child(div().text_sm().text_color(theme.muted_foreground).child(permission_hint))
                    .child({
                        let row = div().flex().flex_wrap().gap_2();
                        #[cfg(target_os = "macos")]
                        let row = row.child(Button::new("permission").label(if capture_access { "请求辅助功能权限…" } else { "请求输入监控权限…" }).on_click(move |_, _, _| platform::request_input_access(capture_access)));
                        row.child(Button::new("retry").ghost().label("重新检测").on_click(cx.listener(|this, _, _, cx| this.retry_input(cx))))
                    }))
    }

    fn animation_settings(&self, cx: &Context<Self>) -> Div {
        let incoming_muted = self.model.read(cx).profile.incoming_muted();
        let theme = cx.theme();
        let overlay = self.overlay_settings(cx);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("信号"))
                    .child(
                        Button::new("preview-ping")
                            .label("预览信号")
                            .on_click(cx.listener(|this, _, _, cx| this.preview(Wheel::Ping, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_5()
                    .child(slider_field(
                        "信号大小",
                        format!("{} px", overlay.ping_size),
                        Slider::new(&self.ping_size),
                    ))
                    .child(slider_field(
                        "信号停留时长",
                        format!("{:.1} 秒", overlay.ping_timeout_ms as f32 / 1000.),
                        Slider::new(&self.ping_timeout),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("表情"))
                    .child(
                        Button::new("preview-emote")
                            .label("预览表情")
                            .on_click(cx.listener(|this, _, _, cx| this.preview(Wheel::Emote, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_5()
                    .child(slider_field(
                        "表情大小",
                        format!("{} px", overlay.emote_size),
                        Slider::new(&self.emote_size),
                    ))
                    .child(slider_field(
                        "表情停留时长",
                        format!("{:.1} 秒", overlay.emote_timeout_ms as f32 / 1000.),
                        Slider::new(&self.emote_timeout),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("文字浮层"))
                    .child(
                        Button::new("preview-text-overlay")
                            .label("预览文字")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let settings = this.text_overlay_settings(cx);
                                cx.update_global::<Desktop, _>(|desktop, cx| {
                                    desktop.overlay.play_text(
                                        "这是一条文字消息预览。",
                                        settings,
                                        TextEffect::Default,
                                        cx,
                                    );
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(overlay_select_field("显示位置", &self.text_position))
                    .child(overlay_select_field("默认入场", &self.text_enter))
                    .child(overlay_select_field("默认退场", &self.text_exit)),
            )
            .child(slider_field(
                "文字停留时长",
                format!("{:.1} 秒", overlay.text.timeout_ms as f32 / 1000.),
                Slider::new(&self.text_timeout),
            ))
            .child(
                Switch::new("room-overlay")
                    .label("在桌面显示收到的房间消息")
                    .checked(self.remote_overlay)
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.remote_overlay = *value;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        Switch::new("incoming-mute")
                            .label("静音")
                            .checked(incoming_muted)
                            .on_change(cx.listener(|this, value, _, cx| {
                                this.feedback = set_incoming_muted(*value, cx)
                                    .err()
                                    .map(|error| (false, format!("无法切换静音：{error:#}")));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("开启后立即生效，收到的内容仅显示在聊天和历史记录中。"),
                    ),
            )
    }

    fn sound_settings(&self, cx: &Context<Self>) -> Div {
        let overlay = self.overlay_settings(cx);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                Switch::new("sound")
                    .label("播放音效")
                    .checked(self.sound)
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.sound = *value;
                        cx.notify();
                    })),
            )
            .child(slider_field(
                "音量",
                format!("{}%", overlay.volume),
                Slider::new(&self.volume).disabled(!self.sound),
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_5()
                    .child(
                        Switch::new("emote-sound")
                            .label("表情音效")
                            .checked(self.emote_sound)
                            .disabled(!self.sound)
                            .on_change(cx.listener(|this, value, _, cx| {
                                this.emote_sound = *value;
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new("wheel-sound")
                            .label("轮盘提示音")
                            .checked(self.wheel_sound)
                            .disabled(!self.sound)
                            .on_change(cx.listener(|this, value, _, cx| {
                                this.wheel_sound = *value;
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sidebar = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_32()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .children(SettingsCategory::ALL.map(|category| {
                Button::new(("settings-category", category as usize))
                    .ghost()
                    .selected(self.category == category)
                    .toggled(self.category == category)
                    .accessibility_label(category.label())
                    .child(div().w_full().text_left().child(category.label()))
                    .w_full()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.end_capture(cx);
                        if this.category != category {
                            this.swap_source = None;
                        }
                        this.category = category;
                        cx.notify();
                    }))
            }));
        let panel = match self.category {
            SettingsCategory::General => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().child("昵称"))
                .child(Input::new(&self.name).id("settings-name")),
            SettingsCategory::Room => self.room_settings(cx),
            SettingsCategory::Wheel => div()
                .flex()
                .flex_col()
                .gap_5()
                .child(self.wheel_layout(cx))
                .child(self.wheel_shortcuts(cx)),
            SettingsCategory::Animation => self.animation_settings(cx),
            SettingsCategory::Sound => self.sound_settings(cx),
        };
        let content = div().flex().flex_1().min_h_0().child(sidebar).child(
            div()
                .id(("settings-scroll", self.category as usize))
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .p_5()
                        .flex()
                        .flex_col()
                        .gap_5()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(self.category.label()),
                        )
                        .child(panel),
                )
                .overflow_y_scrollbar(),
        );

        div()
            .track_focus(&self.capture_focus)
            .when(self.recording.is_some(), |root| {
                root.capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    if event.is_held {
                        return;
                    }
                    let stroke = &event.keystroke;
                    if stroke.key == "escape" {
                        this.end_capture(cx);
                    } else if let Some(code) = platform::capture_key_code() {
                        this.record_key(
                            TriggerKey::Keyboard {
                                code,
                                label: stroke.key.to_uppercase(),
                            },
                            cx,
                        );
                    } else {
                        this.feedback =
                            Some((false, "无法识别此键，请按另一个键或鼠标侧键。".into()));
                        cx.notify();
                    }
                }))
                .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                    let key = match (event.alt, event.control, event.shift, event.platform) {
                        (true, false, false, false) => TriggerKey::Alt,
                        (false, true, false, false) => TriggerKey::Ctrl,
                        (false, false, true, false) => TriggerKey::Shift,
                        (false, false, false, true) => TriggerKey::Super,
                        _ => return,
                    };
                    this.record_key(key, cx);
                }))
                .capture_any_mouse_down(cx.listener(
                    |this, event: &MouseDownEvent, window, cx| {
                        let key = match event.button {
                            MouseButton::Navigate(NavigationDirection::Back) => TriggerKey::Mouse4,
                            MouseButton::Navigate(NavigationDirection::Forward) => {
                                TriggerKey::Mouse5
                            }
                            _ => {
                                this.end_capture(cx);
                                return;
                            }
                        };
                        window.prevent_default();
                        cx.stop_propagation();
                        this.record_key(key, cx);
                    },
                ))
            })
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(content)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .px_5()
                    .py_3()
                    .flex_shrink_0()
                    .child(
                        div().flex_1().text_sm().when_some(
                            self.feedback.clone().or_else(|| {
                                self.pending_image.map(|_| (true, "正在导入图片…".into()))
                            }),
                            |label, (ok, message)| {
                                label
                                    .text_color(if ok {
                                        theme.muted_foreground
                                    } else {
                                        theme.danger
                                    })
                                    .child(message)
                            },
                        ),
                    )
                    .child(
                        Button::new("close-settings")
                            .outline()
                            .label("关闭")
                            .on_click(|_, window, cx| close_window(window, cx)),
                    )
                    .child(
                        Button::new("save")
                            .primary()
                            .label("保存设置")
                            .disabled(self.pending_image.is_some())
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use gpui_kit::base::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext, QuitMode, TestAppContext, WindowVisibility, px, size,
    };
    use uuid::Uuid;

    use super::{
        ANIMATION_TIMEOUT, Desktop, Settings, SettingsCategory, WheelSlot, open_chat,
        open_settings, register_window_actions, setting_slider, shortcut,
    };
    use crate::model::Model;

    fn settings_desktop(cx: &mut TestAppContext) -> (AnyWindowHandle, AnyWindowHandle) {
        // The production emote library and wheel previews finish on smol workers.
        // 实际的表情库与轮盘预览由 smol 工作线程加载。
        cx.dispatcher.allow_parking();
        let data = std::env::temp_dir().join(format!("pingpingping-settings-{}", Uuid::new_v4()));
        let model = Model::load(data.clone()).unwrap();
        cx.on_quit(move || fs::remove_dir_all(data).unwrap());
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_quit_mode(QuitMode::Explicit);
            let desktop = Desktop::new(
                model,
                async_channel::bounded(8).0,
                Arc::new(AtomicBool::new(false)),
                cx,
            );
            cx.set_global(desktop);
            register_window_actions(cx);
            open_chat(cx);
            open_settings(cx);
            let desktop = cx.global::<Desktop>();
            let chat = desktop.chat.unwrap();
            let settings = desktop.settings.unwrap();
            settings
                .update(cx, |_, window, _| window.activate_window())
                .unwrap();
            (chat, settings)
        })
    }

    fn start_shortcut_recording(
        handle: AnyWindowHandle,
        cx: &mut TestAppContext,
    ) -> Arc<AtomicBool> {
        cx.simulate_window_resize(handle, size(px(800.), px(1500.)));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(("settings-category", SettingsCategory::Wheel as usize), cx);
            window.click("ping-trigger-record", cx);
            let paused = cx.global::<Desktop>().capture_paused.clone();
            assert!(paused.load(Ordering::Relaxed));
            paused
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn close_shortcut_dismisses_settings_during_shortcut_recording(cx: &mut TestAppContext) {
        let (chat, settings) = settings_desktop(cx);
        let paused = start_shortcut_recording(settings, cx);
        cx.simulate_keystrokes(settings, &shortcut("w"));

        assert_eq!(cx.windows(), vec![chat]);
        assert!(!paused.load(Ordering::Relaxed));
    }

    #[gpui_kit::test]
    fn settings_close_button_allows_reopening_when_the_tray_is_unavailable(
        cx: &mut TestAppContext,
    ) {
        let (chat, settings) = settings_desktop(cx);
        cx.update_window(settings, |_, window, cx| {
            window.render_frame(cx);
            window.click("settings-name", cx);
            window.press(&shortcut("a"), cx);
            window.input("未保存的昵称", cx);
            assert_eq!(window.find("settings-name").value(), Some("未保存的昵称"));
            window.click("close-settings", cx);
        })
        .unwrap();
        assert_eq!(cx.windows(), vec![chat]);

        cx.update_window(chat, |_, window, cx| {
            window.render_frame(cx);
            window.click("settings", cx);
        })
        .unwrap();
        let reopened = cx.update(|cx| cx.global::<Desktop>().settings.unwrap());
        assert_ne!(reopened, settings);
        cx.update_window(reopened, |_, window, cx| {
            window.render_frame(cx);
            let saved_name = &cx.global::<Desktop>().model.read(cx).profile.name;
            assert_ne!(saved_name, "未保存的昵称");
            assert_eq!(
                window.find("settings-name").value(),
                Some(saved_name.as_str())
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn hiding_settings_cancels_shortcut_recording_and_reopening_keeps_the_view(
        cx: &mut TestAppContext,
    ) {
        let (_, settings) = settings_desktop(cx);
        let paused = start_shortcut_recording(settings, cx);
        cx.simulate_window_visibility_change(settings, WindowVisibility::Hidden);
        assert!(!paused.load(Ordering::Relaxed));

        cx.update(open_settings);
        assert_eq!(
            cx.update(|cx| cx.global::<Desktop>().settings),
            Some(settings)
        );
        cx.update_window(settings, |_, window, cx| {
            let settings = Root::read(window, cx)
                .view()
                .clone()
                .downcast::<Settings>()
                .unwrap();
            let settings = settings.read(cx);
            assert!(settings.category == SettingsCategory::Wheel);
            assert!(settings.recording.is_none());
        })
        .unwrap();
    }

    #[test]
    fn deleting_a_page_preserves_the_target_of_a_pending_wheel_drop() {
        let target = WheelSlot { page: 2, sector: 5 };
        assert!(target.after_page_removed(0) == Some(WheelSlot { page: 1, ..target }));
        assert!(target.after_page_removed(2).is_none());
        assert!(target.after_page_removed(3) == Some(target));
    }

    #[test]
    fn timeout_slider_accepts_the_saved_millisecond_range() {
        let state = setting_slider(ANIMATION_TIMEOUT, 100, 3200);
        assert_eq!((state.min_value(), state.max_value()), (1000.0, 8000.0));
        assert_eq!(state.value().start(), 3200.0);
    }
}
