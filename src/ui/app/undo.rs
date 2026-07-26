//! Deferred, undoable mail mutations.
//!
//! Every mutation the user can take back — move, delete, archive, tag,
//! read/flagged, kanban move, and sending itself — goes through
//! `schedule_action`: the UI state is updated optimistically, the commands are
//! held for `action_delay_secs` (`send_delay_secs` for a send) behind a
//! notification whose "cancel" button restores what the optimistic update
//! removed, and only then are they submitted to the durable outbox.
//!
//! The `PendingCancelEffect` of an action is exactly what undo has to put
//! back, which is why the optimistic removals live here too.

use crate::model::{AccountId, MessageHeader, MessageRef};
use crate::runtime::{Cmd, MessageMutationKind};
use crate::ui::app::{
    AviaryApp, BulkDeferral, BulkReply, MessageState, OptimisticMessageRemoval,
    OptimisticSelection, PendingAction, PendingActionNotification, PendingCancelEffect,
};
use crate::ui::state::SenderHistoryState;
use gpui::{Context, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    notification::Notification,
    WindowExt,
};
use std::time::Duration;

/// How long a batch keeps aggregating its replies after its commands were
/// submitted. This is a memory guard, not a deadline: the outbox retries a
/// failing mutation up to eight times behind a backoff capped at five minutes,
/// so ten minutes of legitimate straggling is normal, and forgetting a batch
/// earlier would turn its last replies back into one toast per message.
const BULK_COMPLETION_TTL: Duration = Duration::from_secs(20 * 60);

/// The loaded members of a thread that still have to be marked read when its
/// row is opened.
///
/// `opened_id` is left out on purpose: the runtime already reads the message
/// it opens. Members whose header is not in the listing are left out too —
/// what the row stands for is what is loaded under it, and marking a message
/// nobody can see would be unexplainable.
fn unread_conversation_members(
    headers: &[MessageHeader],
    members: &[MessageRef],
    opened_id: &str,
) -> Vec<MessageRef> {
    members
        .iter()
        .filter(|member| member.id != opened_id)
        .filter(|member| {
            headers.iter().any(|header| {
                header.account_id == member.account_id && header.id == member.id && !header.is_read
            })
        })
        .cloned()
        .collect()
}

impl AviaryApp {
    /// Books one terminal provider reply against the batch it belongs to.
    /// `error` set means the mutation failed for good.
    pub(crate) fn take_bulk_message_completion(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        error: Option<String>,
    ) -> BulkReply {
        let reference = MessageRef {
            account_id: account_id.clone(),
            id: message_id.to_string(),
        };
        self.bulk_completions.record(&reference, error)
    }

    /// Books a deferral, which is not terminal: the operation stays in the
    /// outbox and the message keeps its place in the batch.
    pub(crate) fn note_bulk_deferral(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
    ) -> BulkDeferral {
        let reference = MessageRef {
            account_id: account_id.clone(),
            id: message_id.to_string(),
        };
        self.bulk_completions.note_deferral(&reference)
    }

