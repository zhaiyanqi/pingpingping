//! Borderless picker for sending effects at the mouse release point.
//! 在鼠标松手的位置发送动效的无边框选择面板。

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Selectable, Sizable,
    button::{Button, ButtonVariants},
    scroll::ScrollableElement,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{Desktop, LocalEmotes};
use crate::assets::{EMOTES, PINGS};
use crate::gesture::{Position, Wheel};
use crate::model::{Body, TextEffect, WheelEmote};
use crate::platform::{self, AnimationPosition};

struct OpenPalette(AnyWindowHandle);

impl Global for OpenPalette {}

pub(super) fn is_open(cx: &App) -> bool {
    cx.has_global::<OpenPalette>()
}

pub(super) fn restore(cx: &mut App) {
    if cx
        .global::<Desktop>()
        .model
        .read(cx)
        .profile
        .palette_visible
        && let Err(error) = open(cx)
    {
        eprintln!("无法打开快捷浮窗：{error:#}");
    }
}

pub(super) fn toggle(cx: &mut App) {
    let result = if let Some(&OpenPalette(handle)) = cx.try_global::<OpenPalette>() {
        handle.update(cx, |_, window, cx| close(window, cx))
    } else {
        open(cx)
    };
    if let Err(error) = result {
        if let Some(tray) = &cx.global::<Desktop>().tray {
            tray.set_palette_visible(is_open(cx));
        }
        eprintln!("无法切换快捷浮窗：{error:#}");
    }
}

fn open(cx: &mut App) -> anyhow::Result<()> {
    let pinned = cx.global::<Desktop>().model.read(cx).profile.palette_pinned;
    let size = size(px(288.), px(272.));
    let bounds = cx.primary_display().map(|display| {
        let area = display.visible_bounds();
        WindowBounds::Windowed(Bounds::new(
            point(
                (area.right() - size.width - px(16.)).max(area.left()),
                area.top() + px(16.),
            ),
            size,
        ))
    });
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: bounds,
            kind: WindowKind::PopUp,
            titlebar: None,
            focus: false,
            is_resizable: false,
            is_minimizable: false,
            app_owns_titlebar_drag: true,
            ..Default::default()
        },
        cx,
        |window, cx| {
            window.on_window_should_close(cx, |_, cx| {
                if is_open(cx) {
                    record_close(cx);
                }
                true
            });
            cx.new(|cx| Palette::new(pinned, cx))
        },
    )?;
    let configured = handle.update(cx, |_, window, _| {
        if platform::configure_palette(window, pinned) {
            true
        } else {
            window.remove_window();
            false
        }
    })?;
    anyhow::ensure!(configured, "无法配置快捷浮窗");
    cx.set_global(OpenPalette(handle));
    remember(true, cx);
    Ok(())
}

fn close(window: &mut Window, cx: &mut App) {
    if is_open(cx) {
        record_close(cx);
    }
    window.remove_window();
}

fn record_close(cx: &mut App) {
    cx.remove_global::<OpenPalette>();
    remember(false, cx);
}

