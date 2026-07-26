//! Routes send, draft, and AI-edit results to the correct composer.

use super::super::app::AviaryApp;
use crate::model::{AccountId, Message, SentMessage};
use crate::runtime::Cmd;
use gpui::{Context, Window};
use gpui_component::notification::Notification;

impl AviaryApp {
    pub(super) fn on_mail_sent(
        &mut self,
        compose_id: u64,
        sent_message: Option<SentMessage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sent_message) = sent_message {
            let fetch = sent_message.needs_resolution().then(|| Cmd::FetchSentCopy {
                account_id: sent_message.message.header.account_id.clone(),
                related_to: sent_message.related_to.clone(),
                snapshot_id: sent_message.message.header.id.clone(),
                sent_id: sent_message.sent_id.clone(),
                internet_message_id: sent_message.internet_message_id.clone(),
            });
            let related_to = sent_message.related_to.clone();
            let messages = self.mailbox.sent_messages.entry(related_to).or_default();
            if !messages
                .iter()
                .any(|existing| existing.message.header.id == sent_message.message.header.id)
            {
                messages.insert(0, sent_message);
                messages.truncate(20);
            }
            // Swap the snapshot for the provider's Sent-items copy as soon
            // as possible; the card silently upgrades when it resolves.
            if let Some(cmd) = fetch {
                self.send(cmd);
            }
        }
        if super::super::quick_actions::is_quick_action_execution(compose_id) {
            return;
        }
        self.close_compose(compose_id, cx);
        self.toast(window, cx, Notification::success(tr!("toast-message-sent")));
    }

    /// The provider's real Sent-items copy arrived for a local reply/forward
    /// snapshot: swap it in, carrying over the expansion state since the
    /// card is keyed by message id.
    pub(super) fn on_sent_copy_resolved(
        &mut self,
        related_to: String,
        snapshot_id: String,
        message: Box<Message>,
        cx: &mut Context<Self>,
    ) {
        let Some(messages) = self.mailbox.sent_messages.get_mut(&related_to) else {
            return;
        };
        let Some(sent) = messages
            .iter_mut()
            .find(|sent| sent.message.header.id == snapshot_id)
        else {
            return;
        };
        let new_id = message.header.id.clone();
        sent.sent_id = Some(new_id.clone());
        sent.message = message;
        if self.mailbox.expanded_sent_messages.remove(&snapshot_id) {
            self.mailbox.expanded_sent_messages.insert(new_id);
        }
        cx.notify();
    }

    pub(super) fn on_ai_edit_chunk(
        &mut self,
        compose_id: u64,
        delta: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.viewer_translation_chunk(compose_id, &delta) {
            true
        } else {
            self.compose_ai_chunk(compose_id, delta, cx);
            false
        }
    }

    pub(super) fn on_ai_edit_finished(
        &mut self,
        compose_id: u64,
        markdown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer_translation_finished(compose_id, markdown.clone()) {
            self.toast(
                window,
                cx,
                Notification::success(tr!("viewer-translation-done")),
            );
        } else {
            self.compose_ai_finished(compose_id, markdown, window, cx);
            self.toast(window, cx, Notification::success(tr!("compose-ai-done")));
        }
    }

    pub(super) fn on_ai_edit_error(
        &mut self,
        compose_id: u64,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.viewer_translation_error(compose_id) {
            self.compose_ai_error(compose_id, error.clone(), cx);
        }
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_compose_runtime_error(
        &mut self,
        compose_id: u64,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.compose_error(compose_id, error.clone(), cx);
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_draft_saved(
        &mut self,
        account_id: AccountId,
        compose_id: u64,
        draft_id: Option<String>,
        autosave: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.compose_draft_saved(&account_id, compose_id, draft_id, autosave, cx);
        if !autosave {
            self.toast(window, cx, Notification::success(tr!("toast-draft-saved")));
        }
    }

    pub(super) fn on_draft_save_error(
        &mut self,
        account_id: AccountId,
        compose_id: u64,
        error: String,
        autosave: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if autosave {
            self.compose_draft_error(&account_id, compose_id, error.clone(), true, cx);
            log::warn!("provider draft autosave failed");
        } else {
            self.on_compose_runtime_error(compose_id, error, window, cx);
        }
    }
}
