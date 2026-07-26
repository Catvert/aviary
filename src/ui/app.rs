//! Root entity for the main window.
//!
//! `AviaryApp` owns all UI state and consumes runtime `Evt`s through an async
//! gpui task. Each view (`inbox`, `calendar_view`, `kanban_view`,
//! `contacts_view`, and `settings_view`) adds its rendering methods
//! in its own file through `impl AviaryApp` blocks.
//!
//! This file keeps the struct, its construction and the command helpers the
//! views call. Four submodules take the layers that had grown inside it:
//! `chrome` (top bar, sidebar, root `Render`), `undo` (deferred, undoable
//! mutations), `quick_action_state` (optimistic quick-action bookkeeping) and
//! `session` (working-session snapshot and restore).

mod chrome;
mod quick_action_state;
mod session;
mod undo;

use super::compose::{ComposeHandle, InlineCompose};
use super::motion::{HoverMotionMap, ScrollPane};
use super::settings::{AppSession, EventComposeSession, SentMessageSession, Settings};
use super::state::{
    AuthState, ContactsState, MailSearchState, MailboxState, MainView, SenderHistoryState,
};
use crate::auth;
use crate::model::{Account, AccountId, Message, MessageHeader, MessageRef, Provider, Tag};
use crate::runtime::{self, Cmd, MessageMutationKind, QuickActionStep, UnifiedAccountPage};
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, DismissEvent, Entity, FocusHandle,
    Focusable as _, Render, ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    notification::{Notification, NotificationList},
    resizable::{h_resizable, resizable_panel, ResizablePanel, ResizableState},
    Sizable, VirtualListScrollHandle, WindowExt,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Repeating j/k produces several selections within milliseconds. A brief
/// delay avoids sending intermediate messages to the runtime that the user
/// will never see.
const MESSAGE_NAVIGATION_DEBOUNCE: Duration = Duration::from_millis(80);
const MESSAGE_ROW_HOVER_DURATION: Duration = Duration::from_millis(120);
/// Captures edits made inside child entities (block editor, recipient chips,
/// detached windows) even when the root entity itself was not notified.
const SESSION_AUTOSAVE_INTERVAL: Duration = Duration::from_millis(400);
const PROVIDER_DRAFT_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);

/// The reply/forward snapshots accumulate without bound while the app runs;
/// only the most recent ones survive a restart.
const SESSION_SENT_SNAPSHOT_CAP: usize = 50;

/// Search history is useful across restarts but must remain a small, recent
/// convenience list rather than growing with every query ever submitted.
const SEARCH_HISTORY_CAP: usize = 10;

/// Cadence of the memory report written to the log. Frequent enough that a
/// session's growth is visible in the Logs tab without re-running the app,
/// rare enough to stay invisible in the noise.
const MEMORY_REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// How many runtime events one gpui update may absorb. Large enough that an
/// ordinary burst is handled in a single pass, small enough that a sustained
/// stream still yields to rendering and input.
const EVENT_BATCH_LIMIT: usize = 64;

/// Presents gpui-component notifications from the bottom edge of the window.
///
/// `Root` still owns their lifecycle; this view only replaces its default
/// top-right layout while observing the same list and dismissal events.
struct BottomRightNotifications {
    source: Entity<NotificationList>,
    _source_observer: Subscription,
    notification_subscriptions: Vec<Subscription>,
}

impl BottomRightNotifications {
    fn new(source: Entity<NotificationList>, cx: &mut Context<Self>) -> Self {
        let source_observer = cx.observe(&source, |this, source, cx| {
            this.refresh_notification_subscriptions(&source, cx);
            cx.notify();
        });
        let mut this = Self {
            source,
            _source_observer: source_observer,
            notification_subscriptions: Vec::new(),
        };
        let source = this.source.clone();
        this.refresh_notification_subscriptions(&source, cx);
        this
    }

    fn refresh_notification_subscriptions(
        &mut self,
        source: &Entity<NotificationList>,
        cx: &mut Context<Self>,
    ) {
        let notifications = source.read(cx).notifications();
        self.notification_subscriptions = notifications
            .iter()
            .map(|notification| {
                cx.subscribe(notification, |_, _, _: &DismissEvent, cx| cx.notify())
            })
            .collect();
    }
}

impl Render for BottomRightNotifications {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut notifications = self.source.read(cx).notifications();
        if notifications.len() > 10 {
            notifications.drain(..notifications.len() - 10);
        }

        div().absolute().bottom_4().right_4().child(
            gpui_component::v_flex()
                .id("bottom-right-notification-list")
                .gap_3()
                .children(notifications),
        )
    }
}

/// Builds the navigation pane shared by all main views. Its geometry must
/// remain identical everywhere because its state is shared.
pub(super) fn sidebar_panel() -> ResizablePanel {
    resizable_panel()
        .size(px(240.))
        .size_range(px(210.)..px(360.))
}

/// Shared shell for main views: resizable navigation on the left and domain
/// content on the right.
pub(super) fn sidebar_layout(
    id: &'static str,
    state: Entity<ResizableState>,
    sidebar: AnyElement,
    content: AnyElement,
) -> AnyElement {
    h_resizable(id)
        .with_state(&state)
        .child(sidebar_panel().child(sidebar))
        .child(resizable_panel().child(content))
        .into_any_element()
}

struct PendingActionNotification;

/// A per-message state the user can toggle from a row, the reader, a shortcut
/// or a whole selection. Read and flagged travel the exact same path — skip
/// offline accounts, update the cached headers, schedule one command per
/// message — and differ only in what this enum names.
#[derive(Clone, Copy)]
enum MessageState {
    Read,
    Flagged,
}

impl MessageState {
    fn command(self, reference: &MessageRef, value: bool) -> Cmd {
        match self {
            Self::Read => Cmd::MarkRead {
                account_id: reference.account_id.clone(),
                id: reference.id.clone(),
                read: value,
            },
            Self::Flagged => Cmd::SetFlag {
                account_id: reference.account_id.clone(),
                id: reference.id.clone(),
                flagged: value,
            },
        }
    }

    fn apply(self, header: &mut MessageHeader, value: bool) {
        match self {
            Self::Read => header.is_read = value,
            Self::Flagged => header.is_flagged = value,
        }
    }

    /// What undo has to put back: each message's value from before the toggle.
    fn restore_effect(self, previous: Vec<(MessageRef, bool)>) -> PendingCancelEffect {
        match self {
            Self::Read => PendingCancelEffect::MessageReads(previous),
            Self::Flagged => PendingCancelEffect::MessageFlags(previous),
        }
    }

    /// Notification copy for the undo window: pending, started, cancelled.
    /// One message and a selection do not read the same, and the keys stay
    /// literal here so they remain greppable from the catalogs.
    fn undo_copy(self, count: usize, delay: u32) -> (SharedString, SharedString, SharedString) {
        match (self, count) {
            (Self::Read, 1) => (
                tr!("undo-read-pending", { seconds: delay }),
                tr!("undo-read-started"),
                tr!("undo-read-cancelled"),
            ),
            (Self::Read, count) => (
                tr!("undo-bulk-read-pending", { count: count, seconds: delay }),
                tr!("undo-bulk-read-started", { count: count }),
                tr!("undo-bulk-read-cancelled", { count: count }),
            ),
            (Self::Flagged, 1) => (
                tr!("undo-flag-pending", { seconds: delay }),
                tr!("undo-flag-started"),
                tr!("undo-flag-cancelled"),
            ),
            (Self::Flagged, count) => (
                tr!("undo-bulk-flag-pending", { count: count, seconds: delay }),
                tr!("undo-bulk-flag-started", { count: count }),
                tr!("undo-bulk-flag-cancelled", { count: count }),
            ),
        }
    }
}

enum PendingCancelEffect {
    None,
    Compose {
        compose_id: u64,
    },
    MessageFlags(Vec<(MessageRef, bool)>),
    MessageReads(Vec<(MessageRef, bool)>),
    MessageTags {
        message_id: String,
        header_tags: Vec<String>,
        message_tags: Option<Vec<String>>,
    },
    MessageRemoved(Box<OptimisticMessageRemoval>),
    MessagesRemoved {
        removals: Vec<OptimisticMessageRemoval>,
        selected: HashSet<MessageRef>,
    },
    KanbanMove {
        account_id: AccountId,
        /// Boxed: a whole header dwarfs every other effect, and this enum is
        /// held for the length of an undo window.
        message: Box<MessageHeader>,
        source_tag_id: String,
        target_tag_id: String,
    },
}

