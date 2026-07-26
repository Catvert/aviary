//! Reduces list, open, search, and conversation events.

use super::super::app::AviaryApp;
use super::super::compose::ComposeInit;
use super::super::state::{SenderHistoryState, ThreadBodyState, ViewerTab};
use super::super::util;
use crate::model::{AccountId, Message, MessageHeader};
use crate::runtime::Cmd;
use gpui::{Context, Window};
use gpui_component::notification::Notification;
use std::collections::HashMap;
use std::rc::Rc;

/// The Graph delta stream does not carry extended properties: a
/// `last_action: None` means unavailable information, not a cleared action.
fn preserve_last_action(old: &[MessageHeader], new: &mut [MessageHeader]) {
    let known: std::collections::HashMap<&str, _> = old
        .iter()
        .filter(|message| message.last_action.is_some())
        .map(|message| {
            (
                message.id.as_str(),
                (message.last_action, message.last_action_at),
            )
        })
        .collect();
    for message in new
        .iter_mut()
        .filter(|message| message.last_action.is_none())
    {
        if let Some((action, at)) = known.get(message.id.as_str()) {
            message.last_action = *action;
            message.last_action_at = *at;
        }
    }
}

impl AviaryApp {
    /// Restores the last provider-confirmed header after a permanent failure.
    /// The following refresh reconciles folder/search membership in full.
    pub(super) fn restore_confirmed_header(
        &mut self,
        account_id: AccountId,
        header: MessageHeader,
    ) {
        if !self.event_relevant(&account_id) {
            return;
        }
        if let Some(existing) = self
            .mailbox
            .messages
            .iter_mut()
            .find(|message| message.account_id == account_id && message.id == header.id)
        {
            *existing = header.clone();
        } else {
            self.mailbox.messages.push(header.clone());
        }
        if let Some(messages) = &mut self.mailbox.search.results {
            if let Some(existing) = messages
                .iter_mut()
                .find(|message| message.account_id == account_id && message.id == header.id)
            {
                *existing = header.clone();
            }
        }
        if let Some(selected) = self.mailbox.selected_mut() {
            if selected.header.account_id == account_id && selected.header.id == header.id {
                selected.header = header;
            }
        }
        self.mailbox
            .messages
            .sort_by_key(|message| std::cmp::Reverse(message.received));
        self.update_tray_unread();
        self.invalidate_message_list();
    }

    pub(super) fn on_messages(
        &mut self,
        account_id: AccountId,
        mut messages: Vec<MessageHeader>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.uses_unified_pagination() || !self.event_relevant(&account_id) {
            return;
        }
        preserve_last_action(&self.mailbox.messages, &mut messages);
        self.mailbox.pagination.loading_more = false;
        self.mailbox.pagination.last_request_len = None;
        self.mailbox.messages_loaded = true;
        let limit = self.fetch_limit(&account_id);
        let has_more = messages.len() >= limit;
        self.mailbox
            .messages
            .retain(|message| message.account_id != account_id);
        self.mailbox.messages.extend(messages);
        self.mailbox
            .messages
            .sort_by_key(|message| std::cmp::Reverse(message.received));
        self.mailbox.pagination.has_more = self.can_paginate_account(&account_id) && has_more;
        if self.mailbox.refresh_pending {
            self.mailbox.refresh_pending = false;
            self.toast(
                window,
                cx,
                Notification::success(tr!("toast-inbox-updated")),
            );
        }
        self.update_tray_unread();
    }

    /// Replaces this account's thread counts wholesale: the query behind them
    /// is an aggregate over the whole folder, so a partial merge could only
    /// keep counts for threads the cache no longer knows about.
    pub(super) fn on_conversation_totals(
        &mut self,
        account_id: AccountId,
        folder_id: Option<String>,
        totals: HashMap<String, usize>,
    ) {
        // In unified mode every account lists its inbox, which is also the
        // `None` the selection holds — so the same comparison covers both.
        if self.mailbox.selected_folder_id != folder_id {
            return;
        }
        self.mailbox
            .conversation_totals
            .retain(|(existing, _), _| existing != &account_id);
        self.mailbox.conversation_totals.extend(
            totals
                .into_iter()
                .map(|(conversation_id, total)| ((account_id.clone(), conversation_id), total)),
        );
    }

