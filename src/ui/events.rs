//! Handles runtime (`Evt`) and notification-area events: the receiving half
//! of the `Cmd`/`Evt` pair. All logic for updating state and notifying after
//! an event arrives lives
//! here; `app.rs` retains construction and rendering.

mod accounts;
mod calendar;
mod compose;
mod contacts;
mod folders_tags;
mod mailbox;
mod outbox;
mod quick_actions;

use super::app::AviaryApp;
use super::state::{AuthState, MainView};
use crate::model::{AccountId, MessageRef};
use crate::runtime::Evt;
use gpui::{Context, Window};
use gpui_component::notification::Notification;

impl AviaryApp {
    pub(super) fn handle_notification_action(
        &mut self,
        action: crate::notify::NotificationAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::notify::NotificationActionKind;

        window.activate_window();
        match (action.kind, action.message) {
            (NotificationActionKind::ShowInbox, _) => {
                self.enter_main_view(MainView::Mail, cx);
                self.send_refresh();
            }
            (NotificationActionKind::Open, Some(reference)) => {
                self.enter_main_view(MainView::Mail, cx);
                self.pending_notification_open = Some(reference.clone());
                self.open_message(reference.account_id, reference.id, cx);
            }
            (NotificationActionKind::Reply, Some(reference)) => {
                self.enter_main_view(MainView::Mail, cx);
                self.pending_notification_open = Some(reference.clone());
                self.pending_reply_id = Some(reference.id.clone());
                self.open_message(reference.account_id, reference.id, cx);
            }
            (NotificationActionKind::MarkRead, Some(reference)) => {
                self.update_header_for(&reference, |header| header.is_read = true);
                self.send(crate::runtime::Cmd::MarkRead {
                    account_id: reference.account_id,
                    id: reference.id,
                    read: true,
                });
            }
            (NotificationActionKind::Archive, Some(reference)) => {
                let MessageRef { account_id, id } = reference;
                self.archive_message_with_undo(account_id, &id, window, cx);
            }
            (_, None) => {}
        }
        cx.notify();
    }

    /// Every block editor the main window can route a runtime reply to, by
    /// `editor_id`: grammar results and downloaded pasted images both land here.
    fn block_editors(&self, cx: &gpui::App) -> Vec<gpui::Entity<super::block_editor::BlockEditor>> {
        let mut editors: Vec<_> = self
            .composes
            .iter()
            .filter_map(|handle| handle.view.upgrade())
            .map(|view| view.read(cx).editor.clone())
            .collect();
        for reply in [
            self.inline_reply.as_ref(),
            self.pending_inline_reply.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            editors.push(reply.view.read(cx).editor.clone());
        }
        editors
    }

    pub(crate) fn configure_languagetool(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings.global.languagetool.clone();
        self.send(crate::runtime::Cmd::ConfigureLanguageTool(settings.clone()));
        self.apply_languagetool_editor_settings(settings, cx);
    }

    pub(crate) fn apply_languagetool_editor_settings(
        &mut self,
        settings: crate::proofreading::LanguageToolSettings,
        cx: &mut Context<Self>,
    ) {
        for editor in self.block_editors(cx) {
            editor.update(cx, |editor, cx| {
                editor.update_proofreading_settings(settings.clone(), cx);
            });
        }
        cx.notify();
    }

    fn route_languagetool_result(
        &mut self,
        editor_id: &str,
        block_id: u64,
        revision: u64,
        source: String,
        issues: Vec<crate::proofreading::ProofreadingIssue>,
        cx: &mut Context<Self>,
    ) {
        for editor in self.block_editors(cx) {
            let handled = editor.update(cx, |editor, cx| {
                editor.apply_languagetool_result(
                    editor_id,
                    block_id,
                    revision,
                    source.clone(),
                    issues.clone(),
                    cx,
                )
            });
            if handled {
                break;
            }
        }
    }

    fn route_languagetool_failure(
        &mut self,
        editor_id: &str,
        block_id: u64,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        for editor in self.block_editors(cx) {
            if editor.update(cx, |editor, cx| {
                editor.apply_languagetool_failure(editor_id, block_id, revision, cx)
            }) {
                break;
            }
        }
    }