/// Portions of UI state removed immediately during a move or deletion. They
/// are sufficient to restore the message to its exact position if the user
/// triggers undo during the delay.
struct OptimisticMessageRemoval {
    mailbox: Option<(usize, MessageHeader)>,
    search: Option<(usize, MessageHeader)>,
    sender_history: Option<(usize, MessageHeader)>,
    selection: Option<OptimisticSelection>,
    open_tab: Option<(usize, Rc<Message>, bool)>,
}

struct OptimisticSelection {
    message_id: String,
    message: Option<Rc<Message>>,
    thread: Option<(String, Vec<MessageHeader>)>,
}

struct QuickActionMessageSnapshot {
    tags: Option<Vec<String>>,
    body_tags: Option<Vec<String>>,
    read: Option<bool>,
    flagged: Option<bool>,
}

pub(crate) struct QuickActionOptimisticEffect {
    reference: MessageRef,
    steps: Vec<QuickActionStep>,
    snapshot: QuickActionMessageSnapshot,
    removal: Option<OptimisticMessageRemoval>,
}

struct PendingSentRestore {
    related_to: String,
    position: usize,
    session: SentMessageSession,
}

struct PendingAction {
    commands: Vec<Cmd>,
    notification_key: SharedString,
    started_message: SharedString,
    canceled_message: SharedString,
    cancel_effect: PendingCancelEffect,
}

/// A bulk action's per-message replies, aggregated into one outcome.
///
/// The outbox holds one row per message, so a batch of thirty deletions comes
/// back as thirty independent replies, each free to succeed, fail permanently
/// or be deferred. Reported one by one they would bury the user under thirty
/// toasts; reported as a boolean they would lose the case that actually
/// happens — seven moved, three refused. Counting both sides is what makes a
/// partial outcome sayable.
struct PendingBulkCompletion {
    remaining: HashSet<MessageRef>,
    succeeded: usize,
    failed: usize,
    /// First permanent failure, verbatim: a summary that only counts failures
    /// leaves the user nothing to act on.
    first_error: Option<String>,
    /// Copy for the all-succeeded case. Empty keeps the batch silent when
    /// everything works, which is what an implicit action wants — marking a
    /// conversation read on open should only ever speak up to report a
    /// failure.
    message: String,
    notification_key: SharedString,
    /// Deferrals all say the same thing and the outbox retries every one of
    /// them, so only the first of a batch is worth a toast.
    deferred_notified: bool,
}

/// What the reply that closed a batch has to report.
pub(crate) struct BulkCompletion {
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) first_error: Option<String>,
    pub(crate) message: String,
    pub(crate) notification_key: SharedString,
}

impl BulkCompletion {
    pub(crate) fn total(&self) -> usize {
        self.succeeded + self.failed
    }
}

/// Where one provider reply sits in the batch it belongs to.
pub(crate) enum BulkReply {
    /// Not part of a batch: the reducer reports this message on its own.
    Single,
    /// One of many, with replies still outstanding: the batch speaks for it.
    Pending,
    /// The reply that closed the batch.
    Completed(Box<BulkCompletion>),
}

impl BulkReply {
    /// Whether reporting this message on its own would be one toast of many
    /// for a single user gesture.
    pub(crate) fn is_bulk(&self) -> bool {
        !matches!(self, Self::Single)
    }

    pub(crate) fn completion(self) -> Option<BulkCompletion> {
        match self {
            Self::Completed(completion) => Some(*completion),
            _ => None,
        }
    }
}

/// A deferred reply: whether it belongs to a batch, and whether it is the
/// first deferral of that batch — the only one worth telling the user about.
pub(crate) struct BulkDeferral {
    pub(crate) bulk: bool,
    pub(crate) first: bool,
}

pub(super) fn compact_error(error: &str) -> String {
    const MAX_CHARS: usize = 420;
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let compact: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

/// Where each scrollable pane is, and where it is heading. One field per pane
/// rather than a handle and a motion each, so the pair cannot drift apart.
/// Quick-action executions Aviary is tracking.
///
/// A recipe is submitted to the durable outbox, so what the UI keeps is the
/// request it can still cancel, the optimistic mail-view change it may have to
/// roll back, and the menu the user opened it from.
#[derive(Default)]
pub(crate) struct QuickActionState {
    /// Requests handed to the runtime, keyed by execution id.
    pub(crate) pending: HashMap<u64, super::quick_actions::PendingQuickActionRequest>,
    pub(crate) seq: u64,
    /// Pure-triage recipes update the mail view before their undo timer
    /// expires. These snapshots restore that state when the durable operation
    /// is cancelled or fails partway through.
    pub(crate) effects: HashMap<u64, QuickActionOptimisticEffect>,
    /// Stable popup shared by mouse and keyboard quick-action entry points.
    pub(crate) menu: Option<super::quick_actions::QuickActionMenu>,
}

/// Bulk actions waiting for their per-message provider replies, aggregated into
/// one completion toast per batch.
#[derive(Default)]
struct BulkCompletions {
    pending: HashMap<u64, PendingBulkCompletion>,
    /// Reverse index: a reply names a message, the toast belongs to a batch.
    by_message: HashMap<MessageRef, u64>,
    seq: u64,
}

impl BulkCompletions {
    /// Claims a batch id and indexes its messages. Split from `arm` because
    /// the notification key only exists once the action has been scheduled,
    /// while the index has to be in place before any command goes out.
    fn claim(&mut self, references: &[MessageRef]) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        let completion_id = self.seq;
        for reference in references {
            self.by_message.insert(reference.clone(), completion_id);
        }
        completion_id
    }

    /// Arms a claimed batch with what its completion will have to say.
    fn arm(
        &mut self,
        completion_id: u64,
        references: &[MessageRef],
        message: String,
        notification_key: SharedString,
    ) {
        self.pending.insert(
            completion_id,
            PendingBulkCompletion {
                remaining: references.iter().cloned().collect(),
                succeeded: 0,
                failed: 0,
                first_error: None,
                message,
                notification_key,
                deferred_notified: false,
            },
        );
    }

    /// Books one terminal reply — `error` set means it failed for good — and
    /// says whether it closed the batch.
    fn record(&mut self, reference: &MessageRef, error: Option<String>) -> BulkReply {
        let Some(completion_id) = self.by_message.remove(reference) else {
            return BulkReply::Single;
        };
        let Some(batch) = self.pending.get_mut(&completion_id) else {
            // Aged out of the aggregation, but still a batch member: staying
            // silent beats one toast per straggler.
            return BulkReply::Pending;
        };
        batch.remaining.remove(reference);
        match error {
            Some(error) => {
                batch.failed += 1;
                if batch.first_error.is_none() {
                    batch.first_error = Some(error);
                }
            }
            None => batch.succeeded += 1,
        }
        if !batch.remaining.is_empty() {
            return BulkReply::Pending;
        }
        let batch = self
            .pending
            .remove(&completion_id)
            .expect("borrowed just above");
        BulkReply::Completed(Box::new(BulkCompletion {
            succeeded: batch.succeeded,
            failed: batch.failed,
            first_error: batch.first_error,
            message: batch.message,
            notification_key: batch.notification_key,
        }))
    }

    /// Books a deferral, which is *not* terminal: the operation stays in the
    /// outbox, so the message keeps its place in the batch.
    fn note_deferral(&mut self, reference: &MessageRef) -> BulkDeferral {
        let Some(completion_id) = self.by_message.get(reference) else {
            return BulkDeferral {
                bulk: false,
                first: true,
            };
        };
        let first = self
            .pending
            .get_mut(completion_id)
            .is_some_and(|batch| !std::mem::replace(&mut batch.deferred_notified, true));
        BulkDeferral { bulk: true, first }
    }

    /// Drops a batch whole: undo took its commands back, or the aggregation
    /// window closed.
    fn forget(&mut self, completion_id: u64, references: &[MessageRef]) {
        for reference in references {
            // Only if it still points at this batch: a later action on the
            // same message owns the index by then.
            if self.by_message.get(reference) == Some(&completion_id) {
                self.by_message.remove(reference);
            }
        }
        self.pending.remove(&completion_id);
    }
}