    pub(super) fn on_cached_messages(
        &mut self,
        account_id: AccountId,
        folder_id: Option<String>,
        mut messages: Vec<MessageHeader>,
    ) {
        if self.uses_unified_pagination()
            || !self.event_relevant(&account_id)
            || self.mailbox.selected_folder_id != folder_id
        {
            return;
        }
        preserve_last_action(&self.mailbox.messages, &mut messages);
        self.mailbox.messages_loaded = true;
        let limit = self.fetch_limit(&account_id);
        let has_more = messages.len() >= limit;
        self.mailbox
            .messages
            .retain(|message| message.account_id != account_id);
        self.mailbox.messages.extend(messages);
        self.mailbox
            .messages
            .sort_by_key(|message| std::cmp::Reverse(message.received));
        self.mailbox.pagination.has_more = self.can_paginate_account(&account_id) && has_more;
        self.update_tray_unread();
    }

    pub(super) fn on_more_messages(
        &mut self,
        account_id: AccountId,
        messages: Vec<MessageHeader>,
        has_more: bool,
    ) {
        if !self.event_relevant(&account_id) || !self.can_paginate_account(&account_id) {
            return;
        }
        util::dedup_append(&mut self.mailbox.messages, messages);
        self.mailbox.pagination.has_more = has_more;
        self.mailbox.pagination.loading_more = false;
    }

    pub(super) fn on_unified_messages(
        &mut self,
        request_id: u64,
        mut messages: Vec<MessageHeader>,
        has_more: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_unified_pagination()
            || request_id != self.mailbox.pagination.unified_request_id
        {
            return;
        }
        preserve_last_action(&self.mailbox.messages, &mut messages);
        self.mailbox.messages = messages;
        self.mailbox.messages_loaded = true;
        self.mailbox.pagination.has_more = has_more;
        self.mailbox.pagination.loading_more = false;
        self.mailbox.pagination.last_request_len = None;
        if self.mailbox.refresh_pending {
            self.mailbox.refresh_pending = false;
            self.toast(
                window,
                cx,
                Notification::success(tr!("toast-inbox-updated")),
            );
        }
        self.update_tray_unread();
    }

    pub(super) fn on_unified_cached_messages(
        &mut self,
        request_id: u64,
        mut messages: Vec<MessageHeader>,
    ) {
        if !self.uses_unified_pagination()
            || request_id != self.mailbox.pagination.unified_request_id
        {
            return;
        }
        preserve_last_action(&self.mailbox.messages, &mut messages);
        self.mailbox.messages = messages;
        self.mailbox.messages_loaded = true;
        self.mailbox.pagination.has_more = false;
        self.update_tray_unread();
    }

    pub(super) fn on_unified_more_messages(
        &mut self,
        request_id: u64,
        messages: Vec<MessageHeader>,
        has_more: bool,
    ) {
        if !self.uses_unified_pagination()
            || request_id != self.mailbox.pagination.unified_request_id
        {
            return;
        }
        util::dedup_append(&mut self.mailbox.messages, messages);
        self.mailbox.messages.sort_by(|left, right| {
            right.received.cmp(&left.received).then_with(|| {
                left.account_id
                    .0
                    .cmp(&right.account_id.0)
                    .then_with(|| left.id.cmp(&right.id))
            })
        });
        self.mailbox.pagination.has_more = has_more;
        self.mailbox.pagination.loading_more = false;
    }

    pub(super) fn on_new_messages(
        &mut self,
        account_id: AccountId,
        messages: Vec<MessageHeader>,
        cx: &mut Context<Self>,
    ) {
        // Before anything else, and before the relevance filter below: a
        // blocked sender must not raise a desktop notification, and mail
        // arriving on an account the user is not looking at is exactly the
        // case the block list exists for.
        let messages = self.junk_blocked_messages(messages, cx);
        if messages.is_empty() {
            return;
        }
        if self.settings.global.notifications_enabled {
            if messages.len() > 2 {
                crate::notify::new_message_aggregated(messages.len(), self.notification_tx.clone());
            } else {
                for message in &messages {
                    crate::notify::new_message(message, self.notification_tx.clone());
                }
            }
        }
        if !self.event_relevant(&account_id) {
            return;
        }
        let mut new = messages;
        new.extend(std::mem::take(&mut self.mailbox.messages));
        let mut seen = std::collections::HashSet::new();
        new.retain(|message| seen.insert((message.account_id.clone(), message.id.clone())));
        self.mailbox.messages = new;
        self.update_tray_unread();
    }

