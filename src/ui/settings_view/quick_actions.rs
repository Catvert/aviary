//! Preferences editor for account-local one-click mail recipes.

use super::super::app::AviaryApp;
use super::super::block_editor::BlockEditor;
use super::super::settings::{QuickAction, QuickActionIcon, QuickForward, QuickReply};
use super::{labelled, QuickActionEditorState, SettingsTab};
use gpui::{div, prelude::*, px, Context, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    color_picker::ColorPicker,
    h_flex,
    input::InputState,
    menu::{DropdownMenu, PopupMenuItem},
    scroll::ScrollableElement,
    v_flex, ActiveTheme, Selectable, Sizable, StyledExt,
};

impl AviaryApp {
    pub(super) fn render_settings_quick_actions(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        self.ensure_contacts_loaded();
        let selector = self.render_settings_account_selector(SettingsTab::ActionsRapides, cx);
        let Some(account_id) = self.current_account_id.clone() else {
            return v_flex()
                .child(selector)
                .child(tr!("quick-actions-no-account"))
                .into_any_element();
        };
        self.ensure_tags_loaded(&account_id);

        let list = self.render_quick_action_list(&account_id, cx);

        v_flex()
            .child(selector)
            .child(
                self.section(&tr!("settings-tab-quick-actions"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("quick-actions-hint")),
                    )
                    .child(list),
            )
            .child(self.render_quick_action_editor(&account_id, cx))
            .into_any_element()
    }