    /// Starts aggregating the replies of commands that are about to be
    /// submitted. A single message is left alone: it reports itself.
    ///
    /// `completed_message` is the all-succeeded copy; `None` keeps the batch
    /// silent unless something fails, which is what an implicit action wants.
    fn begin_bulk_completion(
        &mut self,
        references: Vec<MessageRef>,
        completed_message: Option<SharedString>,
        notification_key: SharedString,
        cx: &mut Context<Self>,
    ) {
        if references.len() < 2 {
            return;
        }
        let completion_id = self.bulk_completions.claim(&references);
        self.bulk_completions.arm(
            completion_id,
            &references,
            completed_message.map(Into::into).unwrap_or_default(),
            notification_key,
        );
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BULK_COMPLETION_TTL).await;
            let _ = this.update(cx, |this, _| {
                this.bulk_completions.forget(completion_id, &references);
            });
        })
        .detach();
    }

    /// Submits commands right away — no undo window — while still folding
    /// their replies into one batch. Used by the side effects of reading,
    /// which the user did not ask for explicitly and must not have to cancel.
    fn send_batch_now(&mut self, commands: Vec<Cmd>, cx: &mut Context<Self>) {
        let references = Self::command_message_references(&commands);
        self.pending_action_seq = self.pending_action_seq.wrapping_add(1);
        let notification_key: SharedString =
            format!("bulk-action:{}", self.pending_action_seq).into();
        self.begin_bulk_completion(references, None, notification_key, cx);
        for command in commands {
            self.send(command);
        }
    }

    /// Moves incoming mail from blocked senders to junk, returning what is
    /// left for the list to display.
    ///
    /// Silent and without an undo window, like `mark_conversation_read`: the
    /// user decided once, when they blocked the sender, and a toast per spam
    /// message would defeat the point of blocking. The batch aggregation still
    /// applies, so a provider failure produces one toast rather than twenty.
    ///
    /// A message the account cannot junk — an IMAP server with no junk mailbox,
    /// an account currently offline — is left in the list instead: hiding it
    /// with nowhere to put it would lose it.
    pub(crate) fn junk_blocked_messages(
        &mut self,
        messages: Vec<MessageHeader>,
        cx: &mut Context<Self>,
    ) -> Vec<MessageHeader> {
        if self.settings.global.blocked_senders.is_empty() {
            return messages;
        }
        let (blocked, kept): (Vec<_>, Vec<_>) = messages.into_iter().partition(|header| {
            self.settings.global.sender_is_blocked(&header.from)
                && self.junk_folder_available(&header.account_id)
                && !self.offline_accounts.contains(&header.account_id)
        });
        if blocked.is_empty() {
            return kept;
        }
        let commands = blocked
            .iter()
            .map(|header| Cmd::MoveMessage {
                account_id: header.account_id.clone(),
                message_id: header.id.clone(),
                source_folder_id: None,
                target_folder_id: self.junk_target_folder_id(&header.account_id),
            })
            .collect();
        self.send_batch_now(commands, cx);
        kept
    }

    /// Blocks or unblocks a sender from a message, and says what happened.
    ///
    /// The toast is the whole feedback: blocking moves messages out of a list
    /// the user is looking at, and unblocking changes nothing visible at all —
    /// neither reads as having worked without being told.
    pub(crate) fn toggle_sender_block(
        &mut self,
        from: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(address) = crate::ui::settings::normalized_sender(from) else {
            return;
        };
        if self.settings.global.sender_is_blocked(from) {
            self.settings.global.unblock_sender(from);
            self.settings.save();
            window.push_notification(
                Notification::info(tr!("sender-unblocked", { sender: address.clone() })),
                cx,
            );
        } else {
            let moved = self.block_sender_and_junk_loaded(from, cx);
            window.push_notification(
                Notification::info(if moved > 0 {
                    tr!("sender-blocked-with-messages", { sender: address.clone(), count: moved })
                } else {
                    tr!("sender-blocked", { sender: address.clone() })
                }),
                cx,
            );
        }
        cx.notify();
    }

    /// Blocks a sender and clears out what is already on screen from them.
    ///
    /// Blocking only future mail would leave the messages that prompted it
    /// sitting in the inbox, so the loaded ones follow the same silent path as
    /// arrivals. Only *loaded* headers: what the list holds is what the user
    /// can see, and reaching for the rest would mean a provider search behind
    /// an action that reads as immediate. Returns how many moved.
    pub(crate) fn block_sender_and_junk_loaded(
        &mut self,
        from: &str,
        cx: &mut Context<Self>,
    ) -> usize {
        let Some(address) = crate::ui::settings::normalized_sender(from) else {
            return 0;
        };
        if !self.settings.global.block_sender(from) {
            return 0;
        }
        self.settings.save();
        // This sender only, not every blocked one: the count goes into a toast
        // naming them, and sweeping up someone else's mail under their name
        // would be a lie about what just happened.
        let references: Vec<MessageRef> = self
            .mailbox
            .messages
            .iter()
            .filter(|header| {
                crate::ui::settings::normalized_sender(&header.from).as_ref() == Some(&address)
            })
            .filter(|header| {
                self.junk_folder_available(&header.account_id)
                    && !self.offline_accounts.contains(&header.account_id)
            })
            .map(|header| MessageRef {
                account_id: header.account_id.clone(),
                id: header.id.clone(),
            })
            .collect();
        if references.is_empty() {
            return 0;
        }
        let commands = references
            .iter()
            .map(|reference| Cmd::MoveMessage {
                account_id: reference.account_id.clone(),
                message_id: reference.id.clone(),
                source_folder_id: None,
                target_folder_id: self.junk_target_folder_id(&reference.account_id),
            })
            .collect();
        for reference in &references {
            self.remove_message_optimistically_ref(reference);
        }
        self.send_batch_now(commands, cx);
        self.invalidate_message_list();
        references.len()
    }

    /// Marks messages unread with no undo window, for the one case where the
    /// user did not just ask for it: a snoozed message coming back due.
    ///
    /// Silent for the same reason reading a thread is — the decision was made
    /// when the deadline was set — and batched so a provider failure is one
    /// toast rather than one per woken message.
    pub(crate) fn bulk_mark_unread_silently(
        &mut self,
        references: Vec<MessageRef>,
        cx: &mut Context<Self>,
    ) {
        let references: Vec<_> = references
            .into_iter()
            .filter(|reference| !self.offline_accounts.contains(&reference.account_id))
            .collect();
        if references.is_empty() {
            return;
        }
        let commands = references
            .iter()
            .map(|reference| MessageState::Read.command(reference, false))
            .collect();
        for reference in &references {
            self.update_header_for(reference, |header| MessageState::Read.apply(header, false));
        }
        self.send_batch_now(commands, cx);
    }

    /// Reads a conversation whole when its collapsed row is opened.
    ///
    /// The row stands for the thread — its unread mark and its counter are
    /// the thread's — so leaving the other loaded members unread would keep
    /// the row bold right after it was read. Deliberately silent and without
    /// an undo window: this is a side effect of reading, like the one the
    /// runtime already applies to the message it opens (hence `opened_id`,
    /// which is skipped here).
    pub(crate) fn mark_conversation_read(
        &mut self,
        members: &[MessageRef],
        opened_id: &str,
        cx: &mut Context<Self>,
    ) {
        let unread = unread_conversation_members(&self.mailbox.messages, members, opened_id);
        if unread.is_empty() {
            return;
        }
        let commands = unread
            .iter()
            .map(|reference| MessageState::Read.command(reference, true))
            .collect();
        for reference in &unread {
            self.update_header_for(reference, |header| MessageState::Read.apply(header, true));
        }
        self.send_batch_now(commands, cx);
        self.update_tray_unread();
        self.invalidate_message_list();
    }

    pub(crate) fn action_delay_secs(&self) -> u32 {
        self.settings.global.effective_action_delay_secs()
    }

    fn command_message_mutation(command: &Cmd) -> Option<(MessageMutationKind, &AccountId, &str)> {
        match command {
            Cmd::DeleteMessage { account_id, id } => {
                Some((MessageMutationKind::Delete, account_id, id))
            }
            Cmd::MoveMessage {
                account_id,
                message_id,
                ..
            } => Some((MessageMutationKind::Move, account_id, message_id)),
            Cmd::SetFlag {
                account_id,
                id,
                flagged,
            } => Some((MessageMutationKind::SetFlag(*flagged), account_id, id)),
            Cmd::MarkRead {
                account_id,
                id,
                read,
            } => Some((MessageMutationKind::MarkRead(*read), account_id, id)),
            _ => None,
        }
    }

    /// The messages a batch of commands acts on, in submission order. Derived
    /// from the commands themselves rather than passed alongside them: the two
    /// could only drift apart, and a batch is exactly what its commands touch.
    fn command_message_references(commands: &[Cmd]) -> Vec<MessageRef> {
        commands
            .iter()
            .filter_map(|command| {
                let (_, account_id, message_id) = Self::command_message_mutation(command)?;
                Some(MessageRef {
                    account_id: account_id.clone(),
                    id: message_id.to_string(),
                })
            })
            .collect()
    }

    pub(super) fn message_mutation_notification_key(
        kind: MessageMutationKind,
        account_id: &AccountId,
        message_id: &str,
    ) -> SharedString {
        let action = match kind {
            MessageMutationKind::Delete => "delete",
            MessageMutationKind::Move => "move",
            MessageMutationKind::SetFlag(true) => "flag",
            MessageMutationKind::SetFlag(false) => "unflag",
            MessageMutationKind::MarkRead(true) => "read",
            MessageMutationKind::MarkRead(false) => "unread",
        };
        // Length-prefix both provider-owned identifiers so no separator
        // character inside either value can make two messages collide.
        format!(
            "message-action:{action}:{}:{}:{}:{}",
            account_id.0.len(),
            account_id,
            message_id.len(),
            message_id
        )
        .into()
    }

    pub(crate) fn message_mutation_notification(
        notification: Notification,
        kind: MessageMutationKind,
        account_id: &AccountId,
        message_id: &str,
    ) -> Notification {
        Self::pending_action_notification(
            notification,
            Self::message_mutation_notification_key(kind, account_id, message_id),
        )
    }

    pub(crate) fn pending_action_notification(
        notification: Notification,
        notification_key: SharedString,
    ) -> Notification {
        notification.id1::<PendingActionNotification>(notification_key)
    }

    /// Holds a common mutation for a short window so the notification can
    /// genuinely cancel it before the network call.
    pub(crate) fn send_undoable(
        &mut self,
        command: Cmd,
        pending_message: impl Into<SharedString>,
        started_message: impl Into<SharedString>,
        canceled_message: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = self.action_delay_secs();
        self.schedule_action(
            vec![command],
            delay,
            pending_message.into(),
            started_message.into(),
            canceled_message.into(),
            None,
            PendingCancelEffect::None,
            window,
            cx,
        );
    }

    pub(crate) fn send_many_undoable(
        &mut self,
        commands: Vec<Cmd>,
        pending_message: impl Into<SharedString>,
        started_message: impl Into<SharedString>,
        canceled_message: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if commands.is_empty() {
            return;
        }
        let delay = self.action_delay_secs();
        self.schedule_action(
            commands,
            delay,
            pending_message.into(),
            started_message.into(),
            canceled_message.into(),
            None,
            PendingCancelEffect::None,
            window,
            cx,
        );
    }

    pub(crate) fn send_compose_undoable(
        &mut self,
        command: Cmd,
        compose_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = self.settings.global.send_delay_secs;
        self.schedule_action(
            vec![command],
            delay,
            tr!("undo-send-pending", { seconds: delay }),
            tr!("undo-send-started"),
            tr!("undo-send-cancelled"),
            None,
            PendingCancelEffect::Compose { compose_id },
            window,
            cx,
        );
    }

    /// Flags a message, optimistically and undoably.
    pub(crate) fn set_flag_undoable(
        &mut self,
        reference: MessageRef,
        flagged: bool,
        previous_flagged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_message_state_undoable(
            MessageState::Flagged,
            vec![(reference, previous_flagged)],
            flagged,
            window,
            cx,
        );
    }

    /// Marks a message read or unread, optimistically and undoably.
    pub(crate) fn mark_read_undoable(
        &mut self,
        reference: MessageRef,
        read: bool,
        previous_read: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_message_state_undoable(
            MessageState::Read,
            vec![(reference, previous_read)],
            read,
            window,
            cx,
        );
    }

    pub(crate) fn bulk_set_flag_undoable(
        &mut self,
        items: Vec<(MessageRef, bool)>,
        flagged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_message_state_undoable(MessageState::Flagged, items, flagged, window, cx);
    }

    pub(crate) fn bulk_mark_read_undoable(
        &mut self,
        items: Vec<(MessageRef, bool)>,
        read: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_message_state_undoable(MessageState::Read, items, read, window, cx);
    }

    /// Applies `value` to one message or to a whole selection: offline accounts
    /// are skipped, the cached headers take the new value immediately, and one
    /// command per message is scheduled behind the undo window. `items` carries
    /// each message's previous value, which a mixed selection needs to restore
    /// itself exactly if the user cancels.
    fn toggle_message_state_undoable(
        &mut self,
        state: MessageState,
        items: Vec<(MessageRef, bool)>,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items: Vec<_> = items
            .into_iter()
            .filter(|(reference, _)| !self.offline_accounts.contains(&reference.account_id))
            .collect();
        if items.is_empty() {
            return;
        }
        let commands = items
            .iter()
            .map(|(reference, _)| state.command(reference, value))
            .collect();
        for (reference, _) in &items {
            self.update_header_for(reference, |header| state.apply(header, value));
        }
        let delay = self.action_delay_secs();
        let (pending, started, canceled) = state.undo_copy(items.len(), delay);
        self.schedule_action(
            commands,
            delay,
            pending,
            started,
            canceled,
            None,
            state.restore_effect(items),
            window,
            cx,
        );
    }

    /// Removes the message from the current folder immediately, then restores
    /// it if the provider move is cancelled during the configured undo window.
    pub(crate) fn move_message_with_undo(
        &mut self,
        account_id: AccountId,
        message_id: &str,
        source_folder_id: Option<String>,
        target_folder_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = self.action_delay_secs();
        self.remove_message_with_undo(
            Cmd::MoveMessage {
                account_id,
                message_id: message_id.to_string(),
                source_folder_id,
                target_folder_id,
            },
            message_id,
            delay,
            tr!("undo-message-move-pending", { seconds: delay }),
            tr!("undo-message-move-started"),
            tr!("undo-message-move-cancelled"),
            window,
            cx,
        );
    }

    /// Id of one of the account's well-known folders, falling back to the
    /// provider-agnostic alias each backend resolves itself. The account's own
    /// id is preferred when the folder list already carries it; the alias is
    /// what makes a single call site work across the three providers when it
    /// does not — Gmail in particular exposes no Archive folder at all,
    /// archiving being the removal of `INBOX` there.
    fn well_known_folder_id(
        &self,
        account_id: &AccountId,
        well_known: &str,
        alias: &str,
    ) -> String {
        self.mailbox
            .folders_by_account
            .get(account_id)
            .and_then(|folders| {
                folders
                    .iter()
                    .find(|folder| folder.well_known_name.as_deref() == Some(well_known))
            })
            .map(|folder| folder.id.clone())
            .unwrap_or_else(|| alias.to_string())
    }

    /// Folder id every "archive" entry point moves to.
    /// See [`Self::well_known_folder_id`] and `providers::ARCHIVE_FOLDER_ALIAS`.
    pub(crate) fn archive_target_folder_id(&self, account_id: &AccountId) -> String {
        self.well_known_folder_id(
            account_id,
            "archive",
            crate::providers::ARCHIVE_FOLDER_ALIAS,
        )
    }

    /// Folder id every "mark as junk" entry point moves to.
    /// See [`Self::well_known_folder_id`] and `providers::JUNK_FOLDER_ALIAS`.
    pub(crate) fn junk_target_folder_id(&self, account_id: &AccountId) -> String {
        self.well_known_folder_id(account_id, "junkemail", crate::providers::JUNK_FOLDER_ALIAS)
    }

    /// Whether the account has anywhere to put junk, which is what decides
    /// whether the action is offered at all.
    ///
    /// Graph and Gmail always list one (`junkemail`, the `SPAM` label). An IMAP
    /// server may have no mailbox flagged `\Junk` and none named like one, and
    /// `providers::imap` fails with a translated error when asked to resolve the
    /// alias — an error the user can do nothing about, so the entry is hidden
    /// instead. Reading the folder list rather than branching on the provider
    /// keeps that true for all three without a special case.
    pub(crate) fn junk_folder_available(&self, account_id: &AccountId) -> bool {
        self.mailbox
            .folders_by_account
            .get(account_id)
            .is_some_and(|folders| {
                folders
                    .iter()
                    .any(|folder| folder.well_known_name.as_deref() == Some("junkemail"))
            })
    }

    /// Whether the folder currently on screen is that account's junk folder,
    /// which is what flips the action between "this is junk" and "this is not".
    ///
    /// The *displayed* folder, deliberately, not the message's own: a listing
    /// is a folder, and a search result is not a place the reverse action makes
    /// sense from. It is the same rule every desktop client applies.
    pub(crate) fn viewing_junk_folder(&self, account_id: &AccountId) -> bool {
        let Some(selected) = self.mailbox.selected_folder_id.as_deref() else {
            return false;
        };
        self.mailbox
            .folders_by_account
            .get(account_id)
            .and_then(|folders| folders.iter().find(|folder| folder.id == selected))
            .is_some_and(|folder| folder.well_known_name.as_deref() == Some("junkemail"))
    }

    /// Marking as junk deliberately reports no source folder, for the same
    /// reason archiving does not: Graph and IMAP ignore it, while for Gmail the
    /// source is the label to drop, and passing the folder on screen would strip
    /// *that* label and leave the message in the inbox — flagged as spam yet
    /// still delivered. `None` drops `INBOX`, which is what junking means.
    pub(crate) fn mark_junk_with_undo(
        &mut self,
        account_id: AccountId,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_folder_id = self.junk_target_folder_id(&account_id);
        let delay = self.action_delay_secs();
        self.remove_message_with_undo(
            Cmd::MoveMessage {
                account_id,
                message_id: message_id.to_string(),
                source_folder_id: None,
                target_folder_id,
            },
            message_id,
            delay,
            tr!("undo-message-junk-pending", { seconds: delay }),
            tr!("undo-message-junk-started"),
            tr!("undo-message-junk-cancelled"),
            window,
            cx,
        );
    }

    pub(crate) fn bulk_mark_junk_with_undo(
        &mut self,
        references: Vec<MessageRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = references.len();
        let delay = self.action_delay_secs();
        let commands = references
            .iter()
            .map(|reference| Cmd::MoveMessage {
                account_id: reference.account_id.clone(),
                message_id: reference.id.clone(),
                source_folder_id: None,
                target_folder_id: self.junk_target_folder_id(&reference.account_id),
            })
            .collect();
        self.bulk_remove_messages_with_undo(
            commands,
            references,
            delay,
            tr!("undo-bulk-junk-pending", { count: count, seconds: delay }),
            tr!("undo-bulk-junk-started", { count: count }),
            tr!("undo-bulk-junk-cancelled", { count: count }),
            tr!("undo-bulk-junk-completed", { count: count }),
            window,
            cx,
        );
    }

    /// The reverse action, and the one place a junk source folder *is* reported:
    /// Gmail needs `SPAM` dropped explicitly, or the message would land back in
    /// the inbox while its own UI still calls it spam.
    pub(crate) fn mark_not_junk_with_undo(
        &mut self,
        account_id: AccountId,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source_folder_id = self.junk_target_folder_id(&account_id);
        let target_folder_id = self.inbox_target_folder_id(&account_id);
        let delay = self.action_delay_secs();
        self.remove_message_with_undo(
            Cmd::MoveMessage {
                account_id,
                message_id: message_id.to_string(),
                source_folder_id: Some(source_folder_id),
                target_folder_id,
            },
            message_id,
            delay,
            tr!("undo-message-not-junk-pending", { seconds: delay }),
            tr!("undo-message-not-junk-started"),
            tr!("undo-message-not-junk-cancelled"),
            window,
            cx,
        );
    }

    pub(crate) fn bulk_mark_not_junk_with_undo(
        &mut self,
        references: Vec<MessageRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = references.len();
        let delay = self.action_delay_secs();
        let commands = references
            .iter()
            .map(|reference| Cmd::MoveMessage {
                account_id: reference.account_id.clone(),
                message_id: reference.id.clone(),
                source_folder_id: Some(self.junk_target_folder_id(&reference.account_id)),
                target_folder_id: self.inbox_target_folder_id(&reference.account_id),
            })
            .collect();
        self.bulk_remove_messages_with_undo(
            commands,
            references,
            delay,
            tr!("undo-bulk-not-junk-pending", { count: count, seconds: delay }),
            tr!("undo-bulk-not-junk-started", { count: count }),
            tr!("undo-bulk-not-junk-cancelled", { count: count }),
            tr!("undo-bulk-not-junk-completed", { count: count }),
            window,
            cx,
        );
    }

    fn inbox_target_folder_id(&self, account_id: &AccountId) -> String {
        self.well_known_folder_id(account_id, "inbox", crate::providers::INBOX_FOLDER_ALIAS)
    }

    /// Archiving deliberately reports no source folder. Graph and IMAP ignore
    /// it, and for Gmail the source is the label to drop: passing the folder
    /// currently on screen would remove *that* label and leave the message in
    /// the inbox — the opposite of archiving. `None` makes the backend drop
    /// `INBOX`, which is what archiving means for every provider.
    pub(crate) fn archive_message_with_undo(
        &mut self,
        account_id: AccountId,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_folder_id = self.archive_target_folder_id(&account_id);
        let delay = self.action_delay_secs();
        self.remove_message_with_undo(
            Cmd::MoveMessage {
                account_id,
                message_id: message_id.to_string(),
                source_folder_id: None,
                target_folder_id,
            },
            message_id,
            delay,
            tr!("undo-message-archive-pending", { seconds: delay }),
            tr!("undo-message-archive-started"),
            tr!("undo-message-archive-cancelled"),
            window,
            cx,
        );
    }

    pub(crate) fn bulk_archive_messages_with_undo(
        &mut self,
        references: Vec<MessageRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = references.len();
        let delay = self.action_delay_secs();
        let commands = references
            .iter()
            .map(|reference| Cmd::MoveMessage {
                account_id: reference.account_id.clone(),
                message_id: reference.id.clone(),
                source_folder_id: None,
                target_folder_id: self.archive_target_folder_id(&reference.account_id),
            })
            .collect();
        self.bulk_remove_messages_with_undo(
            commands,
            references,
            delay,
            tr!("undo-bulk-archive-pending", { count: count, seconds: delay }),
            tr!("undo-bulk-archive-started", { count: count }),
            tr!("undo-bulk-archive-cancelled", { count: count }),
            tr!("undo-bulk-archive-completed", { count: count }),
            window,
            cx,
        );
    }

    /// Removes the message from the UI immediately, then restores it if the
    /// deletion is cancelled during the configured undo window.
    pub(crate) fn delete_message_with_undo(
        &mut self,
        account_id: AccountId,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = self.action_delay_secs();
        self.remove_message_with_undo(
            Cmd::DeleteMessage {
                account_id,
                id: message_id.to_string(),
            },
            message_id,
            delay,
            tr!("undo-message-delete-pending", { seconds: delay }),
            tr!("undo-message-delete-started"),
            tr!("undo-message-delete-cancelled"),
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_message_with_undo(
        &mut self,
        command: Cmd,
        message_id: &str,
        delay: u32,
        pending_message: SharedString,
        started_message: SharedString,
        canceled_message: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_displayed = match self.mailbox.active_tab {
            Some(index) => self
                .mailbox
                .open_tabs
                .get(index)
                .and_then(|tab| tab.message())
                .is_some_and(|message| message.header.id == message_id),
            None => self.mailbox.selected_id.as_deref() == Some(message_id),
        };
        let neighbor = was_displayed
            .then(|| self.message_neighbor_after_removal(message_id))
            .flatten();
        let removal = self.remove_message_optimistically(message_id);
        if was_displayed {
            if let Some(message) = neighbor {
                self.open_message(message.account_id, message.id, cx);
            } else {
                // Final visible message: no new body will replace
                // the one that just disappeared.
                self.cancel_pending_message_open(cx);
            }
        }
        self.schedule_action(
            vec![command],
            delay,
            pending_message,
            started_message,
            canceled_message,
            None,
            PendingCancelEffect::MessageRemoved(Box::new(removal)),
            window,
            cx,
        );
    }

    pub(crate) fn bulk_move_messages_with_undo(
        &mut self,
        references: Vec<MessageRef>,
        source_folder_id: Option<String>,
        target_folder_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = references.len();
        let delay = self.action_delay_secs();
        let commands = references
            .iter()
            .map(|reference| Cmd::MoveMessage {
                account_id: reference.account_id.clone(),
                message_id: reference.id.clone(),
                source_folder_id: source_folder_id.clone(),
                target_folder_id: target_folder_id.clone(),
            })
            .collect();
        self.bulk_remove_messages_with_undo(
            commands,
            references,
            delay,
            tr!("undo-bulk-move-pending", { count: count, seconds: delay }),
            tr!("undo-bulk-move-started", { count: count }),
            tr!("undo-bulk-move-cancelled", { count: count }),
            tr!("undo-bulk-move-completed", { count: count }),
            window,
            cx,
        );
    }

    pub(crate) fn bulk_delete_messages_with_undo(
        &mut self,
        references: Vec<MessageRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = references.len();
        let delay = self.action_delay_secs();
        let commands = references
            .iter()
            .map(|reference| Cmd::DeleteMessage {
                account_id: reference.account_id.clone(),
                id: reference.id.clone(),
            })
            .collect();
        self.bulk_remove_messages_with_undo(
            commands,
            references,
            delay,
            tr!("undo-bulk-delete-pending", { count: count, seconds: delay }),
            tr!("undo-bulk-delete-started", { count: count }),
            tr!("undo-bulk-delete-cancelled", { count: count }),
            tr!("undo-bulk-delete-completed", { count: count }),
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn bulk_remove_messages_with_undo(
        &mut self,
        commands: Vec<Cmd>,
        references: Vec<MessageRef>,
        delay: u32,
        pending_message: SharedString,
        started_message: SharedString,
        canceled_message: SharedString,
        completed_message: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if commands.is_empty() || references.is_empty() {
            return;
        }
        let selected = self.mailbox.selected_messages.clone();
        let displayed_reference = self
            .displayed_message()
            .map(|message| MessageRef::from(message.as_ref()))
            .or_else(|| {
                let id = self.mailbox.selected_id.clone()?;
                let header = self
                    .mailbox
                    .messages
                    .iter()
                    .find(|message| message.id == id)?;
                Some(MessageRef {
                    account_id: header.account_id.clone(),
                    id,
                })
            });
        let neighbor = displayed_reference
            .as_ref()
            .filter(|reference| references.contains(reference))
            .and_then(|reference| self.message_neighbor_after_bulk_removal(reference, &references));
        let removals = references
            .iter()
            .map(|reference| self.remove_message_optimistically_ref(reference))
            .collect();
        self.mailbox.selected_messages.clear();
        self.mailbox.selection_anchor = None;
        if displayed_reference
            .as_ref()
            .is_some_and(|reference| references.contains(reference))
        {
            if let Some(message) = neighbor {
                self.open_message(message.account_id, message.id, cx);
            } else {
                self.cancel_pending_message_open(cx);
            }
        }
        self.schedule_action(
            commands,
            delay,
            pending_message,
            started_message,
            canceled_message,
            Some(completed_message),
            PendingCancelEffect::MessagesRemoved { removals, selected },
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_tag_undoable(
        &mut self,
        command: Cmd,
        account_id: &AccountId,
        message_id: &str,
        tag_id: &str,
        added: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let header_tags = self
            .mailbox
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .map(|message| message.tags.clone())
            .or_else(|| {
                self.mailbox
                    .search
                    .results
                    .as_ref()
                    .and_then(|messages| messages.iter().find(|message| message.id == message_id))
                    .map(|message| message.tags.clone())
            })
            .or_else(|| match &self.sender_history {
                SenderHistoryState::Loaded { messages, .. } => messages
                    .iter()
                    .find(|message| message.id == message_id)
                    .map(|message| message.tags.clone()),
                _ => None,
            })
            .or_else(|| {
                self.mailbox
                    .selected
                    .as_ref()
                    .filter(|message| message.header.id == message_id)
                    .map(|message| message.header.tags.clone())
            })
            .unwrap_or_default();
        let message_tags = self
            .mailbox
            .selected
            .as_ref()
            .filter(|message| message.header.id == message_id)
            .map(|message| message.tags.clone());
        let key = self
            .tags_by_account
            .get(account_id)
            .and_then(|tags| tags.iter().find(|tag| tag.id == tag_id))
            .map(|tag| {
                let provider = self
                    .account(account_id)
                    .map(|account| account.provider)
                    .unwrap_or_default();
                crate::ui::util::tag_storage_key(provider, tag)
            })
            .unwrap_or_else(|| tag_id.to_string());
        self.apply_message_tag(message_id, &key, added);
        let delay = self.action_delay_secs();
        self.schedule_action(
            vec![command],
            delay,
            tr!("undo-tags-pending", { seconds: delay }),
            tr!("undo-tags-started"),
            tr!("undo-tags-cancelled"),
            None,
            PendingCancelEffect::MessageTags {
                message_id: message_id.to_string(),
                header_tags,
                message_tags,
            },
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn move_kanban_undoable(
        &mut self,
        commands: Vec<Cmd>,
        account_id: AccountId,
        message: MessageHeader,
        source_tag_id: String,
        target_tag_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = self.action_delay_secs();
        self.schedule_action(
            commands,
            delay,
            tr!("undo-card-move-pending", { seconds: delay }),
            tr!("undo-card-move-started"),
            tr!("undo-card-move-cancelled"),
            None,
            PendingCancelEffect::KanbanMove {
                account_id,
                message: Box::new(message),
                source_tag_id,
                target_tag_id,
            },
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn schedule_action(
        &mut self,
        commands: Vec<Cmd>,
        delay_secs: u32,
        pending_message: SharedString,
        started_message: SharedString,
        canceled_message: SharedString,
        completed_message: Option<SharedString>,
        cancel_effect: PendingCancelEffect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SharedString {
        self.pending_action_seq = self.pending_action_seq.wrapping_add(1);
        let action_id = self.pending_action_seq;
        let notification_key = match commands.as_slice() {
            [command] => Self::command_message_mutation(command)
                .map(|(kind, account_id, message_id)| {
                    Self::message_mutation_notification_key(kind, account_id, message_id)
                })
                .unwrap_or_else(|| format!("pending-action:{action_id}").into()),
            _ => format!("pending-action:{action_id}").into(),
        };
        // The batch is registered when the commands are actually submitted,
        // never when they are scheduled: an action cancelled inside its undo
        // window sends nothing, and a batch waiting on replies that will never
        // come would swallow the reply of whatever the user does next to the
        // same message.
        let references = Self::command_message_references(&commands);
        if delay_secs == 0 {
            self.begin_bulk_completion(references, completed_message, notification_key.clone(), cx);
            for command in commands {
                self.send(command);
            }
            return notification_key;
        }

        self.pending_actions.insert(
            action_id,
            PendingAction {
                commands,
                notification_key: notification_key.clone(),
                started_message,
                canceled_message,
                cancel_effect,
            },
        );

        let app = cx.entity().downgrade();
        window.push_notification(
            Notification::info(pending_message)
                .id1::<PendingActionNotification>(notification_key.clone())
                .autohide(false)
                .action(move |_, _, _| {
                    let app = app.clone();
                    Button::new("cancel-pending-action")
                        .ghost()
                        .label(tr!("cancel"))
                        .on_click(move |_, window, cx| {
                            let canceled = app
                                .update(cx, |this, cx| {
                                    let action = this.pending_actions.remove(&action_id)?;
                                    let notification_key = action.notification_key.clone();
                                    match action.cancel_effect {
                                        PendingCancelEffect::None => {}
                                        PendingCancelEffect::Compose { compose_id } => {
                                            if let Some(compose) = this
                                                .composes
                                                .iter()
                                                .find(|handle| handle.id == compose_id)
                                            {
                                                let _ = compose.view.update(cx, |view, cx| {
                                                    view.cancel_pending_send(cx);
                                                });
                                            }
                                            this.restore_compose_surface(compose_id, cx);
                                        }
                                        PendingCancelEffect::MessageFlags(previous) => {
                                            for (reference, flagged) in previous {
                                                this.update_header_for(&reference, |header| {
                                                    MessageState::Flagged.apply(header, flagged);
                                                });
                                            }
                                        }
                                        PendingCancelEffect::MessageReads(previous) => {
                                            for (reference, read) in previous {
                                                this.update_header_for(&reference, |header| {
                                                    MessageState::Read.apply(header, read);
                                                });
                                            }
                                        }
                                        PendingCancelEffect::MessageTags {
                                            message_id,
                                            header_tags,
                                            message_tags,
                                        } => {
                                            this.update_header(&message_id, |header| {
                                                header.tags.clone_from(&header_tags);
                                            });
                                            if let (Some(message), Some(tags)) = (
                                                this.mailbox.selected_mut().filter(|message| {
                                                    message.header.id == message_id
                                                }),
                                                message_tags,
                                            ) {
                                                message.tags = tags;
                                            }
                                        }
                                        PendingCancelEffect::MessageRemoved(removal) => {
                                            this.restore_optimistic_message(*removal);
                                        }
                                        PendingCancelEffect::MessagesRemoved {
                                            removals,
                                            selected,
                                        } => {
                                            for removal in removals.into_iter().rev() {
                                                this.restore_optimistic_message(removal);
                                            }
                                            this.mailbox.selected_messages = selected;
                                        }
                                        PendingCancelEffect::KanbanMove {
                                            account_id,
                                            message,
                                            source_tag_id,
                                            target_tag_id,
                                        } => {
                                            if let Some(board) =
                                                this.kanban.account_mut(&account_id)
                                            {
                                                if let Some(target) = board
                                                    .columns
                                                    .iter_mut()
                                                    .find(|column| column.tag_id == target_tag_id)
                                                {
                                                    target
                                                        .messages
                                                        .retain(|item| item.id != message.id);
                                                }
                                                if let Some(source) = board
                                                    .columns
                                                    .iter_mut()
                                                    .find(|column| column.tag_id == source_tag_id)
                                                {
                                                    if !source
                                                        .messages
                                                        .iter()
                                                        .any(|item| item.id == message.id)
                                                    {
                                                        source.messages.insert(0, *message);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    cx.notify();
                                    Some((action.canceled_message, notification_key))
                                })
                                .ok()
                                .flatten();
                            if let Some((message, notification_key)) = canceled {
                                window.push_notification(
                                    Notification::success(message)
                                        .id1::<PendingActionNotification>(notification_key),
                                    cx,
                                );
                            }
                        })
                }),
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(delay_secs.into()))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(action) = this.pending_actions.remove(&action_id) else {
                    return;
                };
                this.begin_bulk_completion(
                    references,
                    completed_message,
                    action.notification_key.clone(),
                    cx,
                );
                for command in action.commands {
                    this.send(command);
                }
                window.push_notification(
                    Notification::info(action.started_message)
                        .id1::<PendingActionNotification>(action.notification_key),
                    cx,
                );
            });
        })
        .detach();
        notification_key
    }

    pub(super) fn remove_message_optimistically(&mut self, id: &str) -> OptimisticMessageRemoval {
        self.remove_message_optimistically_matching(None, id)
    }

    pub(super) fn remove_message_optimistically_ref(
        &mut self,
        reference: &MessageRef,
    ) -> OptimisticMessageRemoval {
        self.remove_message_optimistically_matching(Some(&reference.account_id), &reference.id)
    }

    pub(super) fn remove_message_optimistically_matching(
        &mut self,
        account_id: Option<&AccountId>,
        id: &str,
    ) -> OptimisticMessageRemoval {
        fn take_header(
            list: &mut Vec<MessageHeader>,
            account_id: Option<&AccountId>,
            id: &str,
        ) -> Option<(usize, MessageHeader)> {
            let index = list.iter().position(|message| {
                message.id == id
                    && account_id.is_none_or(|account_id| &message.account_id == account_id)
            })?;
            Some((index, list.remove(index)))
        }

        let mailbox = take_header(&mut self.mailbox.messages, account_id, id);
        let search = self
            .mailbox
            .search
            .results
            .as_mut()
            .and_then(|messages| take_header(messages, account_id, id));
        let sender_history = match &mut self.sender_history {
            SenderHistoryState::Loaded { messages, .. } => take_header(messages, account_id, id),
            _ => None,
        };
        let selection_matches = self.mailbox.selected_id.as_deref() == Some(id)
            && account_id.is_none_or(|account_id| {
                self.mailbox
                    .selected
                    .as_ref()
                    .is_none_or(|message| &message.header.account_id == account_id)
            });
        let selection = selection_matches.then(|| {
            let selected = self.mailbox.selected.take();
            self.mailbox.selected_id = None;
            let thread = self.mailbox.thread.take();
            OptimisticSelection {
                message_id: id.to_string(),
                message: selected,
                thread,
            }
        });
        let open_tab = self
            .mailbox
            .open_tabs
            .iter()
            .position(|tab| {
                tab.message().is_some_and(|message| {
                    message.header.id == id
                        && account_id
                            .is_none_or(|account_id| &message.header.account_id == account_id)
                })
            })
            .and_then(|index| {
                let was_active = self.mailbox.active_tab == Some(index);
                let tab = self.mailbox.open_tabs.remove(index);
                self.mailbox.active_tab = match self.mailbox.active_tab {
                    Some(active) if active == index => None,
                    Some(active) if active > index => Some(active - 1),
                    other => other,
                };
                tab.shared_message()
                    .cloned()
                    .map(|message| (index, message, was_active))
            });
        self.mailbox.selected_messages.retain(|reference| {
            reference.id != id
                || account_id.is_some_and(|account_id| &reference.account_id != account_id)
        });
        if self
            .mailbox
            .selection_anchor
            .as_ref()
            .is_some_and(|reference| {
                reference.id == id
                    && account_id.is_none_or(|account_id| &reference.account_id == account_id)
            })
        {
            self.mailbox.selection_anchor = None;
        }
        self.refresh_sender_history_for_displayed();
        self.update_tray_unread();
        self.invalidate_message_list();

        OptimisticMessageRemoval {
            mailbox,
            search,
            sender_history,
            selection,
            open_tab,
        }
    }

    pub(super) fn restore_optimistic_message(&mut self, removal: OptimisticMessageRemoval) {
        fn restore_header(list: &mut Vec<MessageHeader>, removed: Option<(usize, MessageHeader)>) {
            if let Some((index, message)) = removed {
                if !list.iter().any(|current| current.id == message.id) {
                    list.insert(index.min(list.len()), message);
                }
            }
        }

        restore_header(&mut self.mailbox.messages, removal.mailbox);
        if let Some(messages) = &mut self.mailbox.search.results {
            restore_header(messages, removal.search);
        }
        if let SenderHistoryState::Loaded { messages, .. } = &mut self.sender_history {
            restore_header(messages, removal.sender_history);
        }
        if let Some(selection) = removal.selection {
            if self.mailbox.selected_id.is_none() {
                self.mailbox.selected_id = Some(selection.message_id);
                self.mailbox.selected = selection.message;
                self.mailbox.thread = selection.thread;
            }
        }
        if let Some((index, message, was_active)) = removal.open_tab {
            if !self.mailbox.open_tabs.iter().any(|tab| {
                tab.message()
                    .is_some_and(|current| current.header.id == message.header.id)
            }) {
                let index = index.min(self.mailbox.open_tabs.len());
                if let Some(active) = self.mailbox.active_tab.as_mut() {
                    if *active >= index {
                        *active += 1;
                    }
                } else if was_active {
                    self.mailbox.active_tab = Some(index);
                }
                self.mailbox
                    .open_tabs
                    .insert(index, crate::ui::state::ViewerTab::Message(message));
            }
        }
        self.refresh_sender_history_for_displayed();
        self.update_tray_unread();
        self.invalidate_message_list();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn header(id: &str, is_read: bool) -> MessageHeader {
        MessageHeader {
            id: id.to_string(),
            account_id: AccountId("account-a".into()),
            subject: String::new(),
            from: String::new(),
            received: Utc::now(),
            preview: String::new(),
            is_read,
            is_flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: Some("conversation-1".to_string()),
            internet_message_id: None,
        }
    }

    fn reference(id: &str) -> MessageRef {
        MessageRef {
            account_id: AccountId("account-a".into()),
            id: id.to_string(),
        }
    }

    /// Opening a thread reads what the row stood for — not the message the
    /// runtime already reads on open, and not what was read already.
    #[test]
    fn only_the_unread_members_the_click_did_not_open_are_marked() {
        let headers = [
            header("newest", false),
            header("older", false),
            header("read", true),
        ];
        let members = [reference("newest"), reference("older"), reference("read")];

        let unread = unread_conversation_members(&headers, &members, "newest");

        assert_eq!(unread, vec![reference("older")]);
    }

    /// Thread ids are only comparable inside one account, and the inbox may be
    /// unified: a member is matched on both halves of its reference.
    #[test]
    fn a_member_of_another_account_is_not_matched() {
        let headers = [header("shared-id", false)];
        let members = [MessageRef {
            account_id: AccountId("account-b".into()),
            id: "shared-id".to_string(),
        }];

        assert!(unread_conversation_members(&headers, &members, "newest").is_empty());
    }
}
