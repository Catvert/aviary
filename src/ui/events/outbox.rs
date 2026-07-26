//! Acknowledgements from the durable outbox.
//!
//! A mutation the user triggered was applied optimistically long before it
//! reached its provider. These reducers close that loop: a success only has to
//! report the batch it completed, a failure has to put the cached header back
//! and say which message it was about.
//!
//! Bulk operations submit one row per message, so each of these events may be
//! one of many for the same user gesture — `take_bulk_message_completion`
//! folds them into a single outcome and decides which reply is the one that
//! gets a toast. A batch rarely ends cleanly: seven messages move and three
//! come back refused, which is why the completion carries both counts rather
//! than a verdict.

use super::super::app::{AviaryApp, BulkCompletion, BulkReply};
use crate::model::{AccountId, MessageHeader, MessageRef};
use crate::runtime::{Cmd, MessageMutationKind};
use gpui::{Context, SharedString, Window};
use gpui_component::notification::{Notification, NotificationType};

impl AviaryApp {
    /// The provider is unreachable: the operation stays in the outbox and will
    /// be retried, so the optimistic state is kept as is. A whole batch is
    /// unreachable at once — thirty identical toasts would say nothing the
    /// first one did not.
    pub(super) fn on_mutation_deferred(
        &mut self,
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
        kind: MessageMutationKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::warn!("durable mutation {operation_id} deferred");
        let deferral = self.note_bulk_deferral(&account_id, &message_id);
        if !deferral.first {
            return;
        }
        let message = if deferral.bulk {
            tr!("bulk-deferred")
        } else {
            tr!("operation-deferred")
        };
        self.toast(
            window,
            cx,
            Self::message_mutation_notification(
                Notification::warning(message),
                kind,
                &account_id,
                &message_id,
            ),
        );
    }

    /// Nothing to change on screen — the optimistic update already stands.
    /// Returns whether the root view needs a redraw.
    pub(super) fn on_mutation_succeeded(
        &mut self,
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        log::debug!("durable mutation {operation_id} completed");
        let reply = self.take_bulk_message_completion(&account_id, &message_id, None);
        self.report_bulk_completion(reply, window, cx);
        false
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_mutation_failed(
        &mut self,
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
        kind: MessageMutationKind,
        header: Option<MessageHeader>,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::error!("durable mutation {operation_id} failed");
        let reply =
            self.take_bulk_message_completion(&account_id, &message_id, Some(error.clone()));
        let reference = MessageRef {
            account_id: account_id.clone(),
            id: message_id,
        };
        // The rollback is per message whatever the batch does: this one came
        // back, the others may still succeed.
        if let Some(header) = header {
            self.restore_confirmed_header(account_id, header);
        }
        match reply {
            BulkReply::Single => {
                self.notify_message_mutation_error(
                    tr!("operation-failed", { error: error }),
                    kind,
                    &reference,
                    None,
                    window,
                    cx,
                );
                self.send_refresh();
            }
            // One failure of many: the batch reports them together, and a
            // listing refreshed per message would fetch the same folder ten
            // times over.
            BulkReply::Pending => {}
            BulkReply::Completed(completion) => {
                self.report_completion(*completion, window, cx);
                self.send_refresh();
                // The reply that closes a batch may well be a failure, and
                // `on_message_moved` — which is what normally reloads the tree
                // once the batch lands — never sees that one.
                self.send(Cmd::LoadFolders {
                    account_id: reference.account_id,
                });
            }
        }
    }

    /// A send could not go out now. The composer stays open and marked queued
    /// rather than reporting a delivery that has not happened.
    pub(super) fn on_outbox_queued(
        &mut self,
        operation_id: i64,
        compose_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::warn!("outbox operation {operation_id} queued");
        self.compose_outbox_queued(compose_id, cx);
        self.toast(window, cx, Notification::warning(tr!("outbox-queued")));
    }

    /// Confirmed deletion. The row is already gone from the listing; what is
    /// left is the pinning, the tray count and the right notification for a
    /// single message or for the batch this one completes.
    pub(super) fn on_message_deleted(
        &mut self,
        account_id: AccountId,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reply = self.take_bulk_message_completion(&account_id, &id, None);
        self.set_message_pinned(&account_id, &id, false);
        self.remove_message_everywhere(&id);
        self.update_tray_unread();
        if !reply.is_bulk() {
            self.toast(
                window,
                cx,
                Self::message_mutation_notification(
                    Notification::success(tr!("toast-message-deleted")),
                    MessageMutationKind::Delete,
                    &account_id,
                    &id,
                ),
            );
            return;
        }
        self.report_bulk_completion(reply, window, cx);
    }

    /// Toasts the batch outcome, if this reply is the one that closed it.
    pub(super) fn report_bulk_completion(
        &mut self,
        reply: BulkReply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(completion) = reply.completion() {
            self.report_completion(completion, window, cx);
        }
    }

    /// The one toast a batch is entitled to.
    ///
    /// Three outcomes rather than two: all through, none through, and the
    /// mixed one — which is the reason this exists. The summary counts, the
    /// body carries the first provider error, because a count alone leaves
    /// nothing to act on.
    fn report_completion(
        &mut self,
        completion: BulkCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let total = completion.total();
        let notification = match (completion.failed, completion.succeeded) {
            (0, _) => {
                // An implicit batch (reading a conversation on open) reports
                // nothing when it works.
                if completion.message.is_empty() {
                    return;
                }
                Notification::success(completion.message)
            }
            (_, 0) => Self::bulk_failure_notification(
                NotificationType::Error,
                tr!("bulk-all-failed", { total: total }),
                completion.first_error.as_deref(),
            ),
            (failed, succeeded) => Self::bulk_failure_notification(
                NotificationType::Warning,
                tr!("bulk-partially-failed", { succeeded: succeeded, total: total, failed: failed }),
                completion.first_error.as_deref(),
            ),
        };
        self.toast(
            window,
            cx,
            Self::pending_action_notification(notification, completion.notification_key),
        );
    }

    /// Puts the counts in the title and the provider's own words underneath.
    /// Without an error to show, the counts stand on their own rather than
    /// heading an empty body.
    fn bulk_failure_notification(
        type_: NotificationType,
        summary: SharedString,
        first_error: Option<&str>,
    ) -> Notification {
        let notification = Notification::new().with_type(type_).autohide(false);
        match first_error.filter(|error| !error.is_empty()) {
            Some(error) => notification
                .title(summary)
                .message(super::super::app::compact_error(error)),
            None => notification.message(summary),
        }
    }
}