    /// Hands a downloaded pasted image to the editor that requested it.
    fn route_fetched_inline_image(
        &mut self,
        editor_id: &str,
        cid: &str,
        bytes: Vec<u8>,
        mime: String,
        cx: &mut Context<Self>,
    ) {
        for editor in self.block_editors(cx) {
            let handled = editor.update(cx, |editor, cx| {
                editor.apply_fetched_inline_image(editor_id, cid, bytes.clone(), mime.clone(), cx)
            });
            if handled {
                break;
            }
        }
    }

    fn route_inline_image_failure(
        &mut self,
        editor_id: &str,
        cid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for editor in self.block_editors(cx) {
            let handled = editor.update(cx, |editor, cx| {
                editor.apply_inline_image_failure(editor_id, cid, window, cx)
            });
            if handled {
                break;
            }
        }
    }

    // ----------------------------------------------------------------
    // Runtime events
    // ----------------------------------------------------------------

    fn event_relevant(&self, account_id: &AccountId) -> bool {
        self.active_account_ids().contains(account_id)
    }

    /// True when this body was requested by clicking an OS notification. The
    /// notified account is not necessarily the one being listed, and the user
    /// asked for *this* message: the relevance filter must let it through.
    fn awaits_notification_open(&self, account_id: &AccountId, id: &str) -> bool {
        self.pending_notification_open
            .as_ref()
            .is_some_and(|reference| &reference.account_id == account_id && reference.id == id)
    }

    /// True when the reading pane displays a message from this account —
    /// selection or pinned tab — even if the account is not part of the
    /// active listing. Session-restored tabs are rehydrated through
    /// `ThreadMessageLoaded`, which must not be filtered for them.
    fn viewer_shows_account(&self, account_id: &AccountId) -> bool {
        self.mailbox
            .selected
            .as_ref()
            .is_some_and(|message| &message.header.account_id == account_id)
            || self.mailbox.open_tabs.iter().any(|tab| {
                tab.message_ref()
                    .is_some_and(|reference| &reference.account_id == account_id)
            })
    }

    /// Pagination shares one `has_more`/`skip` pair in
    /// list state. It is therefore unambiguous only when the
    /// view displays messages from one account.
    fn can_paginate_account(&self, account_id: &AccountId) -> bool {
        match &self.mailbox.unified_selected_account {
            Some(selected) => selected == account_id,
            None => {
                let active = self.active_account_ids();
                active.len() == 1 && active.first() == Some(account_id)
            }
        }
    }