pub struct Scrolls {
    /// Folder/account tree in the mail sidebar.
    pub folders: ScrollPane<VirtualListScrollHandle>,
    /// Virtual message list; its offset also drives "load more" preloading.
    pub messages: ScrollPane<VirtualListScrollHandle>,
    /// Virtual contact list.
    pub contacts: ScrollPane<VirtualListScrollHandle>,
    /// Continuously scrolling calendar grid.
    pub calendar: ScrollPane<VirtualListScrollHandle>,
    /// Reader pane: message body and thread.
    pub viewer: ScrollPane<ScrollHandle>,
}

impl Default for Scrolls {
    fn default() -> Self {
        Self {
            folders: ScrollPane::new(VirtualListScrollHandle::new()),
            messages: ScrollPane::new(VirtualListScrollHandle::new()),
            contacts: ScrollPane::new(VirtualListScrollHandle::new()),
            calendar: ScrollPane::new(VirtualListScrollHandle::new()),
            viewer: ScrollPane::new(ScrollHandle::new()),
        }
    }
}

pub struct AviaryApp {
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub settings: Settings,
    /// Serializes and writes `session.json` on its own thread; the UI side
    /// only builds snapshots.
    session_store: super::session_store::SessionStore,
    /// Last known Preferences tab, restored from the session and refreshed at
    /// each persist while the view exists — the fallback used once
    /// `settings_ui` has been torn down.
    pub(crate) last_settings_tab: super::settings_view::SettingsTab,
    /// Set by root and child-entity observers; consumed by the debounce pump.
    pub(crate) session_dirty: bool,
    /// Message references restored from the UI session. Once their account is
    /// ready, a cache-first load reconstructs the complete SQLite record.
    pending_rehydrate: Vec<MessageRef>,
    /// Reply/forward cards waiting for their referenced cached message.
    pending_sent_restore: Vec<PendingSentRestore>,
    pub auth: AuthState,
    pub accounts: Vec<Account>,
    /// Token files that exist on disk but could not be resumed. They remain
    /// visible in Settings so the user can remove or reconnect them.
    pub unavailable_accounts: HashMap<AccountId, (Provider, String)>,
    pub current_account_id: Option<AccountId>,
    pub view: MainView,
    pub mailbox: MailboxState,
    pub sender_history: SenderHistoryState,
    /// The panel starts collapsed; its content is requested from the runtime
    /// only when first opened for the displayed message.
    pub sender_history_expanded: bool,
    pub contacts: ContactsState,
    /// Address book shared with composer completion providers.
    pub address_book: super::addresses::AddressBook,
    pub tags_by_account: HashMap<AccountId, Vec<Tag>>,
    pub tags_loading: HashSet<AccountId>,
    pub calendar: super::calendar_view::CalendarViewState,
    /// Calendar editors waiting for their provider account to finish restoring.
    pub(crate) pending_event_composes: Vec<EventComposeSession>,
    pub kanban: super::kanban_view::BoardState,
    pub log_filter: log::LevelFilter,
    /// Accounts whose latest synchronization failed. Cached data remains
    /// available, but mutations are blocked until reconnection.
    pub offline_accounts: HashSet<AccountId>,
    pub mail_cache_used_bytes: u64,
    pub mail_cache_limit_bytes: u64,
    pub languagetool_status: crate::proofreading::LanguageToolStatus,
    pub notification_tx: crate::notify::NotificationActionSender,

    pub compose_seq: u64,
    pub composes: Vec<ComposeHandle>,
    /// Composers embedded in the reader pane, one per `ViewerTab::Compose` tab
    /// (the default mode for a new message); OS windows remain available via
    /// the detach action.
    pub inline_composes: Vec<InlineCompose>,
    /// Reply being composed in the reader above the body.
    pub inline_reply: Option<super::viewer::InlineReply>,
    /// Reply hidden as soon as Send is clicked. It remains intact until the
    /// undo delay expires and the provider confirms or rejects the send.
    pub(crate) pending_inline_reply: Option<super::viewer::InlineReply>,
    /// Reader translation panel: target language, whether it is open, and the
    /// streamed result for the displayed message.
    pub viewer_translation: super::viewer::ViewerTranslationState,
    /// Message to open in a Mail tab as soon as its body arrives
    /// (double-clic Kanban).
    pub pending_kanban_open: Option<(AccountId, String)>,
    /// Message an OS notification asked to open. The listing may show another
    /// account entirely — the per-account relevance filter would then drop the
    /// body that was explicitly requested — so the reference is kept until it
    /// arrives.
    pub pending_notification_open: Option<MessageRef>,
    /// Message to reply to as soon as its body arrives.
    pub pending_reply_id: Option<String>,
    /// Message to forward as soon as its body and attachments arrive.
    pub pending_forward_id: Option<String>,
    /// Quick-action executions in flight, their optimistic effects, and the
    /// shared menu.
    pub(crate) quick_actions: QuickActionState,
    /// Meeting-request responses currently being sent, keyed by their mail
    /// message so duplicate clicks are disabled across selection and tabs.
    pub invitation_responses_in_flight: HashSet<MessageRef>,

    /// Mutations held for a few seconds before being sent to the runtime. While
    /// they remain here, the notification's undo button can guarantee that no
    /// provider call has occurred.
    pending_actions: HashMap<u64, PendingAction>,
    pending_action_seq: u64,
    /// Bulk actions waiting for their per-message provider replies.
    bulk_completions: BulkCompletions,
    /// Invalidates deferred opens when newer navigation occurs.
    pending_message_open_seq: u64,