    pub(super) fn on_message_changes(
        &mut self,
        account_id: AccountId,
        folder_id: Option<String>,
        upserts: Vec<MessageHeader>,
        deleted: Vec<String>,
    ) {
        if !self.event_relevant(&account_id) || self.mailbox.selected_folder_id != folder_id {
            return;
        }
        for id in deleted {
            self.remove_message_everywhere(&id);
        }
        for mut header in upserts {
            if let Some(existing) = self
                .mailbox
                .messages
                .iter_mut()
                .find(|message| message.account_id == account_id && message.id == header.id)
            {
                if header.last_action.is_none() {
                    header.last_action = existing.last_action;
                    header.last_action_at = existing.last_action_at;
                }
                *existing = header;
            } else {
                self.mailbox.messages.push(header);
            }
        }
        self.mailbox
            .messages
            .sort_by_key(|message| std::cmp::Reverse(message.received));
        self.update_tray_unread();
    }

    pub(super) fn on_message_opened(
        &mut self,
        account_id: AccountId,
        message: Box<Message>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut message = *message;
        if self.awaits_notification_open(&account_id, &message.header.id) {
            self.pending_notification_open = None;
        }
        let explicitly_pending =
            self.pending_kanban_open
                .as_ref()
                .is_some_and(|(pending_account, pending_id)| {
                    pending_account == &account_id && pending_id == &message.header.id
                })
                || self.pending_reply_id.as_deref() == Some(message.header.id.as_str())
                || self.pending_forward_id.as_deref() == Some(message.header.id.as_str());
        if !explicitly_pending
            && self.mailbox.selected_id.as_deref() != Some(message.header.id.as_str())
        {
            return;
        }
        if message.draft_id.is_some() {
            if self
                .kanban
                .preview
                .as_ref()
                .is_some_and(|(preview_account, preview_id)| {
                    preview_account == &account_id && preview_id == &message.header.id
                })
            {
                self.kanban.preview = None;
            }
            self.open_inline_compose(ComposeInit::draft(account_id, message), window, cx);
            return;
        }
        self.update_header(&message.header.id.clone(), |header| header.is_read = true);
        if let Some(conversation_id) = &message.header.conversation_id {
            self.send(Cmd::LoadThread {
                account_id: account_id.clone(),
                conversation_id: conversation_id.clone(),
            });
        }
        if self
            .pending_kanban_open
            .as_ref()
            .is_some_and(|(pending_account, pending_id)| {
                pending_account == &account_id && pending_id == &message.header.id
            })
        {
            self.pending_kanban_open = None;
            self.open_message_tab(message, cx);
            return;
        }
        if self.pending_reply_id.as_deref() == Some(message.header.id.as_str()) {
            self.pending_reply_id = None;
            let init = self.reply_all_init(account_id.clone(), &message);
            self.open_compose_window(init, window, cx);
        }
        if self.pending_forward_id.as_deref() == Some(message.header.id.as_str()) {
            self.pending_forward_id = None;
            self.open_inline_compose(ComposeInit::forward(account_id, &message), window, cx);
        }
        message.header.is_read = true;
        self.mailbox.selected_id = Some(message.header.id.clone());
        self.mailbox.selected = Some(Rc::new(message));
        if self.sender_history_expanded {
            self.refresh_sender_history_for_displayed();
        } else {
            self.sender_history = SenderHistoryState::Idle;
        }
    }

    pub(super) fn on_cached_message_opened(
        &mut self,
        account_id: AccountId,
        message: Box<Message>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut message = *message;
        if self.mailbox.selected_id.as_deref() != Some(message.header.id.as_str()) {
            return;
        }
        if self.pending_reply_id.as_deref() == Some(message.header.id.as_str()) {
            self.pending_reply_id = None;
            let init = self.reply_all_init(account_id.clone(), &message);
            self.open_compose_window(init, window, cx);
        }
        if self.pending_forward_id.as_deref() == Some(message.header.id.as_str()) {
            self.pending_forward_id = None;
            self.open_inline_compose(ComposeInit::forward(account_id, &message), window, cx);
        }
        message.header.is_read = true;
        self.update_header(&message.header.id, |header| header.is_read = true);
        self.mailbox.selected = Some(Rc::new(message));
        self.sender_history = SenderHistoryState::Idle;
    }

