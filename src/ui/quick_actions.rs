//! Account-local one-click mail recipes and their UI/runtime hand-off.

use super::app::{compact_error, AviaryApp};
use super::compose::ComposeInit;
use super::settings::QuickAction;
use super::util;
use crate::blocks::{build_html_body, Block};
use crate::model::{AccountId, Message, MessageHeader, MessageRef};
use crate::runtime::{Cmd, OutgoingMail, QuickActionExecution, QuickActionStep};
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    h_flex,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    popover::Popover,
    ActiveTheme, Disableable, Sizable, WindowExt,
};
use gpui_kit::{
    div, prelude::*, Context, DismissEvent, Entity, Focusable as _, MouseButton, Window,
};

const QUICK_ACTION_EXECUTION_BIT: u64 = 1 << 63;
pub(super) struct QuickActionNotification;

pub(super) fn is_quick_action_execution(id: u64) -> bool {
    id & QUICK_ACTION_EXECUTION_BIT != 0
}

pub(super) fn quick_forward_recipients_valid(forward: &super::settings::QuickForward) -> bool {
    let recipients = util::parse_bare_addresses(&forward.to)
        .into_iter()
        .chain(util::parse_bare_addresses(&forward.cc))
        .chain(util::parse_bare_addresses(&forward.bcc))
        .collect::<Vec<_>>();
    !recipients.is_empty()
        && recipients.iter().all(|address| {
            let mut parts = address.split('@');
            parts.next().is_some_and(|local| !local.trim().is_empty())
                && parts.next().is_some_and(|domain| {
                    domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
                })
                && parts.next().is_none()
        })
}

fn quick_reply_body_valid(reply: &super::settings::QuickReply) -> bool {
    reply.body_blocks.iter().any(|block| match &block.kind {
        crate::blocks::BlockKind::Paragraph(text) => !text.trim().is_empty(),
        _ => true,
    })
}

pub(super) struct PendingQuickActionRequest {
    action: QuickAction,
    source_folder_id: Option<String>,
    header: MessageHeader,
    thread: Vec<MessageHeader>,
}

pub(super) struct QuickActionMenu {
    target: MessageRef,
    scope: &'static str,
    menu: Entity<PopupMenu>,
}

/// The other loaded members of the collapsed conversation a row stands for,
/// or nothing: a thread of one loaded message is an ordinary row.
pub(super) fn thread_companions(
    message_id: &str,
    thread: Option<&[MessageRef]>,
) -> Vec<MessageRef> {
    thread
        .filter(|members| members.len() > 1)
        .into_iter()
        .flatten()
        .filter(|member| member.id != message_id)
        .cloned()
        .collect()
}

impl AviaryApp {
    pub(super) fn retry_quick_action(
        &mut self,
        account_id: AccountId,
        mut execution: QuickActionExecution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_actions.seq = self.quick_actions.seq.wrapping_add(1);
        execution.execution_id = QUICK_ACTION_EXECUTION_BIT | self.quick_actions.seq;
        let name = execution.action_name.clone();
        let delay = if quick_action_sends(&execution) {
            self.action_delay_secs()
                .max(self.settings.global.send_delay_secs)
        } else {
            self.action_delay_secs()
        };
        self.queue_quick_action(account_id, execution, delay, name, window, cx);
    }

    fn queue_quick_action(
        &mut self,
        account_id: AccountId,
        execution: QuickActionExecution,
        delay: u32,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let execution_id = execution.execution_id;
        self.begin_optimistic_quick_action(&account_id, &execution, cx);
        self.send(Cmd::ScheduleQuickAction {
            account_id: account_id.clone(),
            execution,
            delay_secs: delay,
        });
        let app = cx.entity().downgrade();
        window.push_notification(
            Notification::info(tr!("quick-actions-pending", {
                name: name.clone(),
                seconds: delay
            }))
            .id1::<QuickActionNotification>(("quick-action", execution_id))
            .autohide(false)
            .action(move |_, _, _| {
                let app = app.clone();
                let account_id = account_id.clone();
                Button::new("cancel-quick-action")
                    .ghost()
                    .label(tr!("cancel"))
                    .on_click(move |_, _, cx| {
                        let _ = app.update(cx, |this, _| {
                            this.send(Cmd::CancelQuickAction {
                                account_id: account_id.clone(),
                                execution_id,
                            });
                        });
                    })
            }),
            cx,
        );
    }

