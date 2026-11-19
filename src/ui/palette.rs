//! Borderless picker for sending effects at the mouse release point.
//! 在鼠标松手的位置发送动效的无边框选择面板。

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, Selectable, Sizable,
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

fn minimum_size() -> Size<Pixels> {
    size(px(224.), px(168.))
}

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

struct ItemDrag {
    item: Item,
    outside: Option<Position>,
    preview: Option<platform::PalettePreview>,
    _load: Task<()>,
}

impl ItemDrag {
    fn move_to(&mut self, position: Option<Position>) {
        if self.outside != position {
            self.outside = position;
            if let Some(preview) = &self.preview {
                preview.show_at(position);
            }
        }
    }
}

struct Palette {
    library: Entity<LocalEmotes>,
    _library_subscription: Subscription,
    pinned: bool,
    category: Wheel,
    dragging: Option<ItemDrag>,
    #[cfg(target_os = "windows")]
    pointer_task: Option<Task<()>>,
    #[cfg(target_os = "macos")]
    resize_start: Option<(Point<Pixels>, Size<Pixels>)>,
    #[cfg(target_os = "macos")]
    resize_task: Option<Task<()>>,
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
            pointer_task: None,
            #[cfg(target_os = "macos")]
            resize_start: None,
            #[cfg(target_os = "macos")]
            resize_task: None,
            error: None,
        }
    }

    #[cfg(target_os = "windows")]
    fn watch_pointer(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<async_channel::Receiver<platform::PalettePointer>> {
        let Some((monitor, _)) = &cx.global::<Desktop>().monitor else {
            self.error = Some("鼠标监听不可用，请在设置中重试。".into());
            cx.notify();
            return None;
        };
        monitor.watch_palette_pointer()
    }

    #[cfg(target_os = "windows")]
    fn track_window(
        &mut self,
        update: impl Fn(platform::PalettePointer) -> bool + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(events) = self.watch_pointer(cx) else {
            return;
        };
        if self.dragging.take().is_some() {
            cx.notify();
        }
        // SetWindowPos can dispatch move and DPI callbacks synchronously.
        // SetWindowPos 可能同步触发窗口移动和 DPI 回调。
        self.pointer_task = Some(cx.spawn(async move |_, _| {
            while let Ok(event) = events.recv().await {
                let released = event.released;
                if !update(event) || released {
                    break;
                }
            }
        }));
    }

    #[cfg(target_os = "windows")]
    fn begin_move(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag) = platform::PaletteMove::new(window, event.position) {
            self.track_window(move |event| drag.update(event), cx);
        }
    }

    #[cfg(target_os = "windows")]
    fn begin_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(drag) = platform::PaletteResize::new(window, event.position, minimum_size()) {
            self.track_window(move |event| drag.update(event), cx);
        }
    }

    #[cfg(target_os = "macos")]
    fn begin_resize(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging.take().is_some() {
            cx.notify();
        }
        self.resize_start = Some((event.position, window.viewport_size()));
    }

    #[cfg(target_os = "macos")]
    fn resize(&mut self, position: Point<Pixels>, window: &Window, cx: &App) {
        if let Some((press, initial)) = self.resize_start {
            let delta = position - press;
            let minimum = minimum_size();
            let target = size(
                (initial.width + delta.x).max(minimum.width),
                (initial.height + delta.y).max(minimum.height),
            );
            self.resize_task = platform::resize_palette(window, target, cx);
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

    fn begin_drag(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        #[cfg(target_os = "macos")]
        {
            self.resize_start = None;
        }
        #[cfg(target_os = "windows")]
        {
            let Some(events) = self.watch_pointer(cx) else {
                return;
            };
            self.pointer_task = Some(cx.spawn_in(window, async move |owner, cx| {
                while let Ok(event) = events.recv().await {
                    if event.released {
                        let _ = owner.update_in(cx, |this, window, cx| {
                            this.finish_drag(platform::palette_drop(window, event.position), cx);
                        });
                        return;
                    }
                    if owner
                        .update_in(cx, |this, window, _| {
                            if let Some(drag) = &mut this.dragging {
                                drag.move_to(platform::palette_outside(window, event.position));
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = owner.update_in(cx, |this, _, cx| this.finish_drag(None, cx));
            }));
        }
        let store = self.library.read(cx).store.clone();
        let body = item.body();
        let pixels = (platform::PALETTE_PREVIEW_SIZE as f32 * window.scale_factor()).round() as u32;
        let load = cx.spawn(async move |owner, cx| {
            let frame =
                smol::unblock(move || platform::load_palette_preview(body, &store, pixels.max(1)))
                    .await;
            if let Some(frame) = frame {
                let _ = owner.update(cx, |this, _| {
                    if let Some(drag) = &mut this.dragging {
                        drag.preview = platform::PalettePreview::new(frame);
                        if let Some(preview) = &drag.preview {
                            preview.show_at(drag.outside);
                        }
                    }
                });
            }
        });
        self.dragging = Some(ItemDrag {
            item,
            outside: None,
            preview: None,
            _load: load,
        });
        cx.notify();
    }

    fn finish_drag(&mut self, position: Option<Position>, cx: &mut Context<Self>) {
        let Some(item) = self.dragging.take().map(|drag| drag.item) else {
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
    fn mouse_moved(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button == Some(MouseButton::Left) {
            if let Some(drag) = &mut self.dragging {
                drag.move_to(platform::palette_outside(window, event.position));
            } else {
                self.resize(event.position, window, cx);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn mouse_released(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left && !platform::palette_left_released() {
            return;
        }
        if self.resize_start.is_some() {
            self.resize(event.position, window, cx);
            self.resize_start = None;
        } else if self.dragging.is_some() {
            let position = platform::palette_outside(window, event.position);
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
            .selected(self.dragging.as_ref().is_some_and(|drag| drag.item == item))
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
            .h_9()
            .pl_3()
            .gap_2()
            .items_center()
            .cursor_move()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .child(
                Icon::new(IconName::GripVertical)
                    .small()
                    .text_color(theme.muted_foreground),
            )
            .child("快捷浮窗");
        // Win32 caption hit testing in GPUI uses the previous pointer event.
        // GPUI 的 Win32 标题栏命中测试使用上一次指针事件。
        #[cfg(target_os = "windows")]
        let title = title.on_mouse_down(MouseButton::Left, cx.listener(Self::begin_move));
        #[cfg(target_os = "macos")]
        let title =
            title.on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move());

        let mut resize = Button::new("resize-palette")
            .ghost()
            .small()
            .icon(IconName::MoveDiagonal2)
            .cursor_nwse_resize()
            .accessibility_label("调整浮窗宽高")
            .tooltip("拖动调整浮窗宽高");
        resize
            .interactivity()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_resize));

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
                    .pr_3()
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
                    .flex()
                    .items_end()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl_3()
                            .pr_1()
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
                    )
                    .child(resize),
            );
        #[cfg(target_os = "macos")]
        let panel = {
            let moved = cx.listener(Self::mouse_moved);
            let released = cx.listener(Self::mouse_released);
            // AppKit sends drags beyond the window. Listen in capture phase without a hitbox.
            // AppKit 会送达窗口外的拖动事件，在捕获阶段监听，无需命中区域。
            panel.child(
                canvas(
                    |_, _, _| (),
                    move |_, (), window, _| {
                        window.on_mouse_event(move |event, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                moved(event, window, cx);
                            }
                        });
                        window.on_mouse_event(move |event, phase, window, cx| {
                            if phase == DispatchPhase::Capture {
                                released(event, window, cx);
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
        };
        panel
    }
}
