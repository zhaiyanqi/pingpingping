//! Search and maintenance for the local emote library.
//! 本机表情库的搜索与管理。

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable, WindowExt,
    button::{Button, ButtonVariant, ButtonVariants},
    dialog::DialogButtonProps,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{Desktop, LibraryEdit, LocalEmotes};
use crate::assets::EMOTES;
use crate::emotes::{EmoteId, MAX_EMOTE_NAME_CHARS, valid_emote_name};

#[derive(Clone, Copy)]
enum Row {
    Custom(usize),
    BuiltIn(usize),
}

pub(super) struct EmoteLibrary {
    library: Entity<LocalEmotes>,
    search: Entity<InputState>,
    rename_name: Entity<InputState>,
    renaming: Option<EmoteId>,
    scroll: UniformListScrollHandle,
    _subscriptions: [Subscription; 3],
}

impl EmoteLibrary {
    pub(super) fn new(
        library: Entity<LocalEmotes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索表情名称"));
        let rename_name = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = [
            cx.observe(&library, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |this: &mut Self, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &rename_name,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { shift: false, .. } => this.save_name(window, cx),
                    _ => {}
                },
            ),
        ];
        Self {
            library,
            search,
            rename_name,
            renaming: None,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn start_rename(&mut self, id: EmoteId, window: &mut Window, cx: &mut Context<Self>) {
        let library = self.library.read(cx);
        if library.task.is_some() {
            return;
        }
        if let Some(entry) = library.entries.iter().find(|entry| entry.id() == id) {
            let name = entry.name().to_owned();
            self.renaming = Some(id);
            self.rename_name.update(cx, |input, cx| {
                input.set_value(name, window, cx);
                input.select_all(window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        }
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.search.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn save_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.renaming else { return };
        let name = self.rename_name.read(cx).value();
        if !valid_emote_name(&name) {
            return;
        }
        let owner = cx.weak_entity();
        let started = self.library.update(cx, |library, cx| {
            library.edit(
                id,
                LibraryEdit::Rename(name.to_string()),
                move |saved, cx| {
                    if saved {
                        let _ = owner.update(cx, |this, cx| {
                            this.renaming = None;
                            cx.notify();
                        });
                    }
                },
                cx,
            )
        });
        if started {
            self.search.focus_handle(cx).focus(window, cx);
        }
    }

    fn confirm_removal(&self, id: EmoteId, window: &mut Window, cx: &mut Context<Self>) {
        let library = self.library.read(cx);
        if library.task.is_some() {
            return;
        }
        let Some(entry) = library.entries.iter().find(|entry| entry.id() == id) else {
            return;
        };
        let title = format!("从表情库删除「{}」？", entry.name());
        let library = self.library.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let library = library.clone();
            dialog
                .confirm()
                .title(title.clone())
                .description("此操作会立即移除表情库中的条目。")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("删除")
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("取消"),
                )
                .on_ok(move |_, _, cx| {
                    library.update(cx, |library, cx| {
                        library.edit(id, LibraryEdit::Remove, |_, _| {}, cx)
                    })
                })
        });
    }

    fn rename_editor(&self, busy: bool, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("library-rename-editor")
            .flex()
            .flex_col()
            .gap_1()
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if !busy && event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.finish_rename(window, cx);
                }
            }))
            .child(div().text_sm().child("表情名称"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Input::new(&self.rename_name)
                            .id("library-rename-name")
                            .aria_label("表情名称")
                            .disabled(busy)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        Button::new("library-cancel-rename")
                            .small()
                            .ghost()
                            .label("取消")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.finish_rename(window, cx);
                            })),
                    )
                    .child(
                        Button::new("library-save-name")
                            .small()
                            .primary()
                            .label("保存名称")
                            .disabled(busy || !valid_emote_name(&self.rename_name.read(cx).value()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_name(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("1 至 {MAX_EMOTE_NAME_CHARS} 个字符")),
            )
    }
}

impl Render for EmoteLibrary {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let library = self.library.read(cx);
        let busy = library.task.is_some();
        let editing = self.renaming.is_some();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let matches = |name: &str| query.is_empty() || name.to_lowercase().contains(&query);
        let rows: Vec<_> = library
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches(entry.name()))
            .map(|(index, _)| Row::Custom(index))
            .chain(
                EMOTES
                    .iter()
                    .enumerate()
                    .filter(|(_, clip)| matches(clip.name))
                    .map(|(index, _)| Row::BuiltIn(index)),
            )
            .collect();
        let count = rows.len();
        let owner = cx.weak_entity();
        let import = self.library.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_5()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("表情库"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Input::new(&self.search)
                            .id("library-search")
                            .aria_label("搜索表情名称")
                            .cleanable(true)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        Button::new("library-import")
                            .small()
                            .icon(IconName::Plus)
                            .label("导入图片…")
                            .disabled(busy || editing)
                            .on_click(move |_, _, cx| import.update(cx, LocalEmotes::import)),
                    ),
            )
            .when(editing, |panel| panel.child(self.rename_editor(busy, cx)))
            .when_some(library.error.clone(), |panel, error| {
                panel.child(div().text_sm().text_color(theme.danger).child(error))
            })
            .when(busy, |panel| {
                panel.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("正在处理表情…"),
                )
            })
            .when(count == 0 && !busy, |panel| {
                panel.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("没有匹配的表情"),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list("library-emote-list", count, move |range, _, cx| {
                            let desktop = cx.global::<Desktop>();
                            let library = desktop.library.read(cx);
                            rows[range]
                                .iter()
                                .map(|row| {
                                    library_row(*row, desktop, library, &owner, busy || editing, cx)
                                })
                                .collect()
                        })
                        .size_full()
                        .track_scroll(&self.scroll),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

fn library_row(
    row: Row,
    desktop: &Desktop,
    library: &LocalEmotes,
    owner: &WeakEntity<EmoteLibrary>,
    disabled: bool,
    cx: &App,
) -> Div {
    let (name, image, id): (_, ImageSource, _) = match row {
        Row::Custom(index) => {
            let emote = &library.entries[index];
            (
                emote.name(),
                library.store.poster(emote.id()).into(),
                Some(emote.id()),
            )
        }
        Row::BuiltIn(index) => (
            EMOTES[index].name,
            desktop.emotes[index].clone().into(),
            None,
        ),
    };
    let theme = cx.theme();
    div()
        .flex()
        .h_12()
        .items_center()
        .gap_3()
        .pl_1()
        .pr_4()
        .border_b_1()
        .border_color(theme.border)
        .child(
            img(image)
                .size_8()
                .flex_shrink_0()
                .object_fit(ObjectFit::Contain),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .child(name.to_owned()),
        )
        .map(|row| {
            if let Some(id) = id {
                let item_id = SharedString::from(id.to_string());
                let rename_owner = owner.clone();
                let delete_owner = owner.clone();
                row.child(
                    Button::new((ElementId::from("rename-library-emote"), item_id.clone()))
                        .small()
                        .ghost()
                        .label("重命名…")
                        .accessibility_label(format!("重命名「{name}」"))
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            let _ = rename_owner
                                .update(cx, |this, cx| this.start_rename(id, window, cx));
                        }),
                )
                .child(
                    Button::new((ElementId::from("delete-library-emote"), item_id))
                        .small()
                        .ghost()
                        .label("删除…")
                        .accessibility_label(format!("从表情库删除「{name}」"))
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            let _ = delete_owner
                                .update(cx, |this, cx| this.confirm_removal(id, window, cx));
                        }),
                )
            } else {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("内置"),
                )
            }
        })
}