    fn commit_quick_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account_id) = self.current_account_id.clone() else {
            return;
        };
        let Some(state) = self.settings_ui.as_ref().map(|ui| &ui.quick_action) else {
            return;
        };
        let name = state.name.read(cx).value().trim().to_string();
        let to = state.to.read(cx).serialized(cx);
        let cc = state.cc.read(cx).serialized(cx);
        let bcc = state.bcc.read(cx).serialized(cx);
        let note_blocks = state.note.read(cx).to_blocks(cx);
        let note_images = state.note.read(cx).images().to_vec();
        let reply_blocks = state.reply_body.read(cx).to_blocks(cx);
        let reply_images = state.reply_body.read(cx).images().to_vec();
        let forward = state.forward_enabled.then_some(QuickForward {
            to,
            cc,
            bcc,
            note_blocks,
            note_images,
        });
        let reply = state.reply_enabled.then_some(QuickReply {
            reply_all: state.reply_all,
            body_blocks: reply_blocks,
            body_images: reply_images,
        });
        let editing_id = state.editing_id;
        let mut action = QuickAction {
            id: editing_id.unwrap_or_default(),
            name,
            icon: state.icon,
            color: state
                .color
                .read(cx)
                .value()
                .map(super::super::util::color_to_packed)
                .unwrap_or(0xE5A50A),
            favorite: state.favorite,
            forward,
            reply,
            add_tags: state.add_tags.iter().cloned().collect(),
            remove_tags: state.remove_tags.iter().cloned().collect(),
            mark_read: state.mark_read,
            set_flagged: state.set_flagged,
            move_to_folder_id: state.move_to_folder_id.clone(),
        };
        if action.name.is_empty()
            || !action.has_steps()
            || !action.targets_are_disjoint()
            || !action.sends_at_most_once()
            || action
                .reply
                .as_ref()
                .is_some_and(|reply| !blocks_have_content(&reply.body_blocks))
            || action.forward.as_ref().is_some_and(|forward| {
                !super::super::quick_actions::quick_forward_recipients_valid(forward)
            })
        {
            self.toast(
                window,
                cx,
                gpui_component::notification::Notification::error(tr!(
                    "quick-actions-validation-error"
                )),
            );
            return;
        }
        let account = self.settings.account_mut(&account_id);
        if action.id == 0 {
            account.quick_action_seq += 1;
            action.id = account.quick_action_seq;
        }
        let mut favorite_rejected = false;
        if action.favorite {
            let favorites = account
                .quick_actions
                .iter()
                .filter(|existing| existing.favorite && existing.id != action.id)
                .count();
            if favorites >= 2 {
                action.favorite = false;
                favorite_rejected = true;
            }
        }
        if let Some(existing) = account
            .quick_actions
            .iter_mut()
            .find(|existing| existing.id == action.id)
        {
            *existing = action;
        } else {
            account.quick_actions.push(action);
        }
        self.settings.save();
        if favorite_rejected {
            self.toast(
                window,
                cx,
                gpui_component::notification::Notification::warning(tr!(
                    "quick-actions-favorite-limit"
                )),
            );
        }
        self.clear_quick_action_form(window, cx);
    }

    fn edit_quick_action(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account_id) = self.current_account_id.clone() else {
            return;
        };
        let Some(action) = self
            .settings
            .account_or_default(Some(&account_id))
            .quick_actions
            .into_iter()
            .find(|action| action.id == id)
        else {
            return;
        };
        let (kinds, images, to, cc, bcc) = action
            .forward
            .as_ref()
            .map(|forward| {
                (
                    forward
                        .note_blocks
                        .iter()
                        .map(|block| block.kind.clone())
                        .collect(),
                    forward.note_images.clone(),
                    forward.to.clone(),
                    forward.cc.clone(),
                    forward.bcc.clone(),
                )
            })
            .unwrap_or_default();
        let note_editor = cx.new(|cx| {
            BlockEditor::new(
                kinds,
                images,
                false,
                self.settings.global.mail_body_options(),
                &tr!("quick-actions-note-placeholder"),
                window,
                cx,
            )
        });
        let (reply_kinds, reply_images, reply_all) = action
            .reply
            .as_ref()
            .map(|reply| {
                (
                    reply
                        .body_blocks
                        .iter()
                        .map(|block| block.kind.clone())
                        .collect(),
                    reply.body_images.clone(),
                    reply.reply_all,
                )
            })
            .unwrap_or_default();
        let reply_editor = cx.new(|cx| {
            BlockEditor::new(
                reply_kinds,
                reply_images,
                false,
                self.settings.global.mail_body_options(),
                &tr!("quick-actions-reply-placeholder"),
                window,
                cx,
            )
        });
        let to_input = new_recipient_input(
            &to,
            tr!("compose-to-placeholder").to_string(),
            10,
            self.address_book.clone(),
            window,
            cx,
        );
        let cc_input = new_recipient_input(
            &cc,
            tr!("compose-cc-placeholder").to_string(),
            20,
            self.address_book.clone(),
            window,
            cx,
        );
        let bcc_input = new_recipient_input(
            &bcc,
            tr!("compose-bcc-placeholder").to_string(),
            30,
            self.address_book.clone(),
            window,
            cx,
        );
        if let Some(ui) = &mut self.settings_ui {
            let state = &mut ui.quick_action;
            state.editing_id = Some(id);
            state.icon = action.icon;
            state.favorite = action.favorite;
            state.add_tags = action.add_tags.into_iter().collect();
            state.remove_tags = action.remove_tags.into_iter().collect();
            state.mark_read = action.mark_read;
            state.set_flagged = action.set_flagged;
            state.move_to_folder_id = action.move_to_folder_id;
            state.note = note_editor;
            state.forward_enabled = action.forward.is_some();
            state.reply_enabled = action.reply.is_some();
            state.reply_all = reply_all;
            state.reply_body = reply_editor;
            state.color.update(cx, |picker, cx| {
                picker.set_value(super::super::util::packed_color(action.color), window, cx)
            });
            set_input(&state.name, action.name, window, cx);
            state.to = to_input;
            state.cc = cc_input;
            state.bcc = bcc_input;
        }
        cx.notify();
    }

    fn delete_quick_action(
        &mut self,
        account_id: &crate::model::AccountId,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings
            .account_mut(account_id)
            .quick_actions
            .retain(|action| action.id != id);
        self.settings.save();
        if self
            .settings_ui
            .as_ref()
            .is_some_and(|ui| ui.quick_action.editing_id == Some(id))
        {
            self.clear_quick_action_form(window, cx);
        }
        cx.notify();
    }

    fn clear_quick_action_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let replacement = QuickActionEditorState::new(
            self.settings.global.mail_body_options(),
            self.address_book.clone(),
            window,
            cx,
        );
        if let Some(ui) = &mut self.settings_ui {
            ui.quick_action = replacement;
        }
        cx.notify();
    }

    /// The account's saved recipes, each with its edit and delete entry.
    fn render_quick_action_list(
        &self,
        account_id: &crate::model::AccountId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let actions = self
            .settings
            .account_or_default(Some(account_id))
            .quick_actions;
        let actions_empty = actions.is_empty();
        let mut list = v_flex().gap_2();
        for action in actions {
            let id = action.id;
            let delete_account = account_id.clone();
            let valid = self.quick_action_is_valid(account_id, &action);
            list = list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        super::super::icons::app_icon(action.icon.asset())
                            .text_color(super::super::util::packed_color(action.color)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child(action.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if valid {
                                        theme.muted_foreground
                                    } else {
                                        theme.danger
                                    })
                                    .child(if valid {
                                        quick_action_summary(&action)
                                    } else {
                                        tr!("quick-actions-invalid-target").to_string()
                                    }),
                            ),
                    )
                    .when(action.favorite, |row| {
                        row.child(
                            super::super::icons::app_icon("star")
                                .xsmall()
                                .text_color(theme.warning),
                        )
                    })
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("edit-quick-action-{id}").into(),
                        ))
                        .ghost()
                        .xsmall()
                        .label(tr!("edit"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.edit_quick_action(id, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("delete-quick-action-{id}").into(),
                        ))
                        .danger()
                        .xsmall()
                        .label(tr!("delete"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.delete_quick_action(&delete_account, id, window, cx);
                            },
                        )),
                    ),
            );
        }
        if actions_empty {
            list = list.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(tr!("quick-actions-empty")),
            );
        }
        list
    }

    /// The recipe being edited, or a new one: what it forwards or replies with,
    /// and the triage it applies.
    fn render_quick_action_editor(
        &self,
        account_id: &crate::model::AccountId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        let folders = self
            .mailbox
            .folders_by_account
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        let tags = self
            .tags_by_account
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        let tags_empty = tags.is_empty();
        let (
            editing_id,
            name,
            to,
            cc,
            bcc,
            forward_enabled,
            note,
            reply_enabled,
            reply_all,
            reply_body,
            color,
            icon,
            favorite,
            add_tags,
            remove_tags,
            mark_read,
            set_flagged,
            move_to,
        ) = {
            let state = &self.settings_ui.as_ref().expect("settings UI").quick_action;
            (
                state.editing_id,
                state.name.clone(),
                state.to.clone(),
                state.cc.clone(),
                state.bcc.clone(),
                state.forward_enabled,
                state.note.clone(),
                state.reply_enabled,
                state.reply_all,
                state.reply_body.clone(),
                state.color.clone(),
                state.icon,
                state.favorite,
                state.add_tags.clone(),
                state.remove_tags.clone(),
                state.mark_read,
                state.set_flagged,
                state.move_to_folder_id.clone(),
            )
        };

        let icon_label = quick_action_icon_label(icon);
        let icon_entity = entity.clone();
        let read_label = tri_state_label(
            mark_read,
            "quick-actions-state-unchanged",
            "quick-actions-mark-read",
            "quick-actions-mark-unread",
        );
        let flag_label = tri_state_label(
            set_flagged,
            "quick-actions-state-unchanged",
            "quick-actions-set-flag",
            "quick-actions-clear-flag",
        );
        let folder_label = move_to
            .as_ref()
            .and_then(|id| folders.iter().find(|folder| &folder.id == id))
            .map(|folder| folder.display_name.clone())
            .unwrap_or_else(|| tr!("quick-actions-no-move").to_string());
        let folder_entity = entity.clone();

        let mut tag_rows = v_flex().gap_1();
        for tag in tags {
            let add_id = tag.id.clone();
            let remove_id = tag.id.clone();
            tag_rows = tag_rows.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(tag.display_name))
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("quick-add-tag-{}", tag.id).into(),
                        ))
                        .xsmall()
                        .selected(add_tags.contains(&tag.id))
                        .label(tr!("quick-actions-add-tag"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ui) = &mut this.settings_ui {
                                let state = &mut ui.quick_action;
                                if !state.add_tags.insert(add_id.clone()) {
                                    state.add_tags.remove(&add_id);
                                } else {
                                    state.remove_tags.remove(&add_id);
                                }
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("quick-remove-tag-{}", tag.id).into(),
                        ))
                        .xsmall()
                        .selected(remove_tags.contains(&tag.id))
                        .label(tr!("quick-actions-remove-tag"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ui) = &mut this.settings_ui {
                                let state = &mut ui.quick_action;
                                if !state.remove_tags.insert(remove_id.clone()) {
                                    state.remove_tags.remove(&remove_id);
                                } else {
                                    state.add_tags.remove(&remove_id);
                                }
                            }
                            cx.notify();
                        })),
                    ),
            );
        }
        let tag_picker = if tags_empty {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("quick-actions-no-tags"))
                .into_any_element()
        } else {
            div()
                .max_h(px(220.))
                .overflow_y_scrollbar()
                .p_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(tag_rows)
                .into_any_element()
        };

        self.section(
            &if editing_id.is_some() {
                tr!("quick-actions-edit-title")
            } else {
                tr!("quick-actions-new-title")
            },
            cx,
        )
        .child(labelled(&tr!("quick-actions-name"), &name, cx))
        .child(
            h_flex()
                .gap_3()
                .items_end()
                .child(
                    v_flex().gap_1().child(tr!("quick-actions-icon")).child(
                        Button::new("quick-action-icon")
                            .outline()
                            .label(icon_label)
                            .dropdown_menu(move |mut menu, _, _| {
                                for option in [
                                    QuickActionIcon::Zap,
                                    QuickActionIcon::Forward,
                                    QuickActionIcon::Reply,
                                    QuickActionIcon::Folder,
                                    QuickActionIcon::Tag,
                                    QuickActionIcon::Archive,
                                ] {
                                    menu = menu.item(
                                        PopupMenuItem::new(quick_action_icon_label(option))
                                            .checked(option == icon)
                                            .on_click({
                                                let entity = icon_entity.clone();
                                                move |_, _, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        if let Some(ui) = &mut this.settings_ui {
                                                            ui.quick_action.icon = option;
                                                        }
                                                        cx.notify();
                                                    });
                                                }
                                            }),
                                    );
                                }
                                menu
                            }),
                    ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(tr!("quick-actions-color"))
                        .child(ColorPicker::new(&color).small()),
                )
                .child(
                    Checkbox::new("quick-action-favorite")
                        .checked(favorite)
                        .label(tr!("quick-actions-favorite"))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if let Some(ui) = &mut this.settings_ui {
                                ui.quick_action.favorite = *checked;
                            }
                            cx.notify();
                        })),
                ),
        )
        .child(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(super::super::icons::app_icon("forward").small())
                        .child(
                            div()
                                .font_semibold()
                                .child(tr!("quick-actions-forward-heading")),
                        ),
                )
                .child(
                    Checkbox::new("quick-action-forward-enabled")
                        .checked(forward_enabled)
                        .label(tr!("quick-actions-forward-enabled"))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if let Some(ui) = &mut this.settings_ui {
                                ui.quick_action.forward_enabled = *checked;
                                if *checked {
                                    ui.quick_action.reply_enabled = false;
                                }
                            }
                            cx.notify();
                        })),
                )
                .when(forward_enabled, |section| {
                    section
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(tr!("quick-actions-forward-hint")),
                        )
                        .child(recipient_row(&tr!("compose-to-label"), to))
                        .child(recipient_row(&tr!("compose-cc-label"), cc))
                        .child(recipient_row(&tr!("compose-bcc"), bcc))
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_sm().child(tr!("quick-actions-note")))
                                .child(BlockEditor::format_toolbar(
                                    "quick-action-forward-format-actions",
                                    note.clone(),
                                    false,
                                )),
                        )
                        .child(
                            div()
                                .h(px(220.))
                                .overflow_y_scrollbar()
                                .p_2()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .child(note),
                        )
                }),
        )
        .child(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(super::super::icons::app_icon("reply").small())
                        .child(
                            div()
                                .font_semibold()
                                .child(tr!("quick-actions-reply-heading")),
                        ),
                )
                .child(
                    Checkbox::new("quick-action-reply-enabled")
                        .checked(reply_enabled)
                        .label(tr!("quick-actions-reply-enabled"))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if let Some(ui) = &mut this.settings_ui {
                                ui.quick_action.reply_enabled = *checked;
                                if *checked {
                                    ui.quick_action.forward_enabled = false;
                                }
                            }
                            cx.notify();
                        })),
                )
                .when(reply_enabled, |section| {
                    section
                        .child(
                            Checkbox::new("quick-action-reply-all")
                                .checked(reply_all)
                                .label(tr!("quick-actions-reply-all"))
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    if let Some(ui) = &mut this.settings_ui {
                                        ui.quick_action.reply_all = *checked;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(tr!("quick-actions-reply-hint")),
                        )
                        .child(h_flex().justify_end().child(BlockEditor::format_toolbar(
                            "quick-action-reply-format-actions",
                            reply_body.clone(),
                            false,
                        )))
                        .child(
                            div()
                                .h(px(220.))
                                .overflow_y_scrollbar()
                                .p_2()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .child(reply_body),
                        )
                }),
        )
        .child(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(super::super::icons::app_icon("tags").small())
                        .child(
                            div()
                                .font_semibold()
                                .child(tr!("quick-actions-triage-heading")),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(tr!("quick-actions-tags-heading")),
                )
                .child(tag_picker)
                .child(
                    div()
                        .mt_1()
                        .text_sm()
                        .font_medium()
                        .child(tr!("quick-actions-state-destination-heading")),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(tri_state_button(
                            "quick-action-read",
                            read_label,
                            true,
                            cx.entity(),
                        ))
                        .child(tri_state_button(
                            "quick-action-flag",
                            flag_label,
                            false,
                            cx.entity(),
                        ))
                        .child(
                            Button::new("quick-action-folder")
                                .outline()
                                .label(folder_label)
                                .dropdown_menu(move |mut menu, _, _| {
                                    menu = menu.item(
                                        PopupMenuItem::new(tr!("quick-actions-no-move"))
                                            .checked(move_to.is_none())
                                            .on_click({
                                                let entity = folder_entity.clone();
                                                move |_, _, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        if let Some(ui) = &mut this.settings_ui {
                                                            ui.quick_action.move_to_folder_id =
                                                                None;
                                                        }
                                                        cx.notify();
                                                    });
                                                }
                                            }),
                                    );
                                    for folder in folders.clone() {
                                        let id = folder.id.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(folder.display_name)
                                                .checked(move_to.as_ref() == Some(&id))
                                                .on_click({
                                                    let entity = folder_entity.clone();
                                                    move |_, _, cx| {
                                                        entity.update(cx, |this, cx| {
                                                            if let Some(ui) = &mut this.settings_ui
                                                            {
                                                                ui.quick_action.move_to_folder_id =
                                                                    Some(id.clone());
                                                            }
                                                            cx.notify();
                                                        });
                                                    }
                                                }),
                                        );
                                    }
                                    menu
                                }),
                        ),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("save-quick-action")
                        .primary()
                        .label(tr!("save"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.commit_quick_action(window, cx);
                        })),
                )
                .when(editing_id.is_some(), |row| {
                    row.child(
                        Button::new("cancel-quick-action")
                            .ghost()
                            .label(tr!("cancel"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.clear_quick_action_form(window, cx);
                            })),
                    )
                }),
        )
    }
}