    pub fn handle_event(&mut self, evt: Evt, window: &mut Window, cx: &mut Context<Self>) {
        let mut notify_root = true;
        let prune_message_row_hover = evt.prunes_message_row_hover();
        if evt.invalidates_message_list() {
            self.invalidate_message_list();
        }
        // Outside unified mode, events from accounts other than the active
        // one are ignored, except lifecycle events and responses to composer
        // or event windows, which may originate from any account.
        let lifecycle = evt.is_lifecycle();
        if !lifecycle {
            if let Some(aid) = evt.account_id() {
                let relevant = match &evt {
                    Evt::CalendarEvents { .. } => self.calendar_account_visible(aid),
                    // Processed even for a calendar that was hidden meanwhile:
                    // the failed window must return to the missing chunks, or
                    // it would show empty once the calendar is visible again.
                    Evt::CalendarLoadFailed { .. } => self.account(aid).is_some(),
                    Evt::Tags { .. }
                    | Evt::TagCreated { .. }
                    | Evt::TagRenamed { .. }
                    | Evt::TagDeleted { .. }
                    | Evt::TagColorSet { .. }
                    | Evt::TagApplied { .. }
                    | Evt::QuickActionMessageState { .. }
                    | Evt::TagApplyError { .. }
                    | Evt::TagListing { .. } => self.account(aid).is_some(),
                    Evt::MessageOpened { message, .. }
                    | Evt::CachedMessageOpened { message, .. } => {
                        self.event_relevant(aid)
                            || self.awaits_notification_open(aid, &message.header.id)
                            || (self.view == MainView::Kanban
                                && (self
                                    .kanban
                                    .preview
                                    .as_ref()
                                    .is_some_and(|(account_id, _)| account_id == aid)
                                    || self
                                        .pending_kanban_open
                                        .as_ref()
                                        .is_some_and(|(account_id, _)| account_id == aid)))
                    }
                    Evt::ThreadMessageLoaded { id, .. } | Evt::ThreadMessageError { id, .. } => {
                        self.event_relevant(aid)
                            || self.viewer_shows_account(aid)
                            || self.awaits_session_message(aid, id)
                            || self
                                .kanban
                                .preview
                                .as_ref()
                                .is_some_and(|(account_id, _)| account_id == aid)
                    }
                    Evt::Thread { .. } => {
                        self.event_relevant(aid)
                            || self.viewer_shows_account(aid)
                            || self
                                .kanban
                                .preview
                                .as_ref()
                                .is_some_and(|(account_id, _)| account_id == aid)
                    }
                    _ => self.event_relevant(aid),
                };
                if !relevant {
                    return;
                }
            }
        }
        match evt {
            Evt::LanguageToolStatus(status) => {
                let ready = status.state == crate::proofreading::LanguageToolState::Ready;
                self.languagetool_status = status;
                if ready {
                    for editor in self.block_editors(cx) {
                        editor.update(cx, |editor, cx| {
                            editor.retry_languagetool_failures(cx);
                        });
                    }
                }
            }
            Evt::LanguageToolChecked {
                editor_id,
                block_id,
                revision,
                source,
                issues,
            } => {
                self.route_languagetool_result(&editor_id, block_id, revision, source, issues, cx);
                notify_root = false;
            }
            Evt::LanguageToolCheckFailed {
                editor_id,
                block_id,
                revision,
                error,
            } => {
                log::debug!("LanguageTool editor check failed: {error}");
                self.route_languagetool_failure(&editor_id, block_id, revision, cx);
                notify_root = false;
            }
            Evt::DeviceCode {
                user_code,
                verification_uri,
                message,
            } => self.on_device_code(user_code, verification_uri, message),
            Evt::GoogleAuthOpening { auth_url } => {
                self.on_google_auth_opening(auth_url);
            }
            Evt::Authenticated => {
                self.auth = AuthState::Authenticated;
            }
            Evt::AccountReady(account) => self.on_account_ready(account, window, cx),
            Evt::AccountRestoreFailed {
                account_id,
                provider,
                error,
            } => self.on_account_restore_failed(account_id, provider, error),
            Evt::Messages {
                account_id,
                messages,
            } => self.on_messages(account_id, messages, window, cx),
            Evt::CachedMessages {
                account_id,
                folder_id,
                messages,
            } => self.on_cached_messages(account_id, folder_id, messages),
            Evt::ConversationTotals {
                account_id,
                folder_id,
                totals,
            } => self.on_conversation_totals(account_id, folder_id, totals),
            Evt::MoreMessages {
                account_id,
                messages,
                has_more,
            } => self.on_more_messages(account_id, messages, has_more),
            Evt::UnifiedMessages {
                request_id,
                messages,
                has_more,
            } => self.on_unified_messages(request_id, messages, has_more, window, cx),
            Evt::UnifiedCachedMessages {
                request_id,
                messages,
            } => self.on_unified_cached_messages(request_id, messages),
            Evt::UnifiedMoreMessages {
                request_id,
                messages,
                has_more,
            } => self.on_unified_more_messages(request_id, messages, has_more),
            Evt::NewMessages {
                account_id,
                messages,
            } => self.on_new_messages(account_id, messages, cx),
            Evt::MessageChanges {
                account_id,
                folder_id,
                upserts,
                deleted,
            } => self.on_message_changes(account_id, folder_id, upserts, deleted),
            Evt::MessageOpened {
                account_id,
                message,
            } => self.on_message_opened(account_id, message, window, cx),
            Evt::CachedMessageOpened {
                account_id,
                message,
            } => self.on_cached_message_opened(account_id, message, window, cx),
            Evt::QuickActionMessageLoaded {
                request_id,
                message,
                ..
            } => self.on_quick_action_message_loaded(request_id, message, window, cx),
            Evt::QuickActionMessageError {
                request_id, error, ..
            } => self.on_quick_action_message_error(request_id, error, window, cx),
            Evt::AttachmentFetched {
                account_id,
                message_id,
                attachment_id,
                bytes,
            } => {
                self.on_attachment_fetched(account_id, message_id, attachment_id, bytes, window, cx)
            }
            Evt::AttachmentFetchError {
                account_id,
                message_id,
                attachment_id,
                error,
            } => self.on_attachment_fetch_error(
                account_id,
                message_id,
                attachment_id,
                error,
                window,
                cx,
            ),
            Evt::SyncStateChanged {
                account_id,
                online,
                error,
            } => self.on_sync_state_changed(account_id, online, error, window, cx),
            Evt::MutationDeferred {
                account_id,
                operation_id,
                message_id,
                kind,
            } => self.on_mutation_deferred(account_id, operation_id, message_id, kind, window, cx),
            Evt::MutationSucceeded {
                account_id,
                operation_id,
                message_id,
            } => {
                notify_root =
                    self.on_mutation_succeeded(account_id, operation_id, message_id, window, cx);
            }
            Evt::MutationFailed {
                account_id,
                operation_id,
                message_id,
                kind,
                header,
                error,
            } => self.on_mutation_failed(
                account_id,
                operation_id,
                message_id,
                kind,
                header,
                error,
                window,
                cx,
            ),
            Evt::OutboxQueued {
                account_id: _,
                operation_id,
                compose_id,
            } => self.on_outbox_queued(operation_id, compose_id, window, cx),
            Evt::MailCacheStats {
                used_bytes,
                limit_bytes,
            } => {
                self.mail_cache_used_bytes = used_bytes;
                self.mail_cache_limit_bytes = limit_bytes;
            }
            Evt::MailCacheCleared => {
                self.mail_cache_used_bytes = 0;
                self.address_book.clear_usage(cx);
                self.toast(
                    window,
                    cx,
                    Notification::success(tr!("settings-cache-cleared")),
                );
            }
            Evt::QuickActionCompleted {
                execution_id,
                action_name,
                message_id,
                ..
            } => self.on_quick_action_completed(execution_id, action_name, message_id, window, cx),
            Evt::QuickActionStarted {
                execution_id,
                action_name,
                ..
            } => self.on_quick_action_started(execution_id, action_name, window, cx),
            Evt::QuickActionCancelled {
                execution_id,
                action_name,
                ..
            } => self.on_quick_action_cancelled(execution_id, action_name, window, cx),
            Evt::QuickActionFailed {
                account_id,
                remaining,
                completed_steps,
                error,
            } => self.on_quick_action_failed(
                account_id,
                remaining,
                completed_steps,
                error,
                window,
                cx,
            ),
            Evt::QuickActionSendUncertain {
                account_id,
                remaining,
            } => self.on_quick_action_send_uncertain(account_id, remaining, window, cx),
            Evt::QuickActionMessageState {
                message_id,
                read,
                flagged,
                ..
            } => self.on_quick_action_message_state(message_id, read, flagged),
            Evt::ThreadMessageLoaded {
                account_id: _,
                id,
                message,
            } => self.on_thread_message_loaded(id, message),
            Evt::ThreadMessageError {
                account_id: _,
                id,
                error,
            } => self.on_thread_message_error(id, error, window, cx),
            Evt::Thread {
                account_id: _,
                conversation_id,
                messages,
            } => self.on_thread(conversation_id, messages),
            Evt::SearchResults {
                account_id,
                query,
                messages,
            } => self.on_search_results(account_id, query, messages),
            Evt::CalendarEvents {
                account_id,
                from,
                to,
                events,
            } => self.on_calendar_events(account_id, from, to, events),
            Evt::CalendarLoadFailed {
                account_id,
                from,
                to,
                error,
            } => self.on_calendar_load_failed(account_id, from, to, error, window, cx),
            Evt::InvitationResponded {
                account_id,
                message_id,
                response,
            } => self.on_invitation_responded(account_id, message_id, response, window, cx),
            Evt::InvitationResponseError {
                account_id,
                message_id,
                error,
            } => self.on_invitation_response_error(account_id, message_id, error, window, cx),
            Evt::IcalEvents {
                subscription_id,
                from,
                to,
                events,
            } => self
                .calendar
                .on_ical_events(&subscription_id, from, to, events),
            Evt::IcalSyncState {
                subscription_id,
                syncing,
                error,
                last_success,
            } => self.on_ical_sync_state(subscription_id, syncing, error, last_success),
            Evt::IcalFeedUpdated { subscription_id } => self.on_ical_feed_updated(subscription_id),
            Evt::EventCreated {
                request_id,
                account_id: _,
            } => self.on_calendar_event_saved(request_id, false, window, cx),
            Evt::EventCreateError {
                request_id,
                account_id: _,
                error,
            } => self.on_calendar_event_save_error(request_id, error, window, cx),
            Evt::CalendarEventUpdated {
                request_id,
                account_id: _,
            } => self.on_calendar_event_saved(request_id, true, window, cx),
            Evt::CalendarEventUpdateError {
                request_id,
                account_id: _,
                error,
            } => self.on_calendar_event_save_error(request_id, error, window, cx),
            Evt::CalendarEventDeleted {
                account_id,
                event_id,
            } => self.on_calendar_event_deleted(account_id, event_id, window, cx),
            Evt::CalendarEventDeleteError {
                account_id,
                event_id,
                error,
            } => self.on_calendar_event_delete_error(account_id, event_id, error, window, cx),
            Evt::CalendarEventMoved {
                account_id,
                event_id,
            } => self.on_calendar_event_moved(account_id, event_id, window, cx),
            Evt::CalendarEventMoveError {
                account_id,
                event_id,
                previous_start,
                previous_end,
                error,
            } => self.on_calendar_event_move_error(
                account_id,
                event_id,
                previous_start,
                previous_end,
                error,
                window,
                cx,
            ),
            Evt::SenderHistory {
                account_id: _,
                email,
                messages,
                next_link,
            } => self.on_sender_history(email, messages, next_link),
            Evt::SenderHistoryMore {
                account_id: _,
                email,
                messages: more,
                next_link,
            } => self.on_sender_history_more(email, more, next_link),
            Evt::SenderHistoryError {
                account_id: _,
                email,
                loading_more,
                error,
            } => {
                self.on_sender_history_error(email, loading_more);
                self.notify_error(error, window, cx);
            }
            Evt::Contacts {
                account_id,
                contacts,
            } => self.on_contacts(account_id, contacts, cx),
            Evt::RecipientUsage { entries } => {
                self.address_book.merge_usage(&entries, cx);
                cx.notify();
            }
            Evt::Folders {
                account_id,
                folders,
            } => self.on_folders(account_id, folders),
            Evt::FolderCreated { account_id, folder } => {
                self.on_folder_created(account_id, folder, window, cx);
            }
            Evt::FolderRenamed {
                account_id,
                id,
                new_id,
                new_name,
            } => self.on_folder_renamed(account_id, id, new_id, new_name),
            Evt::FolderDeleted { account_id, id } => {
                self.on_folder_deleted(account_id, id);
            }
            Evt::MessageMoved {
                account_id,
                message_id,
                source_folder_id: _,
                target_folder_id,
                new_id,
            } => {
                self.on_message_moved(account_id, message_id, target_folder_id, new_id, window, cx)
            }
            Evt::Tags { account_id, tags } => {
                self.on_tags(account_id, tags);
            }
            Evt::TagCreated { account_id, tag } => {
                self.on_tag_created(account_id, tag, window, cx);
            }
            Evt::TagRenamed {
                account_id,
                id,
                new_id,
                old_message_tag,
                new_name,
            } => self.on_tag_renamed(account_id, id, new_id, old_message_tag, new_name),
            Evt::TagDeleted { account_id, id } => {
                self.on_tag_deleted(account_id, id);
            }
            Evt::TagColorSet {
                account_id,
                id,
                color,
            } => self.on_tag_color_set(account_id, id, color),
            Evt::TagApplied {
                account_id,
                message_id,
                tag_id,
                added,
            } => self.on_tag_applied(account_id, message_id, tag_id, added),
            Evt::TagApplyError {
                account_id,
                message_id,
                tag_id,
                added,
                error,
            } => {
                self.on_tag_apply_error(account_id, message_id, tag_id, added);
                self.notify_error(error, window, cx);
            }
            Evt::TagListing {
                account_id,
                tag_id,
                messages,
            } => self.on_tag_listing(account_id, tag_id, messages),
            Evt::MessageDeleted { account_id, id } => {
                self.on_message_deleted(account_id, id, window, cx)
            }
            Evt::MessageActionNoted {
                account_id: _,
                id,
                action,
                at,
            } => self.on_message_action_noted(id, action, at),
            Evt::MailSent {
                account_id: _,
                compose_id,
                sent_message,
            } => self.on_mail_sent(compose_id, sent_message, window, cx),
            Evt::SentCopyResolved {
                account_id: _,
                related_to,
                snapshot_id,
                message,
            } => self.on_sent_copy_resolved(related_to, snapshot_id, message, cx),
            Evt::AiMailEditChunk { compose_id, delta } => {
                notify_root = self.on_ai_edit_chunk(compose_id, delta, cx);
            }
            Evt::AiMailEditFinished {
                compose_id,
                markdown,
            } => self.on_ai_edit_finished(compose_id, markdown, window, cx),
            Evt::AiMailEditError { compose_id, error } => {
                self.on_ai_edit_error(compose_id, error, window, cx);
            }
            Evt::MailSendError {
                account_id: _,
                compose_id,
                error,
            } => self.on_compose_runtime_error(compose_id, error, window, cx),
            Evt::DraftSaved {
                account_id,
                compose_id,
                draft_id,
                autosave,
            } => self.on_draft_saved(account_id, compose_id, draft_id, autosave, window, cx),
            Evt::DraftSaveError {
                account_id,
                compose_id,
                error,
                autosave,
            } => self.on_draft_save_error(account_id, compose_id, error, autosave, window, cx),
            Evt::LoggedOut { account_id } => self.on_logged_out(account_id, cx),
            Evt::InlineImageFetched {
                editor_id,
                cid,
                bytes,
                mime,
            } => {
                self.route_fetched_inline_image(&editor_id, &cid, bytes, mime, cx);
                notify_root = false;
            }
            Evt::InlineImageFetchError {
                editor_id,
                cid,
                error,
            } => {
                // Silent for the user: the body keeps the pasted hotlink, which
                // is what it would have had without this feature at all.
                log::debug!("inline image failed (editor {editor_id}, cid {cid}): {error}");
                self.route_inline_image_failure(&editor_id, &cid, window, cx);
                notify_root = false;
            }
            Evt::Status(s) => {
                log::info!("{s}");
                notify_root = false;
            }
            Evt::Error(e) => {
                if !matches!(self.auth, AuthState::Authenticated) {
                    self.auth = if self.accounts.is_empty() {
                        AuthState::Idle
                    } else {
                        AuthState::Authenticated
                    };
                }
                self.mailbox.pagination.loading_more = false;
                self.mailbox.pagination.last_request_len = None;
                self.notify_error(e, window, cx);
            }
        }
        if prune_message_row_hover {
            self.prune_message_row_hover();
        }
        if notify_root {
            cx.notify();
        }
    }

