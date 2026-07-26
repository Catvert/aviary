//! UI state types owned by `AviaryApp` (see `app.rs`). Pure data only: no
//! rendering logic or runtime access here.

use crate::model::{AccountId, Message, MessageHeader, MessageRef, SentMessage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

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

#[derive(Default)]
pub struct MailboxState {
    pub messages: Vec<MessageHeader>,
    /// Distinguishes a genuinely loaded mailbox from an empty mailbox that has
    /// not made any call yet. Allows returning to Mail without
    /// repeatedly reload the same list.
    pub messages_loaded: bool,
    pub selected_id: Option<String>,
    /// Shared: the reader hands the displayed message to a dozen sub-elements
    /// on every frame, and a `Message` owns its body, its inline images and
    /// its attachment bytes — copying that per frame is megabytes of churn
    /// behind a hover transition. Mutations go through `Rc::make_mut`.
    pub selected: Option<Rc<Message>>,
    /// Explicit multi-selection in the message list. The primary reader
    /// selection remains separate so opening a message does not make every
    /// single-message action look like a bulk operation.
    pub selected_messages: HashSet<MessageRef>,
    /// Last row used as the range-selection anchor for Shift+click.
    pub selection_anchor: Option<MessageRef>,
    pub thread: Option<(String, Vec<MessageHeader>)>,
    pub thread_bodies: HashMap<String, ThreadBodyState>,
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
    use super::MailboxState;

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
    pub list: Vec<crate::model::Contact>,
    pub selected: Option<String>,
    pub query: String,
    pub by_account: HashMap<AccountId, Vec<crate::model::Contact>>,
    pub loading_accounts: HashSet<AccountId>,
}