    pub(super) fn on_sync_state_changed(
        &mut self,
        account_id: AccountId,
        online: bool,
        error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if online {
            self.offline_accounts.remove(&account_id);
        } else if self.offline_accounts.insert(account_id) {
            let detail = error
                .as_deref()
                .map(super::super::app::compact_error)
                .unwrap_or_else(|| tr!("network-unavailable").to_string());
            self.toast(
                window,
                cx,
                Notification::warning(tr!("offline-local-copy", { detail: detail })),
            );
        }
    }

    pub(super) fn on_thread_message_loaded(&mut self, id: String, message: Box<Message>) {
        self.update_header(&id, |header| header.is_read = true);
        // Also refresh the selection and any tab displaying this message:
        // session.json stores only identities, reconstructed through this
        // cache-first event.
        if self.mailbox.selected_id.as_deref() == Some(id.as_str()) {
            let mut refreshed = (*message).clone();
            refreshed.header.is_read = true;
            self.mailbox.selected = Some(Rc::new(refreshed));
        }
        for tab in &mut self.mailbox.open_tabs {
            match tab {
                ViewerTab::Message(existing)
                    if existing.header.id == id
                        && existing.header.account_id == message.header.account_id =>
                {
                    let mut refreshed = (*message).clone();
                    refreshed.header.is_read = true;
                    *existing = Rc::new(refreshed);
                }
                ViewerTab::Loading(reference)
                    if reference.id == id && reference.account_id == message.header.account_id =>
                {
                    let mut refreshed = (*message).clone();
                    refreshed.header.is_read = true;
                    *tab = ViewerTab::Message(Rc::new(refreshed));
                }
                _ => {}
            }
        }
        self.finish_session_rehydrate(&message);
        self.mailbox
            .thread_bodies
            .insert(id, ThreadBodyState::Loaded(message));
    }

    pub(super) fn on_thread_message_error(
        &mut self,
        id: String,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mailbox
            .thread_bodies
            .insert(id, ThreadBodyState::Error(error.clone()));
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_thread(&mut self, conversation_id: String, messages: Vec<MessageHeader>) {
        let relevant = self
            .mailbox
            .selected
            .as_ref()
            .and_then(|message| message.header.conversation_id.as_deref())
            == Some(conversation_id.as_str());
        if relevant {
            self.mailbox.thread_bodies.clear();
            self.mailbox.thread = Some((conversation_id, messages));
        }
    }

    pub(super) fn on_search_results(
        &mut self,
        account_id: AccountId,
        query: String,
        messages: Vec<MessageHeader>,
    ) {
        if !self.event_relevant(&account_id) || query != self.mailbox.search.query {
            return;
        }
        let results = self.mailbox.search.results.get_or_insert_with(Vec::new);
        util::dedup_append(results, messages);
        self.sort_search_results();
    }

    /// Applies the chosen ordering to the accumulated results.
    ///
    /// Relevance is *arrival order*, not a global ranking: results come from
    /// several sources — the local index first, ranked by `bm25`, then one
    /// response per account — and their scores are not comparable across
    /// backends. Each source contributes its own best-first order, and later
    /// arrivals append. A provider hit that is objectively better than a
    /// cached one therefore still lands below it; ranking them together would
    /// require a score no backend exposes.
    pub(crate) fn sort_search_results(&mut self) {
        use crate::ui::settings::MailSearchSort;
        let Some(results) = self.mailbox.search.results.as_mut() else {
            return;
        };
        match self.mailbox.search.sort {
            MailSearchSort::Relevance => {}
            MailSearchSort::Date => {
                results.sort_by_key(|message| std::cmp::Reverse(message.received))
            }
        }
    }

    /// The provider recorded that we replied to or forwarded this message. The
    /// listing row shows it as an arrow, and an open tab holds its own copy of
    /// the header, so both are updated.
    pub(super) fn on_message_action_noted(
        &mut self,
        id: String,
        action: crate::model::LastAction,
        at: chrono::DateTime<chrono::Utc>,
    ) {
        self.update_header(&id, |header| {
            header.last_action = Some(action);
            header.last_action_at = Some(at);
        });
        for tab in &mut self.mailbox.open_tabs {
            if let Some(message) = tab.message_mut() {
                if message.header.id == id {
                    message.header.last_action = Some(action);
                    message.header.last_action_at = Some(at);
                }
            }
        }
    }
}