fn set_input(
    input: &gpui::Entity<InputState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    input.update(cx, |input, cx| input.set_value(value, window, cx));
}

fn new_recipient_input(
    initial: &str,
    placeholder: String,
    tab_index: isize,
    address_book: super::super::addresses::AddressBook,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) -> gpui::Entity<super::super::addresses::RecipientInput> {
    cx.new(|cx| {
        super::super::addresses::RecipientInput::new(initial, placeholder, address_book, window, cx)
            .tab_index(tab_index)
    })
}

fn recipient_row(
    label: &str,
    input: gpui::Entity<super::super::addresses::RecipientInput>,
) -> gpui::AnyElement {
    h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_start()
        .child(div().w(px(42.)).pt_1().text_sm().child(label.to_string()))
        .child(div().flex_1().min_w_0().child(input))
        .into_any_element()
}

fn quick_action_summary(action: &QuickAction) -> String {
    let mut count = action.add_tags.len() + action.remove_tags.len();
    count += usize::from(action.forward.is_some());
    count += usize::from(action.reply.is_some());
    count += usize::from(action.mark_read.is_some());
    count += usize::from(action.set_flagged.is_some());
    count += usize::from(action.move_to_folder_id.is_some());
    tr!("quick-actions-step-count", { count: count }).to_string()
}