    /// Serves what another invocation of the binary was asked to do — this
    /// process's own launch arguments included, since they travel the same
    /// channel.
    ///
    /// A `mailto:` opens a composer in the reader pane, like every other "new
    /// message" path: the user clicked a link expecting to write, and the
    /// mailbox they write from is the one already on screen.
    pub(super) fn handle_external_request(
        &mut self,
        request: crate::single_instance::ExternalRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::single_instance::ExternalRequest;

        // Whatever the request, the user acted on the desktop and expects to
        // see Aviary. A tray-minimized window would otherwise take the mailto
        // and stay hidden.
        window.activate_window();

        match request {
            // Nothing more to do: a second launch with no argument means
            // "bring the running instance up", which the activation above did.
            ExternalRequest::Activate => {}
            ExternalRequest::Compose(mailto) => {
                let init = crate::ui::compose::ComposeInit {
                    to: mailto.to,
                    cc: mailto.cc,
                    bcc: mailto.bcc,
                    subject: mailto.subject,
                    body_md: mailto.body,
                    ..crate::ui::compose::ComposeInit::blank()
                };
                self.open_inline_compose(init, window, cx);
            }
        }
        cx.notify();
    }

    #[cfg(target_os = "linux")]
    pub(super) fn handle_tray(
        &mut self,
        action: crate::tray::TrayAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            crate::tray::TrayAction::Show => {
                window.activate_window();
            }
            crate::tray::TrayAction::Hide => {
                window.minimize_window();
            }
            crate::tray::TrayAction::Refresh => {
                self.send_refresh();
            }
            crate::tray::TrayAction::Quit => {
                if let Some(tray) = self.tray.take() {
                    tray.shutdown();
                }
                cx.quit();
            }
        }
        cx.notify();
    }
}
