//! UI state types owned by `AviaryApp` (see `app.rs`). Pure data only: no
//! rendering logic or runtime access here.

use crate::model::{AccountId, Message, MessageHeader, MessageRef, SentMessage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub enum AuthState {
    Idle,
    StartingMicrosoft,
    AwaitingCode {
        user_code: String,
        verification_uri: String,
    },
    AwaitingGoogle {
        auth_url: String,
    },
    AwaitingImap {
        email: String,
    },
    Authenticated,
}

impl AuthState {
    pub fn is_in_progress(&self) -> bool {
        matches!(
            self,
            Self::StartingMicrosoft
                | Self::AwaitingCode { .. }
                | Self::AwaitingGoogle { .. }
                | Self::AwaitingImap { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MainView {
    #[default]
    Mail,
    Calendar,
    Kanban,
    Contacts,
    Settings,
}

#[derive(Clone)]
pub enum ThreadBodyState {
    Loading,
    Loaded(Box<Message>),
    Error(String),
}

/// How far the message list has been paginated, and what it is waiting for.
#[derive(Default)]
pub struct MailPagination {
    /// The provider has more pages for the current listing.
    pub has_more: bool,
    pub loading_more: bool,
    /// Number of messages present during the latest automatic pagination.
    /// Prevents repeatedly loading a successful page that added no rows.
    pub last_request_len: Option<usize>,
    /// Runtime paginator generation for the all-mailboxes view. Responses from
    /// an earlier generation are ignored after a scope change or refresh.
    pub unified_request_id: u64,
}

/// What the mail search box holds.
///
/// The submitted query and how it is scoped and sorted, the hits it produced,
/// the history the menu offers, and the ephemeral state of that menu. Kept
/// apart from the list's own filters (flagged, tags), which apply whether or
/// not a search is active.
#[derive(Default)]
pub struct MailSearchState {
    /// Identifies one submission, including a repeat of the same query.
    pub request_id: u64,
    pub query: String,
    pub scope: crate::ui::settings::MailSearchScope,
    pub sort: crate::ui::settings::MailSearchSort,
    /// `None` while no search is running — which is not the same as a search
    /// that returned nothing.
    pub results: Option<Vec<MessageHeader>>,
    /// Most recently submitted searches, newest first. Persisted in the
    /// lightweight UI session so the menu survives a restart.
    pub history: Vec<String>,
    /// Whether the Outlook-style suggestions panel under the input is visible.
    /// Intentionally ephemeral.
    pub menu_open: bool,
    /// Keyboard-highlighted row in the flattened contact/history suggestions.
    pub menu_selection: Option<usize>,
}

impl MailSearchState {
    pub(crate) fn begin(&mut self, query: String) {
        self.request_id = self.request_id.wrapping_add(1);
        self.query = query;
        self.results = Some(Vec::new());
    }

    pub(crate) fn accepts_results(&self, request_id: u64, query: &str) -> bool {
        self.results.is_some() && self.request_id == request_id && self.query == query
    }
}

#[derive(Default)]
pub struct MailboxState {
    pub messages: Vec<MessageHeader>,
    /// Distinguishes a genuinely loaded mailbox from an empty mailbox that has
    /// not made any call yet. Allows returning to Mail without
    /// repeatedly reload the same list.
    pub messages_loaded: bool,
    /// Message the list selection points at. A full reference rather than a
    /// bare id: IMAP ids (`INBOX:42`) repeat from one account to the next, so
    /// an id alone would let another account's reply land in the reader.
    pub selected_id: Option<MessageRef>,
    /// Shared: the reader hands the displayed message to a dozen sub-elements
    /// on every frame, and a `Message` owns its body, its inline images and
    /// its attachment bytes — copying that per frame is megabytes of churn
    /// behind a hover transition. Mutations go through `Rc::make_mut`.
    pub selected: Option<Rc<Message>>,
    /// The message the reader showed before the current selection, kept on
    /// screen — inert — until the selection can replace it in one step: its
    /// data arrived, its body is rendered and its thread is in (see
    /// [`linger_step`]). Swapping as soon as the data arrived showed the new
    /// header over a body still rendering — a blank flash between two
    /// messages. Render-only: `displayed_message` never returns it, so no
    /// action, shortcut or menu can target a message the user has already
    /// left. While it is set, a `selected` message exists only off screen.
    /// Dropped when the selection is revealed, after
    /// `LINGERING_SELECTION_GRACE`, and wherever the selection is abandoned.
    pub lingering_selected: Option<LingeringSelection>,
    /// Conversation already requested for the current selection, keyed like
    /// every thread: `(account, conversation)`. The cached open and the
    /// provider open both carry it, and one request is enough — a second
    /// would abort the first mid-flight and restart from the cache.
    pub thread_requested: Option<(AccountId, String)>,
    /// Explicit multi-selection in the message list. The primary reader
    /// selection remains separate so opening a message does not make every
    /// single-message action look like a bulk operation.
    pub selected_messages: HashSet<MessageRef>,
    /// Last row used as the range-selection anchor for Shift+click.
    pub selection_anchor: Option<MessageRef>,
    pub thread: Option<(String, Vec<MessageHeader>)>,
    /// Bodies of expanded thread entries, keyed by account *and* id for the
    /// same reason as `selected_id`.
    pub thread_bodies: HashMap<MessageRef, ThreadBodyState>,
    /// Quoted sub-messages explicitly expanded by the user. Others remain
    /// collapsed by default, Outlook-style.
    pub expanded_quoted_sections: HashSet<String>,
    /// Replies/forwards accepted during this or a restored app session,
    /// grouped by their original provider message id.
    pub sent_messages: HashMap<String, Vec<SentMessage>>,
    /// Synthetic sent-message ids currently expanded above their source.
    pub expanded_sent_messages: HashSet<String>,
    pub search: MailSearchState,
    /// Show only flagged messages (Outlook flag, Gmail star, or IMAP
    /// `\\Flagged`). This filter remains independent of pinning.
    pub show_flagged_only: bool,
    /// Show only messages put off until later, which are otherwise the one
    /// category hidden from every list. It is the only way to see a pending
    /// deadline, so it doubles as the review screen for them.
    pub show_snoozed_only: bool,
    /// Selected tag identifiers by account. Multiple tags from one account are
    /// combined with AND; in unified view, selected accounts use OR.
    pub tag_filters: HashMap<AccountId, HashSet<String>>,
    pub pagination: MailPagination,
    pub folders: Vec<crate::model::MailFolder>,
    pub folders_by_account: HashMap<AccountId, Vec<crate::model::MailFolder>>,
    pub selected_folder_id: Option<String>,
    /// In unified mode: `None` means all mailboxes merged; `Some(aid)` scopes
    /// to one account, optionally with `selected_folder_id`.
    pub unified_selected_account: Option<AccountId>,
    pub last_auto_refresh_sent: Option<(Option<String>, u32, usize)>,
    pub refresh_pending: bool,
    /// Collapsed sections in the message list (`pinned` or a local
    /// `day:YYYY-MM-DD` date). State remains stable across refreshes and searches.
    pub collapsed_message_sections: HashSet<String>,
    /// Conversation groups the user has expanded. Groups start collapsed, so
    /// this set stays small; the key is the same `(account, conversation)`
    /// pair the grouping uses, since thread ids are only unique per account.
    pub expanded_conversations: HashSet<(AccountId, String)>,
    /// Messages the local cache knows of per thread, for the folder on
    /// screen. Answers "how big is this thread" beyond the loaded pages —
    /// see `Evt::ConversationTotals`.
    pub conversation_totals: HashMap<(AccountId, String), usize>,
    /// Reader-pane tabs: pinned messages and active composers.
    pub open_tabs: Vec<ViewerTab>,
    /// Displayed tab; `None` means the list selection.
    pub active_tab: Option<usize>,
}

impl MailboxState {
    /// Leaves search mode and reports whether the regular mailbox listing
    /// still needs its first load. This happens when the application starts
    /// with a search restored from the previous session.
    pub(crate) fn clear_search(&mut self) -> bool {
        self.search.query.clear();
        self.search.results = None;
        !self.messages_loaded
    }

    /// Mutable access to the reader selection, detaching it from any handle
    /// the current frame still holds.
    pub(crate) fn selected_mut(&mut self) -> Option<&mut Message> {
        self.selected.as_mut().map(Rc::make_mut)
    }

    /// The message to paint, inert, in place of the selection: only when the
    /// reader shows the list selection. Whether the selection has arrived
    /// underneath is for [`linger_step`] to weigh.
    pub(crate) fn lingering_for_render(&self) -> Option<Rc<Message>> {
        if self.active_tab.is_some() || self.selected_id.is_none() {
            return None;
        }
        self.lingering_selected
            .as_ref()
            .map(|lingering| lingering.message.clone())
    }

    /// Drops the lingering message if it is this one — deleted, moved or
    /// otherwise gone, it must not stay on screen even for the grace period.
    /// `account_id: None` matches any account, like the optimistic removals
    /// that call it.
    pub(crate) fn forget_lingering(&mut self, account_id: Option<&AccountId>, id: &str) {
        if self.lingering_selected.as_ref().is_some_and(|lingering| {
            lingering.message.header.id == id
                && account_id
                    .is_none_or(|account_id| &lingering.message.header.account_id == account_id)
        }) {
            self.lingering_selected = None;
        }
    }

    /// The list selection is about to change: decides what stays on screen
    /// meanwhile, and returns whether a new lingering period starts (the
    /// caller then arms its time cap).
    ///
    /// What lingers is always the message *the screen shows*: when something
    /// already lingers, the selection behind it never reached the screen —
    /// j/k passing over it — and is simply dropped, the lingering message and
    /// its original deadline staying put. A burst of j/k therefore freezes
    /// the pane for one cap at most, counted from the first key, instead of
    /// restarting it at every row.
    pub(crate) fn leave_selection(&mut self, now: Instant) -> bool {
        let previous = self.selected.take();
        if self.active_tab.is_some() {
            // A tab being left is not what the list replaces.
            self.lingering_selected = None;
            return false;
        }
        match (&self.lingering_selected, previous) {
            (None, Some(previous)) => {
                self.lingering_selected = Some(LingeringSelection {
                    message: previous,
                    thread: None,
                    since: now,
                });
                true
            }
            _ => false,
        }
    }

    /// First arrival of a selection: the thread in place stays only if it is
    /// the new message's conversation, within the selection's account.
    /// Otherwise, a lingering message that still draws with it takes it
    /// along — thread *and* expanded cards — since the new selection's
    /// thread is about to replace them while it is still on screen.
    pub(crate) fn settle_thread_for(&mut self, message: &MessageHeader) {
        // The selection's account, not the header's: a header read back
        // from the cache is not guaranteed to carry it.
        let account_id = self
            .selected_id
            .as_ref()
            .map(|reference| &reference.account_id);
        let same_thread = self.thread.as_ref().is_some_and(|(id, thread)| {
            message.conversation_id.as_deref() == Some(id.as_str())
                && thread
                    .iter()
                    .all(|header| Some(&header.account_id) == account_id)
        });
        if same_thread {
            return;
        }
        match self
            .lingering_selected
            .as_mut()
            .filter(|lingering| lingering.thread.is_none())
        {
            Some(lingering) => {
                lingering.thread = Some(LingeringThread {
                    thread: self.thread.take(),
                    bodies: std::mem::take(&mut self.thread_bodies),
                });
            }
            None => self.thread = None,
        }
    }

    /// Whether the selection's thread was asked for and has not arrived. Its
    /// replies are drawn *above* the body: revealing the message before them
    /// would push the body down a moment later.
    pub(crate) fn selection_thread_pending(&self) -> bool {
        let (Some(selected), Some(reference)) = (&self.selected, &self.selected_id) else {
            return false;
        };
        let Some(conversation_id) = selected.header.conversation_id.as_deref() else {
            return false;
        };
        let requested = self
            .thread_requested
            .as_ref()
            .is_some_and(|(account_id, requested)| {
                account_id == &reference.account_id && requested == conversation_id
            });
        requested
            && !self.thread.as_ref().is_some_and(|(id, thread)| {
                id == conversation_id
                    && thread
                        .iter()
                        .all(|header| header.account_id == reference.account_id)
            })
    }

    /// Installs a thread for the selection and reports whether anything
    /// changed. The same conversation arrives twice per open — from the cache,
    /// then from the provider — and again on every refresh: none of those may
    /// collapse the cards the user expanded, and an identical list must not
    /// cost a render. Only a different conversation starts from scratch.
    pub(crate) fn apply_thread(
        &mut self,
        account_id: &AccountId,
        conversation_id: String,
        messages: Vec<MessageHeader>,
    ) -> bool {
        match thread_update(
            self.thread.as_ref(),
            account_id,
            &conversation_id,
            &messages,
        ) {
            ThreadUpdate::Unchanged => return false,
            ThreadUpdate::Refresh => {
                let members: HashSet<MessageRef> = messages
                    .iter()
                    .map(|header| MessageRef {
                        account_id: header.account_id.clone(),
                        id: header.id.clone(),
                    })
                    .collect();
                self.thread_bodies
                    .retain(|reference, _| members.contains(reference));
            }
            ThreadUpdate::Replace => self.thread_bodies.clear(),
        }
        self.thread = Some((conversation_id, messages));
        true
    }

    /// Whether the list selection is this exact message.
    pub(crate) fn is_selected(&self, account_id: &AccountId, id: &str) -> bool {
        self.selected_id
            .as_ref()
            .is_some_and(|selected| &selected.account_id == account_id && selected.id == id)
    }
}

/// A message the reader keeps painting while the selection that replaces it
/// gets ready (`MailboxState::lingering_selected`).
pub struct LingeringSelection {
    pub message: Rc<Message>,
    /// The thread and expanded cards it is drawn with, once the selection's
    /// own have replaced them in `MailboxState`; `None` while both share
    /// those of `MailboxState` (same conversation, or nothing replaced yet).
    pub thread: Option<LingeringThread>,
    /// When the user first left it: the time cap runs from here, whatever
    /// j/k did since.
    pub since: Instant,
}

pub struct LingeringThread {
    pub thread: Option<(String, Vec<MessageHeader>)>,
    pub bodies: HashMap<MessageRef, ThreadBodyState>,
}

/// Where the selection coming in stands, as far as revealing it goes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IncomingSelection {
    /// A body element built for it has nothing to show yet: no tiles, no
    /// error. Always `false` for a body that does not go through Blitz.
    pub body_pending: bool,
    /// See [`MailboxState::selection_thread_pending`].
    pub thread_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LingerStep {
    /// Keep painting the message being left; the selection stays off screen.
    Hold,
    /// Put the selection on screen now, ready or not.
    Reveal,
}

/// Whether the reader may swap the lingering message for the selection this
/// frame. The swap waits until nothing the selection draws would still
/// change — its data (`incoming: None` while it has not arrived), its body
/// and its thread — so that the pane goes from one complete message to the
/// next. `cap` bounds the wait from the first time the user left the
/// lingering message: a slow render or an unreachable server then gets the
/// usual loading states rather than a frozen pane.
pub(crate) fn linger_step(
    elapsed: Duration,
    cap: Duration,
    incoming: Option<IncomingSelection>,
) -> LingerStep {
    if elapsed >= cap {
        return LingerStep::Reveal;
    }
    match incoming {
        Some(incoming) if !incoming.body_pending && !incoming.thread_pending => LingerStep::Reveal,
        _ => LingerStep::Hold,
    }
}

/// What an incoming thread does to the one already in place.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ThreadUpdate {
    /// Same conversation, same headers: nothing to do.
    Unchanged,
    /// Same conversation, different headers: replace the list, keep the
    /// state of the cards still in it.
    Refresh,
    /// Another conversation, or none in place.
    Replace,
}

/// A conversation is the same only within one account — ids mean nothing
/// across accounts, and two IMAP accounts receiving one mail derive the same
/// root `Message-ID` for it.
pub(crate) fn thread_update(
    current: Option<&(String, Vec<MessageHeader>)>,
    account_id: &AccountId,
    conversation_id: &str,
    messages: &[MessageHeader],
) -> ThreadUpdate {
    match current {
        Some((current_id, current_messages))
            if current_id == conversation_id
                && current_messages
                    .iter()
                    .all(|header| &header.account_id == account_id) =>
        {
            if current_messages.as_slice() == messages {
                ThreadUpdate::Unchanged
            } else {
                ThreadUpdate::Refresh
            }
        }
        _ => ThreadUpdate::Replace,
    }
}

/// A reader-pane tab (new Outlook style): a message
/// kept open, or an inline composer identified by `compose_id`, whose entity
/// lives in `AviaryApp.inline_composes`.
pub enum ViewerTab {
    /// Shared for the same reason as `MailboxState::selected`.
    Message(Rc<Message>),
    /// Session-restored tab whose complete message is still loading from
    /// SQLite (and then the provider when absent from cache).
    Loading(MessageRef),
    Compose(u64),
}

impl ViewerTab {
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(m) => Some(m),
            Self::Loading(_) | Self::Compose(_) => None,
        }
    }

    pub fn message_mut(&mut self) -> Option<&mut Message> {
        match self {
            // Copy-on-write: mutating while the reader still holds the frame's
            // handle detaches this tab's copy instead of aliasing it.
            Self::Message(m) => Some(Rc::make_mut(m)),
            Self::Loading(_) | Self::Compose(_) => None,
        }
    }

    /// Shared handle on the displayed message, for callers that only need to
    /// keep it alive (the reader) rather than copy it.
    pub fn shared_message(&self) -> Option<&Rc<Message>> {
        match self {
            Self::Message(m) => Some(m),
            Self::Loading(_) | Self::Compose(_) => None,
        }
    }

    pub fn message_ref(&self) -> Option<MessageRef> {
        match self {
            Self::Message(message) => Some(MessageRef::from(message.as_ref())),
            Self::Loading(reference) => Some(reference.clone()),
            Self::Compose(_) => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading(_))
    }

    pub fn compose_id(&self) -> Option<u64> {
        match self {
            Self::Compose(id) => Some(*id),
            Self::Message(_) | Self::Loading(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{linger_step, IncomingSelection, LingerStep, MailboxState, ThreadBodyState};
    use crate::model::{AccountId, Message, MessageHeader, MessageRef};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    fn thread_header(account: &str, id: &str, is_read: bool) -> MessageHeader {
        MessageHeader {
            id: id.into(),
            account_id: AccountId(account.into()),
            subject: "Contrat".into(),
            from: "Contact A <contact-a@example.test>".into(),
            received: chrono::DateTime::UNIX_EPOCH,
            preview: String::new(),
            is_read,
            is_flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: Some("conversation-1".into()),
            internet_message_id: None,
        }
    }

    fn reference(account: &str, id: &str) -> MessageRef {
        MessageRef {
            account_id: AccountId(account.into()),
            id: id.into(),
        }
    }

    /// The provider's answer after the cache's, or a refresh: expanded cards
    /// survive as long as their message is still in the thread, and an
    /// identical list changes nothing at all.
    #[test]
    fn the_same_conversation_keeps_its_expanded_cards() {
        let account = AccountId("account-a".into());
        let mut mailbox = MailboxState::default();
        let cached = vec![
            thread_header("account-a", "message-a", true),
            thread_header("account-a", "message-b", false),
        ];
        assert!(mailbox.apply_thread(&account, "conversation-1".into(), cached.clone()));
        for id in ["message-a", "message-b"] {
            mailbox
                .thread_bodies
                .insert(reference("account-a", id), super::ThreadBodyState::Loading);
        }

        assert!(!mailbox.apply_thread(&account, "conversation-1".into(), cached));
        assert_eq!(mailbox.thread_bodies.len(), 2);

        // message-b left the thread (deleted elsewhere), message-c joined.
        let provider = vec![
            thread_header("account-a", "message-a", true),
            thread_header("account-a", "message-c", false),
        ];
        assert!(mailbox.apply_thread(&account, "conversation-1".into(), provider));
        assert!(mailbox
            .thread_bodies
            .contains_key(&reference("account-a", "message-a")));
        assert!(!mailbox
            .thread_bodies
            .contains_key(&reference("account-a", "message-b")));
    }

    #[test]
    fn another_conversation_starts_from_scratch() {
        let account = AccountId("account-a".into());
        let other = AccountId("account-b".into());
        let mut mailbox = MailboxState::default();
        mailbox.apply_thread(
            &account,
            "conversation-1".into(),
            vec![thread_header("account-a", "message-a", true)],
        );
        mailbox.thread_bodies.insert(
            reference("account-a", "message-a"),
            super::ThreadBodyState::Loading,
        );

        // Same conversation id, other account: not the same conversation.
        assert_eq!(
            super::thread_update(
                mailbox.thread.as_ref(),
                &other,
                "conversation-1",
                &[thread_header("account-b", "message-a", true)],
            ),
            super::ThreadUpdate::Replace
        );
        assert!(mailbox.apply_thread(
            &account,
            "conversation-2".into(),
            vec![thread_header("account-a", "message-a", true)],
        ));
        assert!(mailbox.thread_bodies.is_empty());
    }

    fn test_message(account: &str, id: &str, conversation: Option<&str>) -> Message {
        let mut header = thread_header(account, id, true);
        header.conversation_id = conversation.map(str::to_string);
        Message {
            header,
            body: String::new(),
            format: crate::model::BodyFormat::Text,
            inline_images: Vec::new(),
            attachments: Vec::new(),
            tags: Vec::new(),
            raw_body: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            draft_id: None,
            invitation: None,
        }
    }

    /// The reader shows `id` as the list selection.
    fn showing(mailbox: &mut MailboxState, account: &str, id: &str, conversation: Option<&str>) {
        mailbox.selected_id = Some(reference(account, id));
        mailbox.selected = Some(Rc::new(test_message(account, id, conversation)));
    }

    const CAP: Duration = Duration::from_millis(400);

    fn ready() -> Option<IncomingSelection> {
        Some(IncomingSelection {
            body_pending: false,
            thread_pending: false,
        })
    }

    /// The swap waits for the data, then the body, then the thread — and for
    /// nothing once the cap has passed.
    #[test]
    fn the_reader_swaps_messages_only_once_the_next_one_is_complete() {
        let early = Duration::from_millis(50);
        assert_eq!(linger_step(early, CAP, None), LingerStep::Hold);
        let rendering = Some(IncomingSelection {
            body_pending: true,
            thread_pending: false,
        });
        assert_eq!(linger_step(early, CAP, rendering), LingerStep::Hold);
        let thread_loading = Some(IncomingSelection {
            body_pending: false,
            thread_pending: true,
        });
        assert_eq!(linger_step(early, CAP, thread_loading), LingerStep::Hold);
        assert_eq!(linger_step(early, CAP, ready()), LingerStep::Reveal);

        for incoming in [None, rendering, thread_loading] {
            assert_eq!(linger_step(CAP, CAP, incoming), LingerStep::Reveal);
        }
    }

    /// A burst of j/k: what lingers is the last message actually on screen,
    /// and the cap keeps counting from the first key rather than restarting.
    #[test]
    fn a_burst_of_navigation_lingers_on_the_message_last_shown() {
        let mut mailbox = MailboxState::default();
        showing(&mut mailbox, "account-a", "message-a", None);
        let first = Instant::now();
        assert!(mailbox.leave_selection(first));
        mailbox.selected_id = Some(reference("account-a", "message-b"));

        // message-b arrives but is still rendering: it never reaches the
        // screen before the next key.
        showing(&mut mailbox, "account-a", "message-b", None);
        assert!(!mailbox.leave_selection(first + Duration::from_millis(30)));
        mailbox.selected_id = Some(reference("account-a", "message-c"));
        let lingering = mailbox
            .lingering_selected
            .as_ref()
            .expect("still lingering");
        assert_eq!(lingering.message.header.id, "message-a");
        assert_eq!(lingering.since, first);
        assert!(mailbox.selected.is_none());
        assert_eq!(
            mailbox.lingering_for_render().map(|m| m.header.id.clone()),
            Some("message-a".into())
        );

        // Nothing arrived for message-c either: the next key keeps the same.
        assert!(!mailbox.leave_selection(first + Duration::from_millis(60)));
        assert_eq!(
            mailbox.lingering_selected.as_ref().map(|l| l.since),
            Some(first)
        );

        // Once a selection has been revealed, it is what the next key keeps.
        mailbox.lingering_selected = None;
        showing(&mut mailbox, "account-a", "message-d", None);
        let later = first + Duration::from_secs(1);
        assert!(mailbox.leave_selection(later));
        let lingering = mailbox.lingering_selected.as_ref().expect("lingering");
        assert_eq!(lingering.message.header.id, "message-d");
        assert_eq!(lingering.since, later);
    }

    #[test]
    fn leaving_a_tab_lingers_nothing() {
        let mut mailbox = MailboxState::default();
        showing(&mut mailbox, "account-a", "message-a", None);
        mailbox.active_tab = Some(0);
        assert!(!mailbox.leave_selection(Instant::now()));
        assert!(mailbox.lingering_selected.is_none());
        assert!(mailbox.selected.is_none());
    }

    /// The lingering message keeps drawing its own thread and expanded
    /// cards after the new selection moved on to another conversation, and
    /// shares them when the conversation is the same.
    #[test]
    fn the_lingering_message_keeps_the_thread_it_is_drawn_with() {
        let account = AccountId("account-a".into());
        let mut mailbox = MailboxState::default();
        showing(
            &mut mailbox,
            "account-a",
            "message-a",
            Some("conversation-1"),
        );
        mailbox.apply_thread(
            &account,
            "conversation-1".into(),
            vec![thread_header("account-a", "message-a", true)],
        );
        mailbox.thread_bodies.insert(
            reference("account-a", "message-a"),
            ThreadBodyState::Loading,
        );
        mailbox.leave_selection(Instant::now());

        // Same conversation: nothing moves.
        mailbox.selected_id = Some(reference("account-a", "message-b"));
        let same = test_message("account-a", "message-b", Some("conversation-1"));
        mailbox.settle_thread_for(&same.header);
        assert!(mailbox.thread.is_some());
        assert!(mailbox
            .lingering_selected
            .as_ref()
            .is_some_and(|l| l.thread.is_none()));

        // Another conversation: the lingering message takes both along.
        let other = test_message("account-a", "message-b", Some("conversation-2"));
        mailbox.settle_thread_for(&other.header);
        assert!(mailbox.thread.is_none());
        assert!(mailbox.thread_bodies.is_empty());
        let own = mailbox
            .lingering_selected
            .as_ref()
            .and_then(|l| l.thread.as_ref())
            .expect("handed over");
        assert_eq!(
            own.thread.as_ref().map(|(id, _)| id.as_str()),
            Some("conversation-1")
        );
        assert_eq!(own.bodies.len(), 1);

        // A later selection's thread never overwrites what it already holds.
        mailbox.apply_thread(
            &account,
            "conversation-2".into(),
            vec![thread_header("account-a", "message-b", true)],
        );
        let third = test_message("account-a", "message-c", Some("conversation-3"));
        mailbox.selected_id = Some(reference("account-a", "message-c"));
        mailbox.settle_thread_for(&third.header);
        assert!(mailbox.thread.is_none());
        let own = mailbox
            .lingering_selected
            .as_ref()
            .and_then(|l| l.thread.as_ref())
            .expect("still held");
        assert_eq!(
            own.thread.as_ref().map(|(id, _)| id.as_str()),
            Some("conversation-1")
        );
    }

    #[test]
    fn the_thread_is_pending_only_once_asked_for_and_until_it_arrives() {
        let account = AccountId("account-a".into());
        let mut mailbox = MailboxState::default();
        showing(
            &mut mailbox,
            "account-a",
            "message-a",
            Some("conversation-1"),
        );
        assert!(!mailbox.selection_thread_pending(), "not asked for");

        mailbox.thread_requested = Some((account.clone(), "conversation-1".into()));
        assert!(mailbox.selection_thread_pending());

        // Another account's conversation of the same id is not this one.
        mailbox.thread = Some((
            "conversation-1".into(),
            vec![thread_header("account-b", "message-a", true)],
        ));
        assert!(mailbox.selection_thread_pending());

        mailbox.apply_thread(
            &account,
            "conversation-1".into(),
            vec![thread_header("account-a", "message-a", true)],
        );
        assert!(!mailbox.selection_thread_pending());

        showing(&mut mailbox, "account-a", "message-b", None);
        assert!(
            !mailbox.selection_thread_pending(),
            "no conversation, nothing to wait for"
        );
    }

    #[test]
    fn late_results_cannot_enter_a_repeated_search() {
        let mut search = super::MailSearchState::default();
        search.begin("subject:alpha".into());
        let first = search.request_id;
        search.begin("subject:beta".into());
        let second = search.request_id;
        search.begin("subject:alpha".into());
        assert!(!search.accepts_results(first, "subject:alpha"));
        assert!(!search.accepts_results(second, "subject:beta"));
        assert!(search.accepts_results(search.request_id, "subject:alpha"));
    }

    #[test]
    fn resubmitting_after_a_scope_change_rejects_the_previous_response() {
        let mut search = super::MailSearchState::default();
        search.begin("subject:alpha".into());
        let previous = search.request_id;
        search.scope = crate::ui::settings::MailSearchScope::Folder;
        search.begin("subject:alpha".into());
        assert!(!search.accepts_results(previous, "subject:alpha"));
        // Both the local and provider phase of the current request are accepted.
        assert!(search.accepts_results(search.request_id, "subject:alpha"));
        assert!(search.accepts_results(search.request_id, "subject:alpha"));
    }

    #[test]
    fn leaving_search_mode_rejects_in_flight_results() {
        let mut mailbox = MailboxState::default();
        mailbox.search.begin("subject:alpha".into());
        let previous = mailbox.search.request_id;
        // Changing folders leaves the input text in place, but disables search.
        mailbox.search.results = None;
        assert!(!mailbox.search.accepts_results(previous, "subject:alpha"));
        mailbox.search.begin("subject:alpha".into());
        assert!(!mailbox.search.accepts_results(previous, "subject:alpha"));
        let current = mailbox.search.request_id;
        mailbox.clear_search();
        assert!(!mailbox.search.accepts_results(current, "subject:alpha"));
    }

    #[test]
    fn clearing_a_restored_search_requests_the_initial_mailbox_load() {
        let mut mailbox = MailboxState {
            search: super::MailSearchState {
                query: "synthetic query".to_string(),
                results: Some(Vec::new()),
                ..super::MailSearchState::default()
            },
            messages_loaded: false,
            ..MailboxState::default()
        };

        assert!(mailbox.clear_search());
        assert!(mailbox.search.query.is_empty());
        assert!(mailbox.search.results.is_none());

        mailbox.search.query = "another synthetic query".to_string();
        mailbox.search.results = Some(Vec::new());
        mailbox.messages_loaded = true;
        assert!(!mailbox.clear_search());
    }
}

#[derive(Default)]
pub enum SenderHistoryState {
    #[default]
    Idle,
    Loading {
        email: String,
    },
    Loaded {
        email: String,
        messages: Vec<MessageHeader>,
        next: Option<String>,
        loading_more: bool,
    },
}

#[derive(Default)]
pub struct ContactsState {
    pub(crate) render_cache: Option<super::contacts_view::ContactListCache>,
    pub list: Vec<crate::model::Contact>,
    pub selected: Option<String>,
    pub query: String,
    pub by_account: HashMap<AccountId, Vec<crate::model::Contact>>,
    pub loading_accounts: HashSet<AccountId>,
}