fn quick_action_icon_label(icon: QuickActionIcon) -> gpui::SharedString {
    match icon {
        QuickActionIcon::Zap => tr!("quick-actions-icon-zap"),
        QuickActionIcon::Forward => tr!("quick-actions-icon-forward"),
        QuickActionIcon::Reply => tr!("quick-actions-icon-reply"),
        QuickActionIcon::Folder => tr!("quick-actions-icon-folder"),
        QuickActionIcon::Tag => tr!("quick-actions-icon-tag"),
        QuickActionIcon::Archive => tr!("quick-actions-icon-archive"),
    }
}

fn blocks_have_content(blocks: &[crate::blocks::Block]) -> bool {
    blocks.iter().any(|block| match &block.kind {
        crate::blocks::BlockKind::Paragraph(text) => !text.trim().is_empty(),
        _ => true,
    })
}

fn tri_state_label(
    value: Option<bool>,
    unchanged: &'static str,
    yes: &'static str,
    no: &'static str,
) -> gpui::SharedString {
    match value {
        None => tr!(unchanged),
        Some(true) => tr!(yes),
        Some(false) => tr!(no),
    }
}

fn tri_state_button(
    id: &'static str,
    label: gpui::SharedString,
    read: bool,
    entity: gpui::Entity<AviaryApp>,
) -> impl IntoElement {
    Button::new(id)
        .outline()
        .label(label)
        .dropdown_menu(move |menu, _, _| {
            let choices = [
                (None, tr!("quick-actions-state-unchanged")),
                (
                    Some(true),
                    if read {
                        tr!("quick-actions-mark-read")
                    } else {
                        tr!("quick-actions-set-flag")
                    },
                ),
                (
                    Some(false),
                    if read {
                        tr!("quick-actions-mark-unread")
                    } else {
                        tr!("quick-actions-clear-flag")
                    },
                ),
            ];
            choices.into_iter().fold(menu, |menu, (value, label)| {
                let entity = entity.clone();
                menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        if let Some(ui) = &mut this.settings_ui {
                            if read {
                                ui.quick_action.mark_read = value;
                            } else {
                                ui.quick_action.set_flagged = value;
                            }
                        }
                        cx.notify();
                    });
                }))
            })
        })
}