fn remember(visible: bool, cx: &mut App) {
    let desktop = cx.global::<Desktop>();
    if let Some(tray) = &desktop.tray {
        tray.set_palette_visible(visible);
    }
    let model = desktop.model.clone();
    model.update(cx, |model, cx| {
        if model.profile.palette_visible != visible {
            let mut profile = model.profile.clone();
            profile.palette_visible = visible;
            if let Err(error) = model.apply(profile) {
                eprintln!("无法保存快捷浮窗状态：{error:#}");
            }
        }
        cx.notify();
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Ping(usize),
    Emote(WheelEmote),
}

impl Item {
    fn body(self) -> Body {
        match self {
            Self::Ping(index) => Body::Ping {
                index,
                location: None,
            },
            Self::Emote(WheelEmote::BuiltIn(index)) => Body::Emote {
                index,
                location: None,
            },
            Self::Emote(WheelEmote::Custom(id)) => Body::CustomEmote(id),
        }
    }
}

struct Palette {
    library: Entity<LocalEmotes>,
    _library_subscription: Subscription,
    pinned: bool,
    category: Wheel,
    dragging: Option<Item>,
    #[cfg(target_os = "windows")]
    release_task: Option<Task<()>>,
    error: Option<String>,
}

impl Palette {
    fn new(pinned: bool, cx: &mut Context<Self>) -> Self {
        let library = cx.global::<Desktop>().library.clone();
        let library_subscription = cx.observe(&library, |_, _, cx| cx.notify());
        Self {
            library,
            _library_subscription: library_subscription,
            pinned,
            category: Wheel::Emote,
            dragging: None,
            #[cfg(target_os = "windows")]
            release_task: None,
            error: None,
        }
    }

    fn toggle_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = !self.pinned;
        if !platform::set_palette_pinned(window, pinned) {
            self.error = Some("无法切换浮窗置顶状态。".into());
        } else {
            self.pinned = pinned;
            let model = cx.global::<Desktop>().model.clone();
            self.error = model
                .update(cx, |model, _| {
                    let mut profile = model.profile.clone();
                    profile.palette_pinned = pinned;
                    model.apply(profile)
                })
                .err()
                .map(|error| format!("无法保存置顶状态：{error:#}"));
        }
        cx.notify();
    }

    fn begin_drag(&mut self, item: Item, _window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        #[cfg(target_os = "windows")]
        {
            let Some((monitor, _)) = &cx.global::<Desktop>().monitor else {
                self.error = Some("鼠标监听不可用，请在设置中重试。".into());
                cx.notify();
                return;
            };
            let Some(release) = monitor.watch_palette_release() else {
                return;
            };
            self.release_task = Some(cx.spawn_in(_window, async move |owner, cx| {
                let position = release.recv().await.ok();
                let _ = owner.update_in(cx, |this, window, cx| {
                    let position =
                        position.and_then(|position| platform::palette_drop(window, position));
                    this.finish_drag(position, cx);
                });
            }));
        }
        self.dragging = Some(item);
        cx.notify();
    }

    fn finish_drag(&mut self, position: Option<Position>, cx: &mut Context<Self>) {
        let Some(item) = self.dragging.take() else {
            return;
        };
        if let Some(position) = position {
            let body = item.body();
            let location = platform::normalized(position);
            let result = cx
                .global::<Desktop>()
                .model
                .clone()
                .update(cx, |model, cx| {
                    model.send(body.clone(), TextEffect::Default, Some(location))?;
                    cx.notify();
                    Ok::<_, anyhow::Error>(())
                });
            match result {
                Ok(()) => {
                    cx.update_global::<Desktop, _>(|desktop, cx| {
                        desktop.play(
                            &body,
                            TextEffect::Default,
                            None,
                            AnimationPosition::Local(position),
                            cx,
                        );
                    });
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
        }
        cx.notify();
    }

    #[cfg(target_os = "macos")]
    fn mouse_released(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging.is_some() {
            let position = platform::palette_drop(window, event.position);
            cx.defer_in(window, move |this, _, cx| this.finish_drag(position, cx));
        }
    }

    fn tile(
        &self,
        id: impl Into<ElementId>,
        item: Item,
        name: impl Into<SharedString>,
        image: impl Into<ImageSource>,
        cx: &Context<Self>,
    ) -> Button {
        let name = name.into();
        let mut button = Button::new(id)
            .ghost()
            .small()
            .compact()
            .size_9()
            .selected(self.dragging == Some(item))
            .cursor_grab()
            .accessibility_label(name.clone())
            .when(self.dragging.is_none(), |button| button.tooltip(name))
            .child(img(image).size_7().object_fit(ObjectFit::Contain));
        button.interactivity().on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| this.begin_drag(item, window, cx)),
        );
        button
    }

    fn items(&self, cx: &Context<Self>) -> Div {
        let desktop = cx.global::<Desktop>();
        let grid = || div().flex().flex_wrap().gap_1();
        match self.category {
            Wheel::Ping => grid().children(PINGS.iter().zip(&desktop.pings).enumerate().map(
                |(index, (clip, image))| {
                    self.tile(
                        ("ping", index),
                        Item::Ping(index),
                        clip.name,
                        image.clone(),
                        cx,
                    )
                },
            )),
            Wheel::Emote => {
                let library = self.library.read(cx);
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        grid().children(EMOTES.iter().zip(&desktop.emotes).enumerate().map(
                            |(index, (clip, image))| {
                                self.tile(
                                    ("emote", index),
                                    Item::Emote(WheelEmote::BuiltIn(index)),
                                    clip.name,
                                    image.clone(),
                                    cx,
                                )
                            },
                        )),
                    )
                    .when(!library.entries.is_empty(), |content| {
                        content
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("自定义表情"),
                            )
                            .child(grid().children(library.entries.iter().map(|emote| {
                                self.tile(
                                    SharedString::from(emote.id().to_string()),
                                    Item::Emote(WheelEmote::Custom(emote.id())),
                                    emote.name().to_owned(),
                                    library.store.poster(emote.id()),
                                    cx,
                                )
                            })))
                    })
            }
        }
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = div()
            .flex()
            .flex_1()
            .h_7()
            .items_center()
            .cursor_move()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child("快捷浮窗");
        #[cfg(target_os = "windows")]
        let title = title.window_control_area(WindowControlArea::Drag);
        #[cfg(target_os = "macos")]
        let title =
            title.on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move());

        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .child(title)
                    .child(
                        Button::new("pin-palette")
                            .ghost()
                            .small()
                            .icon(if self.pinned {
                                IconName::Pin
                            } else {
                                IconName::PinOff
                            })
                            .selected(self.pinned)
                            .toggled(self.pinned)
                            .accessibility_label("浮窗置顶")
                            .tooltip(if self.pinned {
                                "取消置顶"
                            } else {
                                "置顶"
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_pin(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("close-palette")
                            .ghost()
                            .small()
                            .icon(IconName::X)
                            .accessibility_label("关闭快捷浮窗")
                            .tooltip("关闭快捷浮窗")
                            .on_click(|_, window, cx| close(window, cx)),
                    ),
            )
            .child(div().flex().flex_shrink_0().px_3().py_2().gap_1().children(
                [(Wheel::Emote, "表情"), (Wheel::Ping, "信号")].map(|(category, label)| {
                    Button::new(label)
                        .ghost()
                        .small()
                        .label(label)
                        .selected(self.category == category)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.category = category;
                            cx.notify();
                        }))
                }),
            ))
            .child(
                div()
                    .id("palette-items")
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .pb_1()
                    .child(self.items(cx))
                    .overflow_y_scrollbar(),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.border)
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(if self.error.is_some() {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(self.error.clone().unwrap_or_else(|| {
                        if self.dragging.is_some() {
                            "在浮窗外松手发送".into()
                        } else {
                            "按住图标，拖到桌面发送".into()
                        }
                    })),
            );
        #[cfg(target_os = "macos")]
        let panel = panel
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_released))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_released));
        panel
    }
}