    fn schedule_quick_action(
        &mut self,
        request: PendingQuickActionRequest,
        message: Option<Message>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PendingQuickActionRequest {
            action,
            source_folder_id,
            header,
            thread,
        } = request;
        self.quick_actions.seq = self.quick_actions.seq.wrapping_add(1);
        let execution_id = QUICK_ACTION_EXECUTION_BIT | self.quick_actions.seq;
        let steps = self.quick_action_steps(&action, &header, &thread, source_folder_id, message);
        if steps.is_empty() {
            return;
        }
        let name = action.name.clone();
        let execution = QuickActionExecution {
            execution_id,
            action_name: name.clone(),
            message_id: header.id,
            steps,
        };
        let delay = if quick_action_sends(&execution) {
            self.action_delay_secs()
                .max(self.settings.global.send_delay_secs)
        } else {
            self.action_delay_secs()
        };
        self.queue_quick_action(header.account_id, execution, delay, name, window, cx);
    }

    /// `thread` carries the loaded members of the conversation a **collapsed**
    /// list row stands for: the recipe's move and read-state steps then apply
    /// to the whole thread — archiving the newest message would only bring an
    /// older one up in its place. Sends, tags and the flag stay on the
    /// message, as in the row's context menu.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_quick_action_controls(
        &self,
        account_id: &AccountId,
        message_id: &str,
        thread: Option<&[MessageRef]>,
        scope: &'static str,
        show_favorites: bool,
        show_menu: bool,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let actions = self.quick_actions_for(account_id);
        if actions.is_empty() {
            return div().into_any_element();
        }
        let offline = self.offline_accounts.contains(account_id);
        let favorites: Vec<_> = actions
            .iter()
            .filter(|action| action.favorite)
            .take(2)
            .cloned()
            .collect();
        let entity = cx.entity();
        let companions = thread_companions(message_id, thread);
        let mut controls = h_flex()
            .id(gpui_kit::ElementId::Name(
                format!(
                    "quick-action-controls-{scope}-{}-{message_id}",
                    account_id.0
                )
                .into(),
            ))
            .gap_0p5()
            .items_center()
            // These controls live inside a clickable message row. Consume both
            // phases so opening a recipe never also reopens the message.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation());
        if show_favorites {
            for action in favorites {
                let aid = account_id.clone();
                let mid = message_id.to_string();
                let companions = companions.clone();
                let action_id = action.id;
                let valid = self.quick_action_is_valid(account_id, &action);
                controls = controls.child(
                    Button::new(gpui_kit::ElementId::Name(
                        format!(
                            "quick-action-{scope}-{}-{}-{action_id}",
                            account_id.0, message_id
                        )
                        .into(),
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(offline || !valid)
                    .icon(
                        super::icons::app_icon(action.icon.asset())
                            .text_color(super::util::packed_color(action.color)),
                    )
                    .tooltip(action.name)
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.trigger_quick_action(
                                    aid.clone(),
                                    mid.clone(),
                                    companions.clone(),
                                    action_id,
                                    window,
                                    cx,
                                );
                            });
                        }
                    }),
                );
            }
        }
        if !show_menu {
            return controls.into_any_element();
        }
        let target = MessageRef {
            account_id: account_id.clone(),
            id: message_id.to_string(),
        };
        let menu_open = self.quick_action_menu_is_open(&target, scope);
        let menu = self
            .quick_actions
            .menu
            .as_ref()
            .filter(|state| state.target == target && state.scope == scope)
            .map(|state| state.menu.clone());
        let menu_button = Button::new(gpui_kit::ElementId::Name(
            format!("quick-actions-menu-{scope}-{}-{}", account_id.0, message_id).into(),
        ))
        .ghost()
        .xsmall()
        .icon(super::icons::app_icon("zap").text_color(cx.theme().muted_foreground))
        .tooltip(tr!("quick-actions-menu"));
        let entity_for_toggle = entity.clone();
        let target_for_toggle = target.clone();
        controls
            .child(
                Popover::new(gpui_kit::ElementId::Name(
                    format!(
                        "quick-actions-popover-{scope}-{}-{}",
                        account_id.0, message_id
                    )
                    .into(),
                ))
                .open(menu_open)
                .appearance(false)
                .overlay_closable(false)
                .trigger(menu_button)
                .on_open_change(move |open, window, cx| {
                    entity_for_toggle.update(cx, |this, cx| {
                        if *open {
                            this.open_quick_action_menu(
                                target_for_toggle.clone(),
                                companions.clone(),
                                scope,
                                window,
                                cx,
                            );
                        } else if this.quick_action_menu_is_open(&target_for_toggle, scope) {
                            this.quick_actions.menu = None;
                            cx.notify();
                        }
                    });
                })
                .content(move |_, window, cx| {
                    let Some(menu) = menu.clone() else {
                        return div().into_any_element();
                    };
                    if !menu.focus_handle(cx).contains_focused(window, cx) {
                        menu.focus_handle(cx).focus(window, cx);
                    }
                    menu.into_any_element()
                }),
            )
            .into_any_element()
    }

    pub(super) fn quick_action_menu_is_open(
        &self,
        target: &MessageRef,
        scope: &'static str,
    ) -> bool {
        self.quick_actions
            .menu
            .as_ref()
            .is_some_and(|state| &state.target == target && state.scope == scope)
    }

    pub(super) fn open_quick_action_menu(
        &mut self,
        target: MessageRef,
        companions: Vec<MessageRef>,
        scope: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let actions = self.quick_actions_for(&target.account_id);
        if actions.is_empty() {
            return;
        }
        let menu_actions: Vec<_> = actions
            .iter()
            .map(|action| {
                let valid = self.quick_action_is_valid(&target.account_id, action);
                (action.clone(), valid)
            })
            .collect();
        let offline = self.offline_accounts.contains(&target.account_id);
        let entity = cx.entity();
        let menu_entity = entity.clone();
        let menu_account_id = target.account_id.clone();
        let menu_message_id = target.id.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            append_quick_action_menu(
                menu,
                &menu_actions,
                &menu_entity,
                &menu_account_id,
                &menu_message_id,
                &companions,
                offline,
            )
        });
        let dismiss_entity = entity.clone();
        let dismiss_target = target.clone();
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, _, cx| {
                dismiss_entity.update(cx, |this, cx| {
                    if this.quick_action_menu_is_open(&dismiss_target, scope) {
                        this.quick_actions.menu = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        self.quick_actions.menu = Some(QuickActionMenu {
            target,
            scope,
            menu,
        });
        cx.notify();
    }

    /// Borrowed: message rows ask whether an account has any recipe on every
    /// frame, and handing them a copy of the list made that question cost one
    /// allocation per row.
    pub(super) fn quick_actions_for(&self, account_id: &AccountId) -> &[QuickAction] {
        self.settings
            .accounts
            .get(account_id)
            .map(|settings| settings.quick_actions.as_slice())
            .unwrap_or_default()
    }

    pub(super) fn quick_action_is_valid(
        &self,
        account_id: &AccountId,
        action: &QuickAction,
    ) -> bool {
        if action.name.trim().is_empty() || !action.has_steps() || !action.targets_are_disjoint() {
            return false;
        }
        if !action.sends_at_most_once()
            || action
                .reply
                .as_ref()
                .is_some_and(|reply| !quick_reply_body_valid(reply))
        {
            return false;
        }
        let tags = self.tags_by_account.get(account_id);
        let folders = self.mailbox.folders_by_account.get(account_id);
        let tags_valid = action
            .add_tags
            .iter()
            .chain(&action.remove_tags)
            .all(|id| tags.is_some_and(|tags| tags.iter().any(|tag| &tag.id == id)));
        let folder_valid = action.move_to_folder_id.as_ref().is_none_or(|id| {
            folders.is_some_and(|folders| folders.iter().any(|folder| &folder.id == id))
        });
        let forward_valid = action
            .forward
            .as_ref()
            .is_none_or(quick_forward_recipients_valid);
        tags_valid && folder_valid && forward_valid
    }

    pub(super) fn trigger_quick_action(
        &mut self,
        account_id: AccountId,
        message_id: String,
        companions: Vec<MessageRef>,
        action_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = self
            .quick_actions_for(&account_id)
            .iter()
            .find(|action| action.id == action_id)
            .cloned()
        else {
            return;
        };
        if self.offline_accounts.contains(&account_id)
            || !self.quick_action_is_valid(&account_id, &action)
        {
            self.toast(
                window,
                cx,
                Notification::warning(tr!("quick-actions-unavailable")),
            );
            return;
        }
        let Some(header) = self.find_quick_action_header(&account_id, &message_id) else {
            self.toast(
                window,
                cx,
                Notification::warning(tr!("quick-actions-message-unavailable")),
            );
            return;
        };
        // Members no longer loaded are left alone: acting on messages that
        // are not on screen would be unexplainable.
        let thread: Vec<MessageHeader> = companions
            .iter()
            .filter(|member| member.account_id == account_id)
            .filter_map(|member| self.find_quick_action_header(&account_id, &member.id))
            .collect();
        let sends = action.forward.is_some() || action.reply.is_some();
        let request = PendingQuickActionRequest {
            action,
            source_folder_id: self.mailbox.selected_folder_id.clone(),
            header,
            thread,
        };
        if !sends {
            self.schedule_quick_action(request, None, window, cx);
            return;
        }
        if let Some(message) = self.displayed_message().filter(|message| {
            message.header.account_id == account_id && message.header.id == message_id
        }) {
            self.schedule_quick_action(request, Some((*message).clone()), window, cx);
            return;
        }

        self.quick_actions.seq = self.quick_actions.seq.wrapping_add(1);
        let request_id = self.quick_actions.seq;
        self.quick_actions.pending.insert(request_id, request);
        self.send(Cmd::LoadQuickActionMessage {
            request_id,
            account_id,
            id: message_id,
        });
        self.toast(
            window,
            cx,
            Notification::info(tr!("quick-actions-loading-message")),
        );
    }

    pub(super) fn on_quick_action_message_loaded(
        &mut self,
        request_id: u64,
        message: Box<Message>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.quick_actions.pending.remove(&request_id) else {
            return;
        };
        self.schedule_quick_action(pending, Some(*message), window, cx);
    }

    pub(super) fn on_quick_action_message_error(
        &mut self,
        request_id: u64,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.quick_actions.pending.remove(&request_id) else {
            return;
        };
        self.toast(
            window,
            cx,
            Notification::error(tr!("quick-actions-load-failed", {
                name: pending.action.name,
                error: compact_error(&error)
            })),
        );
    }

    fn find_quick_action_header(
        &self,
        account_id: &AccountId,
        message_id: &str,
    ) -> Option<MessageHeader> {
        self.mailbox
            .messages
            .iter()
            .chain(self.mailbox.search.results.iter().flatten())
            .find(|header| &header.account_id == account_id && header.id == message_id)
            .cloned()
            .or_else(|| {
                self.displayed_message()
                    .filter(|message| {
                        &message.header.account_id == account_id && message.header.id == message_id
                    })
                    .map(|message| message.header.clone())
            })
    }

    /// `thread` holds the other members of a collapsed conversation, which
    /// get one read-state and one move step each (see `QuickActionStep::Move`).
    fn quick_action_steps(
        &self,
        action: &QuickAction,
        header: &MessageHeader,
        thread: &[MessageHeader],
        source_folder_id: Option<String>,
        message: Option<Message>,
    ) -> Vec<QuickActionStep> {
        let mut steps = Vec::new();
        if let Some(message) = message.as_ref() {
            if let Some(forward) = &action.forward {
                steps.push(QuickActionStep::Forward {
                    mail: self.build_quick_forward(forward, message),
                });
            } else if let Some(reply) = &action.reply {
                steps.push(QuickActionStep::Reply {
                    mail: self.build_quick_reply(reply, message),
                    reply_all: reply.reply_all,
                });
            }
        }
        for tag_id in &action.remove_tags {
            if self.header_has_native_tag(header, tag_id) {
                steps.push(QuickActionStep::RemoveTag {
                    tag_id: tag_id.clone(),
                });
            }
        }
        for tag_id in &action.add_tags {
            if !self.header_has_native_tag(header, tag_id) {
                steps.push(QuickActionStep::AddTag {
                    tag_id: tag_id.clone(),
                });
            }
        }
        if let Some(read) = action.mark_read {
            if read != header.is_read {
                steps.push(QuickActionStep::MarkRead {
                    read,
                    message_id: None,
                });
            }
            steps.extend(
                thread
                    .iter()
                    .filter(|member| member.is_read != read)
                    .map(|member| QuickActionStep::MarkRead {
                        read,
                        message_id: Some(member.id.clone()),
                    }),
            );
        }
        if action
            .set_flagged
            .is_some_and(|flagged| flagged != header.is_flagged)
        {
            steps.push(QuickActionStep::SetFlag {
                flagged: action.set_flagged.expect("checked"),
            });
        }
        if let Some(target_folder_id) = &action.move_to_folder_id {
            if source_folder_id.as_deref() != Some(target_folder_id) {
                steps.push(QuickActionStep::Move {
                    source_folder_id: source_folder_id.clone(),
                    target_folder_id: target_folder_id.clone(),
                    message_id: None,
                });
                steps.extend(thread.iter().map(|member| QuickActionStep::Move {
                    source_folder_id: source_folder_id.clone(),
                    target_folder_id: target_folder_id.clone(),
                    message_id: Some(member.id.clone()),
                }));
            }
        }
        steps
    }

    fn header_has_native_tag(&self, header: &MessageHeader, tag_id: &str) -> bool {
        let provider = self
            .account(&header.account_id)
            .map(|account| account.provider)
            .unwrap_or_default();
        let key = self
            .tags_by_account
            .get(&header.account_id)
            .and_then(|tags| tags.iter().find(|tag| tag.id == tag_id))
            .map(|tag| util::tag_storage_key(provider, tag))
            .unwrap_or_else(|| tag_id.to_string());
        header.tags.contains(&key)
    }

    fn build_quick_forward(
        &self,
        forward: &super::settings::QuickForward,
        message: &Message,
    ) -> OutgoingMail {
        let mut blocks = forward.note_blocks.clone();
        let account_settings = self
            .settings
            .account_or_default(Some(&message.header.account_id));
        let mut images = forward.note_images.clone();
        if let Some(signature) = account_settings
            .signatures
            .iter()
            .find(|signature| signature.is_default)
        {
            blocks.extend(signature.blocks.clone());
            merge_images(&mut images, &signature.images);
        }
        let init = ComposeInit::forward(message.header.account_id.clone(), message);
        blocks.extend(
            init.body_kinds
                .unwrap_or_default()
                .into_iter()
                .map(|kind| Block { id: 0, kind }),
        );
        for (index, block) in blocks.iter_mut().enumerate() {
            block.id = index as u64 + 1;
        }
        merge_images(&mut images, &message.inline_images);
        let body = build_html_body(&blocks);
        let images = crate::blocks::referenced_inline_images(&body, &images);
        OutgoingMail {
            to: util::parse_bare_addresses(&forward.to),
            cc: util::parse_bare_addresses(&forward.cc),
            bcc: util::parse_bare_addresses(&forward.bcc),
            subject: init.subject,
            body,
            body_is_html: true,
            attachments: images,
            files: message.attachments.clone(),
        }
    }

    fn build_quick_reply(
        &self,
        reply: &super::settings::QuickReply,
        message: &Message,
    ) -> OutgoingMail {
        let account_email = self
            .account(&message.header.account_id)
            .map(|account| account.email.as_str());
        let init = if reply.reply_all {
            ComposeInit::reply_all(message.header.account_id.clone(), account_email, message)
        } else {
            ComposeInit::reply(message.header.account_id.clone(), message)
        };
        let mut blocks = reply.body_blocks.clone();
        let account_settings = self
            .settings
            .account_or_default(Some(&message.header.account_id));
        let mut images = reply.body_images.clone();
        if let Some(signature) = account_settings
            .signatures
            .iter()
            .find(|signature| signature.is_default)
        {
            blocks.extend(signature.blocks.clone());
            merge_images(&mut images, &signature.images);
        }
        blocks.extend(
            init.body_kinds
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|kind| Block { id: 0, kind }),
        );
        for (index, block) in blocks.iter_mut().enumerate() {
            block.id = index as u64 + 1;
        }
        merge_images(&mut images, &message.inline_images);
        let body = build_html_body(&blocks);
        let images = crate::blocks::referenced_inline_images(&body, &images);
        OutgoingMail {
            to: util::parse_bare_addresses(&init.to),
            cc: util::parse_bare_addresses(&init.cc),
            bcc: util::parse_bare_addresses(&init.bcc),
            subject: init.subject,
            body,
            body_is_html: true,
            attachments: images,
            files: Vec::new(),
        }
    }
}

pub(super) fn append_quick_action_menu(
    mut menu: PopupMenu,
    actions: &[(QuickAction, bool)],
    entity: &Entity<AviaryApp>,
    account_id: &AccountId,
    message_id: &str,
    companions: &[MessageRef],
    offline: bool,
) -> PopupMenu {
    for (action, valid) in actions.iter().cloned() {
        let entity = entity.clone();
        let account_id = account_id.clone();
        let message_id = message_id.to_string();
        let companions = companions.to_vec();
        let action_id = action.id;
        menu = menu.item(
            PopupMenuItem::new(action.name)
                .icon(
                    super::icons::app_icon(action.icon.asset())
                        .text_color(super::util::packed_color(action.color)),
                )
                .disabled(offline || !valid)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.trigger_quick_action(
                            account_id.clone(),
                            message_id.clone(),
                            companions.clone(),
                            action_id,
                            window,
                            cx,
                        );
                    });
                }),
        );
    }
    menu
}

fn quick_action_sends(execution: &QuickActionExecution) -> bool {
    execution.steps.iter().any(|step| {
        matches!(
            step,
            QuickActionStep::Forward { .. } | QuickActionStep::Reply { .. }
        )
    })
}

fn merge_images(target: &mut Vec<crate::model::InlineImage>, source: &[crate::model::InlineImage]) {
    for image in source {
        if !target.iter().any(|item| item.cid == image.cid) {
            target.push(image.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str) -> MessageRef {
        MessageRef {
            account_id: AccountId("account@example.test".into()),
            id: id.into(),
        }
    }

    #[test]
    fn a_collapsed_thread_hands_its_other_members_to_the_recipe() {
        let thread = [member("newest"), member("older"), member("oldest")];
        assert_eq!(
            thread_companions("newest", Some(&thread)),
            vec![member("older"), member("oldest")]
        );
    }

    #[test]
    fn an_ordinary_row_or_a_thread_of_one_keeps_the_recipe_on_the_message() {
        assert!(thread_companions("newest", None).is_empty());
        assert!(thread_companions("newest", Some(&[member("newest")])).is_empty());
    }
}