    /// Command-mode focus target. Without explicit focus, gpui dispatches keys
    /// from `Root` and skips the view's `Aviary` context.
    shortcut_focus: FocusHandle,
    pub search_input: Entity<InputState>,
    pub(super) mail_search_scroll: super::components::overlay_popover::OverlayPopoverScroll,
    pub contacts_search_input: Entity<InputState>,
    /// Scroll position and wheel motion of every scrollable pane.
    pub scrolls: Scrolls,
    /// Hover state shared by mailbox, sender-history and contact message rows.
    /// The scope belongs to the key because one message can appear in several
    /// panes at once.
    pub(super) message_row_hover: HoverMotionMap<(&'static str, AccountId, String)>,
    /// Flattened and measured mail rows, rebuilt only when message/filter
    /// structure changes rather than on every root render.
    pub(super) message_list_cache: Option<super::inbox::messages::MessageListCache>,
    /// Measured height of each sidebar row variant. Only the measurement is
    /// cached — the folder tree itself is rebuilt per render, so no invalidation
    /// has to be remembered when a folder, favorite or account changes.
    pub(super) folder_list_metrics: Option<super::inbox::folders::FolderListMetrics>,
    pub(super) message_list_revision: u64,
    /// Attachment payloads being fetched, and what to do when they land.
    pub attachments: super::viewer::AttachmentFetches,
    /// Actual painted width of the reader pane. Unlike the size reported by
    /// the resizable panel, this measurement includes the final layout after
    /// resolving Blitz flex items.
    pub viewer_layout_width: Option<f32>,
    /// Width of the left navigation pane, shared by all views.
    pub sidebar_resize: Entity<ResizableState>,
    /// Message-list/reader split to the right of the shared pane.
    pub inbox_resize_h: Entity<ResizableState>,
    pub inbox_resize_v: Entity<ResizableState>,
    pub settings_ui: Option<super::settings_view::SettingsUi>,
    pub imap_form: Option<super::auth_view::ImapFormUi>,
    pub folder_dialog_input: Option<Entity<InputState>>,
    /// Kept alive for as long as the "remind me on…" dialog is open: its
    /// closure only borrows the picker, so nothing else owns it.
    pub snooze_dialog_picker: Option<Entity<gpui_component::date_picker::DatePickerState>>,
    notification_layer: Option<Entity<BottomRightNotifications>>,

    #[cfg(target_os = "linux")]
    pub tray: Option<crate::tray::TrayHandle>,
}

impl AviaryApp {
    pub fn new(settings: Settings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        super::blitz_body::install_link_handler(cx.weak_entity(), cx);
        let restored_session = AppSession::load();
        let session_fingerprint =
            serde_json::to_string(&restored_session).unwrap_or_else(|_| String::new());
        let session_store = super::session_store::SessionStore::spawn(session_fingerprint);
        let (cmd_tx, mut evt_rx) = runtime::spawn(
            settings.global.mail_cache_limit_mb,
            settings.global.languagetool.clone(),
        );
        let _ = cmd_tx.send(Cmd::ConfigureIcalSubscriptions(
            settings.global.ical_subscriptions.clone(),
        ));
        let mail_cache_limit_bytes = settings.global.mail_cache_limit_mb * 1024 * 1024;
        let (notification_tx, mut notification_rx) = crate::notify::channel();

        cx.on_app_quit(|app, cx| {
            app.persist_session(cx);
            app.session_store.flush();
            async {}
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            while let Some(evt) = evt_rx.recv().await {
                // Runtime events arrive in bursts — a listed page, a thread
                // being rehydrated, an outbox drain acknowledging a dozen
                // mutations. Each `update_in` flushes gpui's effects and
                // re-arms the whole window, so the burst is handled in one
                // pass instead of one pass per event. The cap keeps a
                // continuous stream from monopolising the frame.
                let mut batch = vec![evt];
                while batch.len() < EVENT_BATCH_LIMIT {
                    let Ok(next) = evt_rx.try_recv() else { break };
                    batch.push(next);
                }
                if this
                    .update_in(cx, |app, window, cx| {
                        for evt in batch {
                            app.handle_event(evt, window, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = notification_rx.recv().await {
                if this
                    .update_in(cx, |app, window, cx| {
                        app.handle_notification_action(action, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(SESSION_AUTOSAVE_INTERVAL)
                .await;
            if this
                .update_in(cx, |app, _, cx| {
                    if app.session_dirty {
                        app.persist_session(cx);
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(PROVIDER_DRAFT_AUTOSAVE_INTERVAL)
                .await;
            if this
                .update_in(cx, |app, _, cx| {
                    let composers: Vec<_> = app
                        .composes
                        .iter()
                        .map(|handle| handle.view.clone())
                        .collect();
                    for composer in composers {
                        let _ = composer.update(cx, |view, cx| {
                            view.maybe_autosave_draft(cx);
                        });
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        // Wakes messages put off until later. The first pass runs before the
        // first tick on purpose: a deadline that fell while Aviary was closed
        // is already due at startup.
        cx.spawn_in(window, async move |this, cx| loop {
            if this
                .update(cx, |app, cx| {
                    app.wake_due_snoozes(cx);
                })
                .is_err()
            {
                break;
            }
            cx.background_executor()
                .timer(super::snooze::WAKE_TICK)
                .await;
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(MEMORY_REPORT_INTERVAL).await;
            if this
                .update(cx, |app, cx| {
                    super::memory::Report::collect(app, cx).log();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        // Logs may arrive from any thread without passing
        // through the Evt channel. A lightweight pump refreshes only the
        // Settings log section when its generation changes.
        cx.spawn_in(window, async move |this, cx| {
            let mut generation = crate::logging::generation();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let current = crate::logging::generation();
                if current == generation {
                    continue;
                }
                generation = current;
                if this
                    .update_in(cx, |app, _, cx| {
                        if app.view == MainView::Settings
                            && app
                                .settings_ui
                                .as_ref()
                                .is_some_and(|ui| ui.tab == super::settings_view::SettingsTab::Logs)
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        #[cfg(target_os = "linux")]
        let tray = if settings.global.tray_enabled {
            let (handle, mut tray_rx) = crate::tray::spawn();
            cx.spawn_in(window, async move |this, cx| {
                while let Some(action) = tray_rx.recv().await {
                    if this
                        .update_in(cx, |app, window, cx| app.handle_tray(action, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            Some(handle)
        } else {
            None
        };

        let initial_search_query = restored_session.mailbox.search_query.clone();
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("search-hint"))
                .default_value(initial_search_query)
        });
        let viewer_translation_target = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("viewer-translation-target-placeholder"))
                .default_value(settings.global.ai.reader_translation_target.clone())
        });
        cx.subscribe_in(
            &search_input,
            window,
            |this: &mut Self, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    if let Some(query) = this.selected_mail_search_query(cx) {
                        this.choose_search_suggestion(query, window, cx);
                    } else {
                        let q = state.read(cx).value().to_string();
                        this.submit_search(q, window, cx);
                    }
                }
                InputEvent::Change => {
                    this.mailbox.search.menu_selection = None;
                    let q = state.read(cx).value().to_string();
                    if state.focus_handle(cx).is_focused(window)
                        && q.trim() != this.mailbox.search.query
                    {
                        // Enter closes the suggestions without moving focus.
                        // Only a subsequent user edit should reopen them; the
                        // programmatic value applied by a selected suggestion
                        // still represents the just-submitted query.
                        this.mailbox.search.menu_open = true;
                    }
                    if q.is_empty() && this.mailbox.search.results.is_some() {
                        this.clear_mail_search();
                    }
                    // The suggestions are rendered by the parent view rather
                    // than the Input entity, so every edit must refresh it.
                    cx.notify();
                }
                InputEvent::Focus => {
                    this.mailbox.search.menu_open = true;
                    this.mailbox.search.menu_selection = None;
                    this.ensure_contacts_loaded();
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.mailbox.search.menu_open = false;
                    this.mailbox.search.menu_selection = None;
                    this.mail_search_scroll.reset();
                    cx.notify();
                }
            },
        )
        .detach();

        let initial_contacts_query = restored_session.contacts.query.clone();
        let contacts_search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("contacts-search-hint"))
                .default_value(initial_contacts_query)
        });
        cx.subscribe_in(
            &contacts_search_input,
            window,
            |this: &mut Self, state, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.contacts.query = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();

        let shortcut_focus = cx.focus_handle();
        let initial_shortcut_focus = shortcut_focus.clone();
        cx.on_next_frame(window, move |_, window, _| {
            initial_shortcut_focus.focus(window);
        });

        let mut calendar =
            super::calendar_view::CalendarViewState::new(settings.global.calendar_layout);
        calendar.range = restored_session.calendar.range;
        calendar.anchor = restored_session.calendar.anchor;
        calendar.selected = restored_session.calendar.selected.clone();
        // Open the scrolling grid on the restored period rather than today.
        calendar.grid_scroll_to(calendar.anchor_date());

        let mut mailbox = MailboxState {
            selected_folder_id: restored_session.mailbox.selected_folder_id.clone(),
            unified_selected_account: restored_session.mailbox.unified_selected_account.clone(),
            selected_id: restored_session
                .mailbox
                .selected_message
                .as_ref()
                .map(|message| message.id.clone()),
            selected: None,
            search: MailSearchState {
                query: restored_session.mailbox.search_query.clone(),
                results: (!restored_session.mailbox.search_query.is_empty()).then(Vec::new),
                history: restored_session.mailbox.search_history.clone(),
                scope: restored_session.mailbox.search_scope,
                sort: restored_session.mailbox.search_sort,
                ..MailSearchState::default()
            },
            show_flagged_only: restored_session.mailbox.show_flagged_only,
            tag_filters: restored_session.mailbox.tag_filters.clone(),
            expanded_quoted_sections: restored_session.mailbox.expanded_quoted_sections.clone(),
            sent_messages: HashMap::new(),
            expanded_sent_messages: restored_session.mailbox.expanded_sent_messages.clone(),
            collapsed_message_sections: restored_session.mailbox.collapsed_message_sections.clone(),
            expanded_conversations: restored_session.mailbox.expanded_conversations.clone(),
            ..MailboxState::default()
        };
        // The persisted selection shows a loading state until its SQLite
        // record arrives; the listing is refreshed after authentication.
        mailbox.messages_loaded = false;

        let contacts = ContactsState {
            selected: restored_session.contacts.selected.clone(),
            query: restored_session.contacts.query.clone(),
            ..ContactsState::default()
        };
        let mut kanban = super::kanban_view::BoardState::default();
        kanban.preview = restored_session.kanban_preview.clone();

        let mut app = Self {
            cmd_tx,
            settings,
            session_store,
            last_settings_tab: restored_session.settings_tab,
            pending_rehydrate: Vec::new(),
            pending_sent_restore: Vec::new(),
            session_dirty: false,
            auth: AuthState::Idle,
            accounts: Vec::new(),
            unavailable_accounts: HashMap::new(),
            current_account_id: None,
            view: restored_session.main_view,
            mailbox,
            sender_history: SenderHistoryState::default(),
            sender_history_expanded: restored_session.sender_history_expanded,
            contacts,
            address_book: Default::default(),
            tags_by_account: HashMap::new(),
            tags_loading: HashSet::new(),
            calendar,
            pending_event_composes: restored_session.event_composes.clone(),
            kanban,
            log_filter: log::LevelFilter::Debug,
            offline_accounts: HashSet::new(),
            mail_cache_used_bytes: 0,
            mail_cache_limit_bytes,
            languagetool_status: crate::proofreading::LanguageToolStatus::default(),
            notification_tx,
            compose_seq: 0,
            composes: Vec::new(),
            inline_composes: Vec::new(),
            inline_reply: None,
            pending_inline_reply: None,
            viewer_translation: super::viewer::ViewerTranslationState {
                target: viewer_translation_target,
                open: false,
                result: None,
            },
            pending_kanban_open: None,
            pending_notification_open: None,
            pending_reply_id: None,
            pending_forward_id: None,
            quick_actions: QuickActionState::default(),
            invitation_responses_in_flight: HashSet::new(),
            pending_actions: HashMap::new(),
            pending_action_seq: 0,
            bulk_completions: BulkCompletions::default(),
            pending_message_open_seq: 0,
            shortcut_focus,
            search_input,
            mail_search_scroll: super::components::overlay_popover::OverlayPopoverScroll::default(),
            contacts_search_input,
            scrolls: Scrolls::default(),
            message_row_hover: HoverMotionMap::new(MESSAGE_ROW_HOVER_DURATION),
            message_list_cache: None,
            folder_list_metrics: None,
            message_list_revision: 0,
            attachments: super::viewer::AttachmentFetches::default(),
            viewer_layout_width: None,
            sidebar_resize: cx.new(|_| ResizableState::default()),
            inbox_resize_h: cx.new(|_| ResizableState::default()),
            inbox_resize_v: cx.new(|_| ResizableState::default()),
            settings_ui: None,
            imap_form: None,
            folder_dialog_input: None,
            snooze_dialog_picker: None,
            notification_layer: None,
            #[cfg(target_os = "linux")]
            tray,
        };
        app.restore_session_editors(restored_session, window, cx);
        cx.observe_self(|app, _| app.session_dirty = true).detach();
        app
    }

    pub(super) fn install_notification_layer(
        &mut self,
        source: Entity<NotificationList>,
        cx: &mut Context<Self>,
    ) {
        self.notification_layer = Some(cx.new(|cx| BottomRightNotifications::new(source, cx)));
    }

    // ----------------------------------------------------------------
    // Runtime commands/helpers
    // ----------------------------------------------------------------

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Account identifiers to act on: all checked accounts, possibly narrowed
    /// to the account of the currently open folder.
    pub fn active_account_ids(&self) -> Vec<AccountId> {
        if let Some(aid) = &self.mailbox.unified_selected_account {
            vec![aid.clone()]
        } else {
            self.accounts
                .iter()
                .filter(|account| self.unified_account_included(&account.id))
                .map(|account| account.id.clone())
                .collect()
        }
    }

    pub(crate) fn unified_account_included(&self, account_id: &AccountId) -> bool {
        !self
            .settings
            .global
            .unified_hidden_account_ids
            .contains(&account_id.0)
    }

    pub fn send_for_active(&self, f: impl Fn(AccountId) -> Cmd) {
        for aid in self.active_account_ids() {
            self.send(f(aid));
        }
    }

    pub fn fetch_limit(&self, aid: &AccountId) -> usize {
        self.settings.account_or_default(Some(aid)).fetch_limit
    }

    pub(super) fn uses_unified_pagination(&self) -> bool {
        self.mailbox.unified_selected_account.is_none() && self.active_account_ids().len() > 1
    }

    pub(super) fn request_mailbox_refresh(&mut self) {
        if self.active_account_ids().is_empty() {
            self.mailbox.messages.clear();
            self.mailbox.messages_loaded = true;
            self.mailbox.pagination.has_more = false;
            self.mailbox.pagination.loading_more = false;
            self.mailbox.refresh_pending = false;
            self.invalidate_message_list();
            return;
        }
        if self.uses_unified_pagination() {
            self.mailbox.pagination.unified_request_id = self
                .mailbox
                .pagination
                .unified_request_id
                .wrapping_add(1)
                .max(1);
            self.mailbox.pagination.has_more = false;
            self.mailbox.pagination.loading_more = false;
            self.mailbox.pagination.last_request_len = None;
            let accounts: Vec<_> = self
                .active_account_ids()
                .into_iter()
                .map(|account_id| UnifiedAccountPage {
                    page_size: self.fetch_limit(&account_id).max(1),
                    account_id,
                })
                .collect();
            let page_size = accounts
                .iter()
                .fold(0usize, |total, account| {
                    total.saturating_add(account.page_size)
                })
                .max(1);
            self.send(Cmd::RefreshUnified {
                request_id: self.mailbox.pagination.unified_request_id,
                accounts,
                page_size,
            });
            return;
        }

        let folder = self.mailbox.selected_folder_id.clone();
        for aid in self.active_account_ids() {
            let limit = self.fetch_limit(&aid);
            self.send(Cmd::Refresh {
                account_id: aid,
                folder_id: folder.clone(),
                limit,
            });
        }
    }

    pub fn send_refresh(&mut self) {
        self.mailbox.refresh_pending = true;
        self.request_mailbox_refresh();
    }

    pub fn sync_auto_refresh(&mut self) {
        let Some(aid) = self.accounts.first().map(|account| account.id.clone()) else {
            return;
        };
        let acc = self.settings.account_or_default(Some(&aid));
        // Background delivery always watches Inbox. Folder selection controls
        // only the foreground list and must never silence new-mail delivery.
        let key = (None, acc.auto_refresh_secs, acc.fetch_limit);
        if self.mailbox.last_auto_refresh_sent.as_ref() == Some(&key) {
            return;
        }
        self.mailbox.last_auto_refresh_sent = Some(key.clone());
        for aid in self.accounts.iter().map(|account| account.id.clone()) {
            let acc = self.settings.account_or_default(Some(&aid));
            self.send(Cmd::SetAutoRefresh {
                account_id: aid,
                folder_id: None,
                secs: acc.auto_refresh_secs,
                limit: acc.fetch_limit,
            });
        }
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    /// Accounts in user-selected order. Accounts absent from
    /// the preference (first launch or recent addition) remain at the end,
    /// in load order.
    pub(crate) fn ordered_accounts(&self) -> Vec<Account> {
        let order = &self.settings.global.account_order;
        let mut accounts = self.accounts.clone();
        accounts.sort_by_key(|account| {
            order
                .iter()
                .position(|id| id == &account.id.0)
                .unwrap_or(usize::MAX)
        });
        accounts
    }

    /// Global creation account. An unavailable preference is ignored, and the
    /// first account in the chosen order is used as a fallback.
    pub(crate) fn default_creation_account_id(&self) -> Option<AccountId> {
        self.settings
            .global
            .default_account_id
            .as_ref()
            .map(|id| AccountId(id.clone()))
            .filter(|id| self.account(id).is_some())
            .or_else(|| {
                self.ordered_accounts()
                    .first()
                    .map(|account| account.id.clone())
            })
    }

    /// For a new message, the open folder's account is more specific than the
    /// global preference. In the all-mailboxes view, the preference correctly
    /// takes over instead of using the latest visited context.
    pub(crate) fn mail_creation_account_id(&self) -> Option<AccountId> {
        self.mailbox
            .unified_selected_account
            .clone()
            .filter(|id| self.account(id).is_some())
            .or_else(|| self.default_creation_account_id())
    }

    pub fn account_label(&self, a: &Account) -> String {
        let over = self
            .settings
            .accounts
            .get(&a.id)
            .map(|s| s.display_name_override.clone())
            .unwrap_or_default();
        if !over.is_empty() {
            over
        } else if !a.display_name.is_empty() {
            a.display_name.clone()
        } else {
            a.email.clone()
        }
    }

    pub fn next_editor_id(&mut self) -> u64 {
        self.compose_seq += 1;
        self.compose_seq
    }

    pub fn toast(&self, window: &mut Window, cx: &mut App, note: Notification) {
        window.push_notification(note, cx);
    }

    pub(crate) fn focus_shortcuts(&self, window: &mut Window) {
        self.shortcut_focus.focus(window);
    }

    pub(crate) fn open_create_tag_dialog(
        &mut self,
        account_id: AccountId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("tags-new-name-placeholder")));
        let entity = cx.entity();
        gpui_component::WindowExt::open_dialog(window, cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            let account_id = account_id.clone();
            dialog
                .title(tr!("tags-create-title"))
                .confirm()
                .child(gpui_component::input::Input::new(&input))
                .on_ok(move |_, _window, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        this.send(Cmd::CreateTag {
                            account_id: account_id.clone(),
                            name,
                            color: None,
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }

    /// Displays a readable, persistent error while retaining details
    /// full details in console and UI logs.
    pub fn notify_error(
        &self,
        error: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notify_error_with_key(error.into(), None, window, cx);
    }

    pub(super) fn notify_message_mutation_error(
        &self,
        error: impl Into<String>,
        kind: MessageMutationKind,
        reference: &MessageRef,
        notification_key: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = notification_key.unwrap_or_else(|| {
            Self::message_mutation_notification_key(kind, &reference.account_id, &reference.id)
        });
        self.notify_error_with_key(error.into(), Some(key), window, cx);
    }

    fn notify_error_with_key(
        &self,
        error: String,
        notification_key: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::error!("{error}");

        let compact = compact_error(&error);
        let app = cx.entity();
        let mut notification = Notification::error(compact)
            .title(tr!("error-dialog-title"))
            .autohide(false)
            .action(move |_, _, _| {
                let app = app.clone();
                Button::new("open-logs")
                    .ghost()
                    .small()
                    .label(tr!("error-view-log"))
                    .on_click(move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.open_logs_settings(window, cx);
                        });
                    })
            });
        if let Some(key) = notification_key {
            notification = notification.id1::<PendingActionNotification>(key);
        }
        self.toast(window, cx, notification);
    }

    pub(crate) fn set_current_account_context(&mut self, id: Option<AccountId>) {
        self.current_account_id = id.clone();
        self.settings.global.last_account_id = id.as_ref().map(|a| a.0.clone());
        // Account-scoped preference inputs are persistent entities; rebuild
        // them next time Settings opens so they reflect the new context.
        self.settings_ui = None;
        if let Some(aid) = &id {
            self.mailbox.folders = self
                .mailbox
                .folders_by_account
                .get(aid)
                .cloned()
                .unwrap_or_default();
            let cols = self
                .settings
                .account_or_default(Some(aid))
                .kanban_tag_columns;
            self.kanban.ensure_account(aid, &cols);
        } else {
            self.mailbox.folders.clear();
        }
    }

    /// Loads only data required by the Mail view. Called at startup (the
    /// initial view) and when explicitly entering the tab, but never from
    /// Calendar, Contacts, or Settings.
    fn load_mail_view(&mut self) {
        let load_messages = !self.mailbox.messages_loaded;
        let active = self.active_account_ids();
        for aid in &active {
            // Mailbox rows display tags without opening the message, so load
            // their registry upon entering Mail, especially in unified view
            // where each account has its own registry.
            self.ensure_tags_loaded(aid);
        }
        let folder_accounts: Vec<_> = self
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        for aid in folder_accounts {
            if !self.mailbox.folders_by_account.contains_key(&aid) {
                self.send(Cmd::LoadFolders {
                    account_id: aid.clone(),
                });
            }
        }
        if load_messages {
            self.request_mailbox_refresh();
        }
        self.sync_auto_refresh();
    }

    pub(crate) fn enter_main_view(&mut self, view: MainView, cx: &mut Context<Self>) {
        let entering = self.view != view;
        self.view = view;
        if entering && view == MainView::Mail {
            self.load_mail_view();
        }
        cx.notify();
    }

    pub(crate) fn ensure_tags_loaded(&mut self, aid: &AccountId) {
        if self.tags_by_account.contains_key(aid) || !self.tags_loading.insert(aid.clone()) {
            return;
        }
        self.send(Cmd::LoadTags {
            account_id: aid.clone(),
        });
    }

    pub fn select_folder(
        &mut self,
        account: Option<AccountId>,
        folder_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pending_message_open(cx);
        self.mailbox.unified_selected_account = account.clone();
        if let Some(account_id) = account {
            self.ensure_tags_loaded(&account_id);
            self.set_current_account_context(Some(account_id));
        }
        self.settings.save();
        self.mailbox.selected_folder_id = folder_id;
        self.mailbox.messages.clear();
        self.mailbox.messages_loaded = false;
        self.mailbox.selected = None;
        self.mailbox.selected_id = None;
        self.mailbox.thread = None;
        self.mailbox.search.results = None;
        self.mailbox.tag_filters.clear();
        self.mailbox.last_auto_refresh_sent = None;
        self.invalidate_message_list();
        self.send_refresh();
        self.sync_auto_refresh();
        cx.notify();
    }

    pub(super) fn submit_search(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = query.trim().to_string();
        self.mailbox.search.menu_open = false;
        self.mailbox.search.menu_selection = None;
        self.mail_search_scroll.reset();
        if query.is_empty() {
            self.clear_mail_search();
            cx.notify();
            return;
        }
        self.mailbox
            .search
            .history
            .retain(|previous| !previous.eq_ignore_ascii_case(&query));
        self.mailbox.search.history.insert(0, query.clone());
        self.mailbox.search.history.truncate(SEARCH_HISTORY_CAP);
        self.mailbox.search.query = query.clone();
        self.mailbox.search.results = Some(Vec::new());
        self.invalidate_message_list();
        let scope = self.mail_search_scope();
        for aid in self.active_account_ids() {
            let limit = self.fetch_limit(&aid);
            self.send(Cmd::Search {
                account_id: aid,
                query: query.clone(),
                scope: scope.clone(),
                limit,
            });
        }
        cx.notify();
    }

    /// Scope the next search is dispatched with.
    ///
    /// "This folder" resolves against the folder selected in the tree; in
    /// unified mode that selection is per account, so each `Cmd::Search`
    /// carries the folder of the account it targets — hence the resolution
    /// here rather than at the call site.
    pub(crate) fn mail_search_scope(&self) -> crate::runtime::SearchScope {
        match self.mailbox.search.scope {
            crate::ui::settings::MailSearchScope::Folder => {
                crate::runtime::SearchScope::Folder(self.mailbox.selected_folder_id.clone())
            }
            crate::ui::settings::MailSearchScope::Everywhere => {
                crate::runtime::SearchScope::Account
            }
        }
    }

    pub(crate) fn set_mail_search_scope(
        &mut self,
        scope: crate::ui::settings::MailSearchScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mailbox.search.scope == scope {
            return;
        }
        self.mailbox.search.scope = scope;
        // Re-run the active search rather than leaving results that no longer
        // match the scope now displayed next to it.
        let query = self.mailbox.search.query.clone();
        if !query.is_empty() {
            self.submit_search(query, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn set_mail_search_sort(
        &mut self,
        sort: crate::ui::settings::MailSearchSort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::settings::MailSearchSort;
        if self.mailbox.search.sort == sort {
            return;
        }
        self.mailbox.search.sort = sort;
        let query = self.mailbox.search.query.clone();
        match sort {
            // Sorting by date is destructive: relevance is the order results
            // arrived in, and nothing records it. Going back to it means
            // asking the sources again.
            MailSearchSort::Relevance if !query.is_empty() => {
                self.submit_search(query, window, cx);
            }
            MailSearchSort::Relevance => {}
            MailSearchSort::Date => self.sort_search_results(),
        }
        self.invalidate_message_list();
        cx.notify();
    }

    pub(super) fn clear_mail_search(&mut self) {
        let needs_mailbox_load = self.mailbox.clear_search();
        self.invalidate_message_list();
        if needs_mailbox_load {
            self.request_mailbox_refresh();
        }
    }

    pub(super) fn choose_search_suggestion(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_input
            .update(cx, |input, cx| input.set_value(query.clone(), window, cx));
        self.submit_search(query, window, cx);
    }

    pub fn reload_kanban(&mut self) {
        let preview = self.kanban.preview.take();
        for account in self.ordered_accounts() {
            let aid = account.id;
            let columns = self
                .settings
                .account_or_default(Some(&aid))
                .kanban_tag_columns;
            self.kanban.reset_account(&aid, &columns);
            if let Some(tags) = self.tags_by_account.get(&aid) {
                self.kanban.set_tags(&aid, tags.clone());
            }
        }
        self.kanban.preview = preview;
        self.ensure_kanban_loaded();
    }

    fn begin_message_open(&mut self, id: &str, cx: &mut Context<Self>) -> u64 {
        self.pending_message_open_seq = self.pending_message_open_seq.wrapping_add(1);
        // The current body disappears as soon as this selection changes.
        // Signaling its Blitz thread immediately prevents renders from piling
        // up when j/k traverses the list faster than messages can rasterize.
        super::blitz_body::cancel_pending_reader(cx);
        self.mailbox.selected_id = Some(id.to_string());
        self.mailbox.selected = None;
        self.mailbox.thread = None;
        // Clicking the list returns the reader to the selection.
        self.mailbox.active_tab = None;
        self.pending_message_open_seq
    }

    fn cancel_pending_message_open(&mut self, cx: &mut Context<Self>) {
        self.pending_message_open_seq = self.pending_message_open_seq.wrapping_add(1);
        super::blitz_body::cancel_pending_reader(cx);
        self.send(Cmd::CancelOpenMessage);
    }

    pub fn open_message(&mut self, account_id: AccountId, id: String, cx: &mut Context<Self>) {
        self.begin_message_open(&id, cx);
        self.ensure_tags_loaded(&account_id);
        let conversation = self.collapsed_conversation_members(&account_id, &id);
        self.send(Cmd::OpenMessage {
            account_id,
            id: id.clone(),
        });
        if let Some(members) = conversation {
            self.mark_conversation_read(&members, &id, cx);
        }
        cx.notify();
    }

    /// Variant used by j/k and arrow keys: the row is selected immediately,
    /// but only the latest message reaches the runtime after a brief pause in
    /// keyboard repetition.
    pub(crate) fn open_message_debounced(
        &mut self,
        account_id: AccountId,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let generation = self.begin_message_open(&id, cx);
        self.ensure_tags_loaded(&account_id);
        // Do not let the previous open occupy Graph while debouncing the new
        // selection.
        self.send(Cmd::CancelOpenMessage);
        let timer = cx.background_executor().timer(MESSAGE_NAVIGATION_DEBOUNCE);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                if this.pending_message_open_seq == generation
                    && this.mailbox.selected_id.as_deref() == Some(id.as_str())
                {
                    // Reading a thread happens here rather than on the key
                    // press: j/k passing over a row is not opening it, and
                    // must not read the threads it crosses.
                    let conversation = this.collapsed_conversation_members(&account_id, &id);
                    this.send(Cmd::OpenMessage {
                        account_id,
                        id: id.clone(),
                    });
                    if let Some(members) = conversation {
                        this.mark_conversation_read(&members, &id, cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Locally marks a header as read/starred in every list.
    pub(crate) fn update_header(&mut self, id: &str, f: impl Fn(&mut MessageHeader)) {
        self.update_first_header_matching(|header| header.id == id, f);
    }

    /// Account-aware counterpart used by cross-account bulk operations. Two
    /// accounts can hold the same provider id — IMAP ids are folder-local — so
    /// this is what an operation carrying a `MessageRef` should use.
    pub(crate) fn update_header_for(
        &mut self,
        reference: &MessageRef,
        f: impl Fn(&mut MessageHeader),
    ) {
        self.update_first_header_matching(
            |header| header.account_id == reference.account_id && header.id == reference.id,
            f,
        );
    }

    /// Applies `f` to the first matching header of every place one is cached:
    /// the listing, the search results, the sender history, the loaded thread
    /// and the reader's own selection.
    fn update_first_header_matching(
        &mut self,
        matches: impl Fn(&MessageHeader) -> bool,
        f: impl Fn(&mut MessageHeader),
    ) {
        for list in [
            Some(&mut self.mailbox.messages),
            self.mailbox.search.results.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(message) = list.iter_mut().find(|message| matches(message)) {
                f(message);
            }
        }
        if let SenderHistoryState::Loaded { messages, .. } = &mut self.sender_history {
            if let Some(message) = messages.iter_mut().find(|message| matches(message)) {
                f(message);
            }
        }
        if let Some((_, thread)) = &mut self.mailbox.thread {
            if let Some(message) = thread.iter_mut().find(|message| matches(message)) {
                f(message);
            }
        }
        if let Some(selected) = self
            .mailbox
            .selected_mut()
            .filter(|selected| matches(&selected.header))
        {
            f(&mut selected.header);
        }
        self.invalidate_message_list();
    }

    pub(crate) fn invalidate_message_list(&mut self) {
        self.message_list_revision = self.message_list_revision.wrapping_add(1);
        self.message_list_cache = None;
    }

    pub(crate) fn prune_message_row_hover(&mut self) {
        let mut visible = HashSet::new();
        for message in self
            .mailbox
            .messages
            .iter()
            .chain(self.mailbox.search.results.iter().flatten())
        {
            visible.insert(("mailbox", message.account_id.clone(), message.id.clone()));
        }
        if let SenderHistoryState::Loaded { messages, .. } = &self.sender_history {
            for message in messages {
                visible.insert((
                    "sender-history",
                    message.account_id.clone(),
                    message.id.clone(),
                ));
                visible.insert((
                    "contact-history",
                    message.account_id.clone(),
                    message.id.clone(),
                ));
            }
        }
        self.message_row_hover
            .retain(|motion_key| visible.contains(motion_key));
    }

    fn apply_message_tag(&mut self, id: &str, key: &str, added: bool) {
        self.update_header(id, |header| {
            header.tags.retain(|tag| tag != key);
            if added {
                header.tags.push(key.to_string());
            }
        });
        if let Some(message) = self
            .mailbox
            .selected_mut()
            .filter(|message| message.header.id == id)
        {
            message.tags.retain(|tag| tag != key);
            if added {
                message.tags.push(key.to_string());
            }
        }
    }

    /// Pinning at the top of the list is display state specific to
    /// Aviary. It deliberately remains separate from `MessageHeader::is_flagged`
    /// (Outlook flag, Gmail star, or IMAP `\\Flagged`).
    pub(crate) fn is_message_pinned(&self, message: &MessageHeader) -> bool {
        self.settings
            .accounts
            .get(&message.account_id)
            .is_some_and(|settings| settings.pinned_message_ids.contains(&message.id))
    }

    pub(crate) fn set_message_pinned(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        pinned: bool,
    ) {
        let changed = if pinned {
            let ids = &mut self.settings.account_mut(account_id).pinned_message_ids;
            if ids.iter().any(|id| id == message_id) {
                false
            } else {
                ids.push(message_id.to_string());
                true
            }
        } else if let Some(settings) = self.settings.accounts.get_mut(account_id) {
            let previous_len = settings.pinned_message_ids.len();
            settings.pinned_message_ids.retain(|id| id != message_id);
            settings.pinned_message_ids.len() != previous_len
        } else {
            false
        };
        if changed {
            self.settings.save();
            self.invalidate_message_list();
        }
    }

    /// Pins or unpins a whole conversation.
    ///
    /// Pinning marks only the newest loaded member: a thread counts as pinned
    /// as soon as one of its messages is, so that single id keeps the group
    /// at the top even as replies arrive. Unpinning has to clear every member
    /// instead, or an older marked message would silently pin the thread
    /// again.
    pub(crate) fn set_conversation_pinned(&mut self, members: &[MessageRef], pinned: bool) {
        if pinned {
            let Some(newest) = members.first() else {
                return;
            };
            self.set_message_pinned(&newest.account_id, &newest.id, true);
            return;
        }
        for member in members {
            self.set_message_pinned(&member.account_id, &member.id, false);
        }
    }

    pub(crate) fn replace_pinned_message_id(
        &mut self,
        account_id: &AccountId,
        old_id: &str,
        new_id: Option<&str>,
    ) {
        let ids = &mut self.settings.account_mut(account_id).pinned_message_ids;
        let Some(position) = ids.iter().position(|id| id == old_id) else {
            return;
        };
        if let Some(new_id) = new_id {
            ids[position] = new_id.to_string();
            self.settings.save();
            self.invalidate_message_list();
        }
    }

    pub(super) fn remove_message_everywhere(&mut self, id: &str) {
        if let Some(sent) = self.mailbox.sent_messages.remove(id) {
            for message in sent {
                self.mailbox
                    .expanded_sent_messages
                    .remove(&message.message.header.id);
            }
        }
        self.mailbox.messages.retain(|m| m.id != id);
        self.mailbox
            .selected_messages
            .retain(|reference| reference.id != id);
        if self
            .mailbox
            .selection_anchor
            .as_ref()
            .is_some_and(|reference| reference.id == id)
        {
            self.mailbox.selection_anchor = None;
        }
        if let Some(res) = &mut self.mailbox.search.results {
            res.retain(|m| m.id != id);
        }
        self.invalidate_message_list();
        if let SenderHistoryState::Loaded { messages, .. } = &mut self.sender_history {
            messages.retain(|m| m.id != id);
        }
        if self.mailbox.selected_id.as_deref() == Some(id) {
            self.mailbox.selected = None;
            self.mailbox.selected_id = None;
            self.mailbox.thread = None;
        }
        for board in self.kanban.accounts.values_mut() {
            for column in &mut board.columns {
                column.messages.retain(|message| message.id != id);
            }
        }
        self.kanban.invalidate_merged();
        if self
            .kanban
            .preview
            .as_ref()
            .is_some_and(|(_, message_id)| message_id == id)
        {
            self.kanban.preview = None;
        }
        if let Some(ix) = self.mailbox.open_tabs.iter().position(|tab| {
            tab.message_ref()
                .is_some_and(|reference| reference.id == id)
        }) {
            self.close_viewer_tab(ix);
        }
        self.pending_rehydrate
            .retain(|reference| reference.id != id);
        self.pending_sent_restore
            .retain(|pending| pending.related_to != id && pending.session.message.id != id);
        if self
            .inline_reply
            .as_ref()
            .is_some_and(|r| r.message_id == id)
        {
            self.inline_reply = None;
        }
        if self
            .pending_inline_reply
            .as_ref()
            .is_some_and(|reply| reply.message_id == id)
        {
            self.pending_inline_reply = None;
        }
    }

    pub(super) fn update_tray_unread(&self) {
        #[cfg(target_os = "linux")]
        if let Some(tray) = &self.tray {
            let unread = self.mailbox.messages.iter().filter(|m| !m.is_read).count();
            tray.set_unread(unread as u32);
        }
    }

    // ----------------------------------------------------------------
    // Rendering
    // ----------------------------------------------------------------

    pub fn start_microsoft_login(&mut self, cx: &mut Context<Self>) {
        let client_id = if self.settings.global.azure_client_id.is_empty() {
            auth::DEFAULT_CLIENT_ID.to_string()
        } else {
            self.settings.global.azure_client_id.clone()
        };
        let tenant = if self.settings.global.azure_tenant.is_empty() {
            auth::DEFAULT_TENANT.to_string()
        } else {
            self.settings.global.azure_tenant.clone()
        };
        self.auth = AuthState::StartingMicrosoft;
        self.send(Cmd::StartLogin { client_id, tenant });
        log::info!("Microsoft connection in progress");
        cx.notify();
    }

    pub fn start_google_login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client_id = if self.settings.global.google_client_id.is_empty() {
            auth::DEFAULT_GOOGLE_CLIENT_ID.to_string()
        } else {
            self.settings.global.google_client_id.clone()
        };
        let client_secret = if self.settings.global.google_client_secret.is_empty() {
            auth::DEFAULT_GOOGLE_CLIENT_SECRET.to_string()
        } else {
            self.settings.global.google_client_secret.clone()
        };
        // Both halves are required, and checking only the id would let two
        // mismatched halves reach Google: a build with no bundled secret, or a
        // user who filled in their own client id and left the secret blank,
        // would pair it with whatever the binary shipped and fail at the code
        // exchange with a message from Google rather than from us.
        if client_id.is_empty() || client_secret.is_empty() {
            self.notify_error(tr!("toast-google-not-configured"), window, cx);
            return;
        }
        self.send(Cmd::StartGoogleLogin {
            client_id,
            client_secret,
        });
        log::info!("Google connection in progress");
        cx.notify();
    }
}

#[cfg(test)]
mod bulk_completion_tests {
    use super::*;

    fn reference(id: &str) -> MessageRef {
        MessageRef {
            account_id: AccountId("account-a".into()),
            id: id.to_string(),
        }
    }

    fn batch(references: &[MessageRef], message: &str) -> BulkCompletions {
        let mut completions = BulkCompletions::default();
        let id = completions.claim(references);
        completions.arm(id, references, message.to_string(), "batch-key".into());
        completions
    }

    /// The case the aggregation exists for: neither "moved" nor "failed" is
    /// true on its own, and the user is told both, once.
    #[test]
    fn a_partial_batch_reports_both_counts_on_its_last_reply() {
        let references = [reference("a"), reference("b"), reference("c")];
        let mut completions = batch(&references, "3 moved");

        assert!(matches!(
            completions.record(&references[0], None),
            BulkReply::Pending
        ));
        assert!(matches!(
            completions.record(&references[1], Some("mailbox full".into())),
            BulkReply::Pending
        ));
        let completion = completions
            .record(&references[2], None)
            .completion()
            .expect("last reply closes the batch");

        assert_eq!(completion.succeeded, 2);
        assert_eq!(completion.failed, 1);
        assert_eq!(completion.total(), 3);
        assert_eq!(completion.first_error.as_deref(), Some("mailbox full"));
    }

    /// A batch that went through keeps its own copy: the summary is only for
    /// the outcomes the action's own wording cannot express.
    #[test]
    fn a_batch_that_fully_succeeds_carries_its_copy() {
        let references = [reference("a"), reference("b")];
        let mut completions = batch(&references, "2 deleted");

        completions.record(&references[0], None);
        let completion = completions
            .record(&references[1], None)
            .completion()
            .expect("closes the batch");

        assert_eq!(completion.failed, 0);
        assert_eq!(completion.message, "2 deleted");
    }

    /// Only the first error is kept: a toast listing eight provider messages
    /// is one nobody reads.
    #[test]
    fn only_the_first_error_of_a_batch_is_kept() {
        let references = [reference("a"), reference("b")];
        let mut completions = batch(&references, "");

        completions.record(&references[0], Some("first".into()));
        let completion = completions
            .record(&references[1], Some("second".into()))
            .completion()
            .expect("closes the batch");

        assert_eq!(completion.failed, 2);
        assert_eq!(completion.first_error.as_deref(), Some("first"));
    }

    #[test]
    fn a_message_outside_any_batch_reports_itself() {
        let mut completions = BulkCompletions::default();
        assert!(!completions.record(&reference("lone"), None).is_bulk());
    }

    /// Every message of an offline batch is deferred at once, and they all say
    /// the same thing.
    #[test]
    fn only_the_first_deferral_of_a_batch_speaks() {
        let references = [reference("a"), reference("b")];
        let completions = &mut batch(&references, "");

        let first = completions.note_deferral(&references[0]);
        assert!(first.bulk && first.first);
        assert!(!completions.note_deferral(&references[1]).first);
        // Deferring is not terminal: the messages are still expected.
        assert!(matches!(
            completions.record(&references[0], None),
            BulkReply::Pending
        ));
    }

    /// Undo and the aggregation window both drop a batch whole — but a message
    /// the user has acted on again belongs to the newer batch, which must keep
    /// its index.
    #[test]
    fn forgetting_a_batch_leaves_a_later_one_indexed() {
        let references = [reference("a")];
        let mut completions = BulkCompletions::default();
        let old = completions.claim(&references);
        completions.arm(old, &references, String::new(), "old".into());
        let new = completions.claim(&references);
        completions.arm(new, &references, String::new(), "new".into());

        completions.forget(old, &references);

        assert!(completions.record(&references[0], None).is_bulk());
    }
}
