//! UI/runtime protocol: commands sent by the UI (`Cmd`),
//! events returned by the runtime (`Evt`), and the composite payloads they
//! carry. This module contains data only, with no logic.

use crate::auth::ImapConfig;
use crate::model::{
    Account, AccountId, Attachment, CalendarEvent, Contact, IcalSubscription, InlineImage,
    InvitationResponse, MailFolder, Message, MessageHeader, Provider, SentMessage,
};
use crate::proofreading::{LanguageToolSettings, LanguageToolStatus, ProofreadingIssue};
use crate::providers::{NewCalendarEvent, OnlineMeetingKind, OutgoingMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug)]
pub struct AiEditRequest {
    pub compose_id: u64,
    pub config: crate::ai::AiConfig,
    pub system_prompt: String,
    pub prompt_template: String,
    pub instruction: String,
    pub subject: String,
    pub body_markdown: String,
}

#[derive(Debug, Clone)]
pub struct UnifiedAccountPage {
    pub account_id: AccountId,
    pub page_size: usize,
}

/// Where a search looks inside one account.
///
/// Cross-account scope is not represented here: the UI dispatches one
/// `Cmd::Search` per account it wants covered, as it does for refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchScope {
    /// Every folder of the account.
    Account,
    /// A single folder. `None` is the inbox — the same convention as
    /// `Cmd::Refresh` and the cache, where an unnamed folder means the inbox
    /// rather than "no folder".
    Folder(Option<String>),
}

impl SearchScope {
    /// Folder this scope restricts to, if any.
    pub(crate) fn folder(&self) -> Option<Option<&str>> {
        match self {
            Self::Account => None,
            Self::Folder(folder_id) => Some(folder_id.as_deref()),
        }
    }
}

/// Identifies a durable mutation closely enough for the UI to keep the
/// pending, in-progress, and final notification in one toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageMutationKind {
    Delete,
    Move,
    SetFlag(bool),
    MarkRead(bool),
}

/// Persisted recipient-selection statistics used to rank contact completion.
/// Addresses are normalized to lowercase before reaching this protocol type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientUsage {
    pub email: String,
    pub use_count: u64,
    pub last_used: i64,
}

/// Complete outgoing message shared by `Cmd::SendMail` and `Cmd::SaveDraft`.
/// The body is already HTML produced by the
/// blocks (`body_is_html: true` en pratique).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub body_is_html: bool,
    /// Inline images referenced through `cid:` in the body.
    pub attachments: Vec<InlineImage>,
    /// Non-inline attachments (bytes required).
    pub files: Vec<Attachment>,
}

impl OutgoingMail {
    /// Borrowed view in the format expected by provider backends.
    pub(crate) fn as_outgoing<'a>(&'a self, from: &'a str) -> OutgoingMessage<'a> {
        OutgoingMessage {
            from,
            to: &self.to,
            cc: &self.cc,
            bcc: &self.bcc,
            subject: &self.subject,
            body: &self.body,
            body_is_html: self.body_is_html,
            attachments: &self.attachments,
            files: &self.files,
        }
    }
}

/// Concrete, account-resolved snapshot of a quick action. Settings may change
/// after the click, so runtime execution never refers back to mutable UI state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickActionExecution {
    pub execution_id: u64,
    pub action_name: String,
    pub message_id: String,
    pub steps: Vec<QuickActionStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QuickActionStep {
    Forward {
        mail: OutgoingMail,
    },
    Reply {
        mail: OutgoingMail,
        reply_all: bool,
    },
    RemoveTag {
        tag_id: String,
    },
    AddTag {
        tag_id: String,
    },
    MarkRead {
        read: bool,
    },
    SetFlag {
        flagged: bool,
    },
    Move {
        source_folder_id: Option<String>,
        target_folder_id: String,
    },
}

/// Calendar event draft carried by `Cmd::CreateEvent`.
/// UTC instants; the UI converts local input before sending.
#[derive(Debug)]
pub struct EventDraft {
    pub subject: String,
    pub description: String,
    pub location: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub online_meeting: Option<OnlineMeetingKind>,
    pub attendees: Vec<String>,
}

impl EventDraft {
    /// Borrowed view in the format expected by provider backends.
    pub(crate) fn as_new_event(&self) -> NewCalendarEvent<'_> {
        NewCalendarEvent {
            subject: &self.subject,
            description: &self.description,
            location: &self.location,
            start: self.start,
            end: self.end,
            all_day: self.all_day,
            online_meeting: self.online_meeting,
            attendees: &self.attendees,
        }
    }
}

#[derive(Debug)]
pub enum Cmd {
    /// Generates or transforms an email body through the configured AI
    /// provider. The response keeps `compose_id` so multiple editors can be
    /// routed in parallel.
    EditMailWithAi(AiEditRequest),
    ConfigureLanguageTool(LanguageToolSettings),
    InstallLanguageTool,
    UninstallLanguageTool,
    TestLanguageTool(LanguageToolSettings),
    ResetLanguageTool,
    CheckLanguageTool {
        editor_id: String,
        block_id: u64,
        revision: u64,
        text: String,
        ui_language: String,
    },
    StartLogin {
        client_id: String,
        tenant: String,
    },
    StartGoogleLogin {
        client_id: String,
        client_secret: String,
    },
    StartImapLogin {
        config: ImapConfig,
        password: String,
    },
    Logout(AccountId),
    SetMailCacheLimit {
        limit_mb: u64,
    },
    ClearMailCache,
    GetMailCacheStats,
    Refresh {
        account_id: AccountId,
        folder_id: Option<String>,
        limit: usize,
    },
    /// Initializes merged chronological pagination for inboxes. Each account
    /// retains its own provider cursor.
    RefreshUnified {
        request_id: u64,
        accounts: Vec<UnifiedAccountPage>,
        page_size: usize,
    },
    LoadMore {
        account_id: AccountId,
        folder_id: Option<String>,
        skip: usize,
        limit: usize,
    },
    LoadMoreUnified {
        request_id: u64,
    },
    OpenMessage {
        account_id: AccountId,
        id: String,
    },
    /// Interrupts the active primary open without starting a new one.
    /// Used while debouncing keyboard navigation.
    CancelOpenMessage,
    /// Restores a session reference directly from SQLite without requiring a
    /// live provider account. Cache misses stay silent and may be fetched
    /// through `LoadThreadMessage` once authentication completes.
    LoadCachedMessage {
        account_id: AccountId,
        id: String,
    },
    /// Loads a complete message for a background quick action without
    /// selecting it or marking it read.
    LoadQuickActionMessage {
        request_id: u64,
        account_id: AccountId,
        id: String,
    },
    /// Fetch one regular attachment on demand. Incoming messages initially
    /// carry only attachment metadata; inline CID images remain eager because
    /// the body renderer needs them immediately.
    FetchAttachment {
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
    },
    /// Lazy-load the body of a message that lives inside the currently
    /// displayed conversation thread, *without* replacing
    /// `mailbox.selected`. Reply lands as `Evt::ThreadMessageLoaded` /
    /// `Evt::ThreadMessageError`. Marks the message read like
    /// `OpenMessage` does — expanding the entry counts as reading it.
    LoadThreadMessage {
        account_id: AccountId,
        id: String,
    },
    DeleteMessage {
        account_id: AccountId,
        id: String,
    },
    LoadThread {
        account_id: AccountId,
        conversation_id: String,
    },
    Search {
        account_id: AccountId,
        query: String,
        scope: SearchScope,
        limit: usize,
    },
    SetFlag {
        account_id: AccountId,
        id: String,
        flagged: bool,
    },
    /// Explicit mark-read/unread, used by the context menu. Opening a message
    /// already marks it read automatically; this lets the user toggle without
    /// opening (e.g., right-click → "Marquer comme non lu").
    MarkRead {
        account_id: AccountId,
        id: String,
        read: bool,
    },
    ScheduleQuickAction {
        account_id: AccountId,
        execution: QuickActionExecution,
        delay_secs: u32,
    },
    CancelQuickAction {
        account_id: AccountId,
        execution_id: u64,
    },
    SetAutoRefresh {
        account_id: AccountId,
        folder_id: Option<String>,
        secs: u32,
        limit: usize,
    },
    LoadFolders {
        account_id: AccountId,
    },
    CreateFolder {
        account_id: AccountId,
        name: String,
        parent_id: Option<String>,
    },
    RenameFolder {
        account_id: AccountId,
        id: String,
        new_name: String,
    },
    DeleteFolder {
        account_id: AccountId,
        id: String,
    },
    /// Move a message into another folder. `source_folder_id` is the folder
    /// the user was viewing when they triggered the move (Gmail needs it to
    /// know which label to drop; Graph and IMAP ignore it).
    MoveMessage {
        account_id: AccountId,
        message_id: String,
        source_folder_id: Option<String>,
        target_folder_id: String,
    },
    LoadTags {
        account_id: AccountId,
    },
    CreateTag {
        account_id: AccountId,
        name: String,
        color: Option<u32>,
    },
    RenameTag {
        account_id: AccountId,
        id: String,
        new_name: String,
    },
    DeleteTag {
        account_id: AccountId,
        id: String,
    },
    /// Change a tag's color on the provider (Outlook preset / Gmail label
    /// palette). `color` is packed sRGB from
    /// `providers::tag_color_palette`. Replies with `Evt::TagColorSet`.
    SetTagColor {
        account_id: AccountId,
        id: String,
        color: u32,
    },
    AddTag {
        account_id: AccountId,
        message_id: String,
        tag_id: String,
    },
    RemoveTag {
        account_id: AccountId,
        message_id: String,
        tag_id: String,
    },
    /// Fetch the page of messages bearing `tag_id`. Used by the kanban
    /// pane to populate one column per visible tag.
    LoadTagListing {
        account_id: AccountId,
        tag_id: String,
        limit: usize,
    },
    LoadCalendar {
        account_id: AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    RespondToInvitation {
        account_id: AccountId,
        message_id: String,
        event_id: String,
        response: InvitationResponse,
    },
    ConfigureIcalSubscriptions(Vec<IcalSubscription>),
    LoadIcalCalendar {
        subscription_id: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        force_refresh: bool,
    },
    RefreshIcalSubscription {
        subscription_id: String,
    },
    DeleteIcalSubscriptionCache {
        subscription_id: String,
    },
    CreateEvent {
        /// Echoed back in `Evt::EventCreated` / `Evt::EventCreateError` so
        /// the UI can match the response to the originating compose
        /// (multiple are supported in flight). Set to the compose's
        /// `editor_id`.
        request_id: u64,
        account_id: AccountId,
        event: EventDraft,
    },
    UpdateCalendarEvent {
        request_id: u64,
        account_id: AccountId,
        event_id: String,
        event: EventDraft,
    },
    DeleteCalendarEvent {
        account_id: AccountId,
        event_id: String,
    },
    /// Move an existing event while preserving its duration. The previous
    /// instants are echoed on failure so the optimistic UI can roll back.
    MoveCalendarEvent {
        account_id: AccountId,
        event_id: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        previous_start: DateTime<Utc>,
        previous_end: DateTime<Utc>,
        all_day: bool,
    },
    LoadContacts {
        account_id: AccountId,
    },
    LoadSenderHistory {
        account_id: AccountId,
        email: String,
        limit: usize,
    },
    LoadMoreSenderHistory {
        account_id: AccountId,
        email: String,
        next_link: String,
    },
    SendMail {
        account_id: AccountId,
        compose_id: u64,
        reply_to: Option<String>,
        /// Distinguishes reply from reply-all when tracking the latest action
        /// on the original message.
        reply_all: bool,
        /// Original message ID when this composition is a forward.
        /// Unlike `reply_to`, the outgoing message remains a new message; the
        /// runtime uses it only to mark the original as
        /// forwarded (`Session::note_last_action`) after a successful send.
        forward_of: Option<String>,
        /// Provider-native draft id when the user is sending an existing
        /// draft. The runtime deletes that draft after the send succeeds
        /// so it doesn't linger in the Drafts folder.
        draft_id: Option<String>,
        mail: OutgoingMail,
    },
    /// Fetch the provider's real Sent-items copy behind a reply/forward
    /// snapshot (`SentMessage`), using the identifiers captured at send
    /// time. Replies with `Evt::SentCopyResolved` on success; stays silent
    /// (log only) when the copy cannot be found, leaving the snapshot as-is.
    FetchSentCopy {
        account_id: AccountId,
        /// Id of the original message the snapshot is attached to (the
        /// `sent_messages` map key on the UI side).
        related_to: String,
        /// Local snapshot id (`aviary-sent-…`) identifying which entry to
        /// replace.
        snapshot_id: String,
        sent_id: Option<String>,
        internet_message_id: Option<String>,
    },
    /// Save the current compose as a draft instead of sending it. Same shape
    /// as `SendMail` minus the reply context — the provider stores the
    /// outgoing message in its Drafts folder and returns control to the UI
    /// via `Evt::DraftSaved` / `Evt::DraftSaveError`. When `replace_id` is
    /// `Some`, the existing draft is updated in place rather than creating
    /// a duplicate.
    SaveDraft {
        account_id: AccountId,
        compose_id: u64,
        replace_id: Option<String>,
        mail: OutgoingMail,
        /// Provider autosaves stay quiet and do not lock or close the editor.
        autosave: bool,
    },
    /// Download an external image (typically from an `<img src="https://...">`
    /// the user pasted as HTML) so we can embed it as a real inline image
    /// instead of a hotlink. The block editor registers a placeholder
    /// `InlineImage { cid, .. }` immediately and swaps in the bytes when the
    /// matching `Evt::InlineImageFetched` arrives.
    ///
    /// `editor_id` is the editor's scope, like `Cmd::CheckLanguageTool`: the
    /// same body can be open in several editors (a detached composer and the
    /// reply panel), and only the one that pasted the image must be updated.
    FetchInlineImage {
        editor_id: String,
        cid: String,
        url: String,
    },
}

#[derive(Debug)]
pub enum Evt {
    LanguageToolStatus(LanguageToolStatus),
    LanguageToolChecked {
        editor_id: String,
        block_id: u64,
        revision: u64,
        source: String,
        issues: Vec<ProofreadingIssue>,
    },
    LanguageToolCheckFailed {
        editor_id: String,
        block_id: u64,
        revision: u64,
        error: String,
    },
    AiMailEditChunk {
        compose_id: u64,
        delta: String,
    },
    AiMailEditFinished {
        compose_id: u64,
        markdown: String,
    },
    AiMailEditError {
        compose_id: u64,
        error: String,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        message: String,
    },
    GoogleAuthOpening {
        auth_url: String,
    },
    Authenticated,
    AccountReady(Account),
    /// A token file exists for this account, but it could not be restored
    /// (most often because its refresh token was revoked). Keeping this
    /// separate from `Error` lets Settings expose and remove the stale
    /// persisted account instead of making it invisible.
    AccountRestoreFailed {
        account_id: AccountId,
        provider: Provider,
        error: String,
    },
    Messages {
        account_id: AccountId,
        messages: Vec<MessageHeader>,
    },
    /// First local response to a refresh. It displays the mailbox without
    /// waiting for the network; `Messages` remains the authoritative remote
    /// response and arrives later when the connection is available.
    CachedMessages {
        account_id: AccountId,
        folder_id: Option<String>,
        messages: Vec<MessageHeader>,
    },
    /// How many messages the local cache knows of per thread, for the folder
    /// that was just listed. Threads of a single message are absent.
    ///
    /// The message list only holds the pages it has loaded, so counting there
    /// would make a group's counter climb as the user scrolls. The cache has
    /// seen far more, and answers without a network round trip.
    ConversationTotals {
        account_id: AccountId,
        folder_id: Option<String>,
        totals: HashMap<String, usize>,
    },
    MoreMessages {
        account_id: AccountId,
        messages: Vec<MessageHeader>,
        has_more: bool,
    },
    UnifiedMessages {
        request_id: u64,
        messages: Vec<MessageHeader>,
        has_more: bool,
    },
    UnifiedCachedMessages {
        request_id: u64,
        messages: Vec<MessageHeader>,
    },
    UnifiedMoreMessages {
        request_id: u64,
        messages: Vec<MessageHeader>,
        has_more: bool,
    },
    NewMessages {
        account_id: AccountId,
        messages: Vec<MessageHeader>,
    },
    MessageChanges {
        account_id: AccountId,
        folder_id: Option<String>,
        upserts: Vec<MessageHeader>,
        deleted: Vec<String>,
    },
    MessageOpened {
        account_id: AccountId,
        message: Box<Message>,
    },
    CachedMessageOpened {
        account_id: AccountId,
        message: Box<Message>,
    },
    QuickActionMessageLoaded {
        request_id: u64,
        account_id: AccountId,
        message: Box<Message>,
    },
    QuickActionMessageError {
        request_id: u64,
        account_id: AccountId,
        error: String,
    },
    AttachmentFetched {
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        bytes: Vec<u8>,
    },
    AttachmentFetchError {
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        error: String,
    },
    SyncStateChanged {
        account_id: AccountId,
        online: bool,
        error: Option<String>,
    },
    /// A durable message mutation was accepted locally and will be retried
    /// after connectivity recovers.
    MutationDeferred {
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
        kind: MessageMutationKind,
    },
    /// The provider acknowledged a mutation previously persisted in the local
    /// operation queue.
    MutationSucceeded {
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
    },
    /// A non-transient mutation failure needs reconciliation with the server.
    MutationFailed {
        account_id: AccountId,
        operation_id: i64,
        message_id: String,
        kind: MessageMutationKind,
        /// Last server-confirmed value, used to roll back optimistic UI state.
        header: Option<MessageHeader>,
        error: String,
    },
    /// An outgoing message is safely persisted and waiting for connectivity.
    OutboxQueued {
        account_id: AccountId,
        operation_id: i64,
        compose_id: u64,
    },
    QuickActionCompleted {
        account_id: AccountId,
        execution_id: u64,
        action_name: String,
        message_id: String,
    },
    QuickActionStarted {
        account_id: AccountId,
        execution_id: u64,
        action_name: String,
    },
    QuickActionCancelled {
        account_id: AccountId,
        execution_id: u64,
        action_name: String,
    },
    QuickActionFailed {
        account_id: AccountId,
        remaining: QuickActionExecution,
        completed_steps: usize,
        error: String,
    },
    /// The process stopped while the provider may have accepted a direct
    /// reply or forward. It must never be retried automatically.
    QuickActionSendUncertain {
        account_id: AccountId,
        remaining: QuickActionExecution,
    },
    QuickActionMessageState {
        account_id: AccountId,
        message_id: String,
        read: Option<bool>,
        flagged: Option<bool>,
    },
    MailCacheStats {
        used_bytes: u64,
        limit_bytes: u64,
    },
    MailCacheCleared,
    /// Reply to `Cmd::LoadThreadMessage` — body of a thread entry the
    /// user expanded inline. Routed separately from `MessageOpened` so
    /// `mailbox.selected` keeps pointing at the message the user is
    /// actually viewing as primary.
    ThreadMessageLoaded {
        account_id: AccountId,
        id: String,
        message: Box<Message>,
    },
    ThreadMessageError {
        account_id: AccountId,
        id: String,
        error: String,
    },
    Thread {
        account_id: AccountId,
        conversation_id: String,
        messages: Vec<MessageHeader>,
    },
    SearchResults {
        account_id: AccountId,
        query: String,
        messages: Vec<MessageHeader>,
    },
    CalendarEvents {
        account_id: AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    },
    IcalEvents {
        subscription_id: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    },
    IcalSyncState {
        subscription_id: String,
        syncing: bool,
        error: Option<String>,
        last_success: Option<DateTime<Utc>>,
    },
    IcalFeedUpdated {
        subscription_id: String,
    },
    EventCreated {
        /// Mirror of `Cmd::CreateEvent::request_id` — lets the UI close
        /// the right compose when several are submitted concurrently.
        request_id: u64,
        account_id: AccountId,
    },
    EventCreateError {
        request_id: u64,
        account_id: AccountId,
        error: String,
    },
    CalendarEventUpdated {
        request_id: u64,
        account_id: AccountId,
    },
    CalendarEventUpdateError {
        request_id: u64,
        account_id: AccountId,
        error: String,
    },
    CalendarEventDeleted {
        account_id: AccountId,
        event_id: String,
    },
    CalendarEventDeleteError {
        account_id: AccountId,
        event_id: String,
        error: String,
    },
    CalendarEventMoved {
        account_id: AccountId,
        event_id: String,
    },
    CalendarEventMoveError {
        account_id: AccountId,
        event_id: String,
        previous_start: DateTime<Utc>,
        previous_end: DateTime<Utc>,
        error: String,
    },
    InvitationResponded {
        account_id: AccountId,
        message_id: String,
        response: InvitationResponse,
    },
    InvitationResponseError {
        account_id: AccountId,
        message_id: String,
        error: String,
    },
    SenderHistory {
        account_id: AccountId,
        email: String,
        messages: Vec<MessageHeader>,
        next_link: Option<String>,
    },
    SenderHistoryMore {
        account_id: AccountId,
        email: String,
        messages: Vec<MessageHeader>,
        next_link: Option<String>,
    },
    SenderHistoryError {
        account_id: AccountId,
        email: String,
        loading_more: bool,
        error: String,
    },
    Contacts {
        account_id: AccountId,
        contacts: Vec<Contact>,
    },
    /// Initial snapshot and incremental updates from the local SQLite
    /// recipient-frequency table.
    RecipientUsage {
        entries: Vec<RecipientUsage>,
    },
    Folders {
        account_id: AccountId,
        folders: Vec<MailFolder>,
    },
    FolderCreated {
        account_id: AccountId,
        folder: MailFolder,
    },
    FolderRenamed {
        account_id: AccountId,
        id: String,
        /// New native id for path-addressed providers such as IMAP.
        new_id: Option<String>,
        new_name: String,
    },
    FolderDeleted {
        account_id: AccountId,
        id: String,
    },
    /// Reply to `Cmd::MoveMessage`. `new_id` is set when the provider
    /// reassigned the id (Graph always; IMAP sometimes); `None` means the
    /// caller can keep the old id (Gmail) or that the id is gone from the
    /// source folder anyway.
    MessageMoved {
        account_id: AccountId,
        message_id: String,
        #[allow(dead_code)]
        source_folder_id: Option<String>,
        target_folder_id: String,
        new_id: Option<String>,
    },
    Tags {
        account_id: AccountId,
        tags: Vec<crate::model::Tag>,
    },
    TagCreated {
        account_id: AccountId,
        tag: crate::model::Tag,
    },
    TagRenamed {
        account_id: AccountId,
        id: String,
        /// Replacement native id when the provider implements rename by
        /// recreating the tag (Microsoft Graph categories).
        new_id: Option<String>,
        /// Old provider value stored on messages when it changes as part of
        /// the rename (Graph stores category display names, not master ids).
        old_message_tag: Option<String>,
        new_name: String,
    },
    TagDeleted {
        account_id: AccountId,
        id: String,
    },
    /// Reply to `Cmd::SetTagColor`: the provider accepted the new color.
    TagColorSet {
        account_id: AccountId,
        id: String,
        color: u32,
    },
    /// Confirms that `tag_id` was added/removed on `message_id`. Carried so
    /// the kanban pane can update its in-memory column immediately without
    /// re-fetching.
    TagApplied {
        account_id: AccountId,
        message_id: String,
        tag_id: String,
        added: bool,
    },
    TagApplyError {
        account_id: AccountId,
        message_id: String,
        tag_id: String,
        added: bool,
        error: String,
    },
    TagListing {
        account_id: AccountId,
        tag_id: String,
        messages: Vec<MessageHeader>,
    },
    MessageDeleted {
        account_id: AccountId,
        id: String,
    },
    MailSent {
        account_id: AccountId,
        compose_id: u64,
        /// Exact outgoing reply/forward, when this send was related to an
        /// existing message. It remains available even when the provider's
        /// send endpoint does not return a Sent-item id.
        sent_message: Option<SentMessage>,
    },
    /// Reply to `Cmd::FetchSentCopy`: the provider's Sent-items copy of the
    /// snapshot identified by `snapshot_id` under `related_to`. The UI swaps
    /// the local snapshot for this real message.
    SentCopyResolved {
        account_id: AccountId,
        related_to: String,
        snapshot_id: String,
        message: Box<Message>,
    },
    /// Message `id` was just marked as replied to or forwarded on the server
    /// after a successful send. The UI updates its headers in place to show
    /// the banner/icon without refetching.
    MessageActionNoted {
        account_id: AccountId,
        id: String,
        action: crate::model::LastAction,
        at: DateTime<Utc>,
    },
    MailSendError {
        account_id: AccountId,
        compose_id: u64,
        error: String,
    },
    /// Reply to `Cmd::SaveDraft`. `draft_id` carries the provider-native id
    /// of the resulting draft so the compose UI can target subsequent saves
    /// at the same draft (avoiding duplicates). `None` only for IMAP
    /// servers without UIDPLUS.
    DraftSaved {
        account_id: AccountId,
        compose_id: u64,
        draft_id: Option<String>,
        autosave: bool,
    },
    DraftSaveError {
        account_id: AccountId,
        compose_id: u64,
        error: String,
        autosave: bool,
    },
    LoggedOut {
        account_id: AccountId,
    },
    /// Reply to `Cmd::FetchInlineImage`: the block editor with `editor_id`
    /// should swap the placeholder bytes of the inline image matching `cid` for
    /// these real bytes and republish them so the Image block updates.
    InlineImageFetched {
        editor_id: String,
        cid: String,
        bytes: Vec<u8>,
        mime: String,
    },
    /// Reply to `Cmd::FetchInlineImage` when the download failed. The editor
    /// restores the original URL, so the body keeps the hotlink it was pasted
    /// with rather than losing the image entirely.
    InlineImageFetchError {
        editor_id: String,
        cid: String,
        error: String,
    },
    Status(String),
    Error(String),
}

impl Evt {
    /// Whether the message rows are rebuilt, leaving their memoized hover
    /// state stale. Matching exhaustively is deliberate: a new event has to
    /// be classified here instead of silently defaulting to one behaviour.
    pub(crate) fn prunes_message_row_hover(&self) -> bool {
        match self {
            Self::Messages { .. }
            | Self::CachedMessages { .. }
            | Self::MoreMessages { .. }
            | Self::UnifiedMessages { .. }
            | Self::UnifiedCachedMessages { .. }
            | Self::UnifiedMoreMessages { .. }
            | Self::NewMessages { .. }
            | Self::MessageChanges { .. }
            | Self::MessageOpened { .. }
            | Self::CachedMessageOpened { .. }
            | Self::QuickActionMessageLoaded { .. }
            | Self::QuickActionMessageError { .. }
            | Self::SearchResults { .. }
            | Self::SenderHistory { .. }
            | Self::SenderHistoryMore { .. }
            | Self::SenderHistoryError { .. }
            | Self::MessageMoved { .. }
            | Self::MessageDeleted { .. }
            | Self::LoggedOut { .. } => true,
            Self::ConversationTotals { .. }
            | Self::LanguageToolStatus { .. }
            | Self::LanguageToolChecked { .. }
            | Self::LanguageToolCheckFailed { .. }
            | Self::AiMailEditChunk { .. }
            | Self::AiMailEditFinished { .. }
            | Self::AiMailEditError { .. }
            | Self::DeviceCode { .. }
            | Self::GoogleAuthOpening { .. }
            | Self::Authenticated { .. }
            | Self::AccountReady { .. }
            | Self::AccountRestoreFailed { .. }
            | Self::AttachmentFetched { .. }
            | Self::AttachmentFetchError { .. }
            | Self::SyncStateChanged { .. }
            | Self::MutationDeferred { .. }
            | Self::MutationSucceeded { .. }
            | Self::MutationFailed { .. }
            | Self::OutboxQueued { .. }
            | Self::QuickActionCompleted { .. }
            | Self::QuickActionStarted { .. }
            | Self::QuickActionCancelled { .. }
            | Self::QuickActionFailed { .. }
            | Self::QuickActionSendUncertain { .. }
            | Self::QuickActionMessageState { .. }
            | Self::MailCacheStats { .. }
            | Self::MailCacheCleared { .. }
            | Self::ThreadMessageLoaded { .. }
            | Self::ThreadMessageError { .. }
            | Self::Thread { .. }
            | Self::CalendarEvents { .. }
            | Self::IcalEvents { .. }
            | Self::IcalSyncState { .. }
            | Self::IcalFeedUpdated { .. }
            | Self::EventCreated { .. }
            | Self::EventCreateError { .. }
            | Self::CalendarEventUpdated { .. }
            | Self::CalendarEventUpdateError { .. }
            | Self::CalendarEventDeleted { .. }
            | Self::CalendarEventDeleteError { .. }
            | Self::CalendarEventMoved { .. }
            | Self::CalendarEventMoveError { .. }
            | Self::InvitationResponded { .. }
            | Self::InvitationResponseError { .. }
            | Self::Contacts { .. }
            | Self::RecipientUsage { .. }
            | Self::Folders { .. }
            | Self::FolderCreated { .. }
            | Self::FolderRenamed { .. }
            | Self::FolderDeleted { .. }
            | Self::Tags { .. }
            | Self::TagCreated { .. }
            | Self::TagRenamed { .. }
            | Self::TagDeleted { .. }
            | Self::TagColorSet { .. }
            | Self::TagApplied { .. }
            | Self::TagApplyError { .. }
            | Self::TagListing { .. }
            | Self::MailSent { .. }
            | Self::SentCopyResolved { .. }
            | Self::MessageActionNoted { .. }
            | Self::MailSendError { .. }
            | Self::DraftSaved { .. }
            | Self::DraftSaveError { .. }
            | Self::InlineImageFetched { .. }
            | Self::InlineImageFetchError { .. }
            | Self::Status { .. }
            | Self::Error { .. } => false,
        }
    }

    /// Whether the virtualized message list has to be laid out again.
    /// Exhaustive on purpose — a missing entry leaves stale rows on screen.
    pub(crate) fn invalidates_message_list(&self) -> bool {
        match self {
            Self::ConversationTotals { .. }
            | Self::Messages { .. }
            | Self::CachedMessages { .. }
            | Self::MoreMessages { .. }
            | Self::UnifiedMessages { .. }
            | Self::UnifiedCachedMessages { .. }
            | Self::UnifiedMoreMessages { .. }
            | Self::NewMessages { .. }
            | Self::MessageChanges { .. }
            | Self::MessageOpened { .. }
            | Self::CachedMessageOpened { .. }
            | Self::SearchResults { .. }
            | Self::MessageMoved { .. }
            | Self::Tags { .. }
            | Self::TagCreated { .. }
            | Self::TagRenamed { .. }
            | Self::TagDeleted { .. }
            | Self::TagApplied { .. }
            | Self::MessageDeleted { .. }
            | Self::MessageActionNoted { .. }
            | Self::LoggedOut { .. } => true,
            Self::LanguageToolStatus { .. }
            | Self::LanguageToolChecked { .. }
            | Self::LanguageToolCheckFailed { .. }
            | Self::AiMailEditChunk { .. }
            | Self::AiMailEditFinished { .. }
            | Self::AiMailEditError { .. }
            | Self::DeviceCode { .. }
            | Self::GoogleAuthOpening { .. }
            | Self::Authenticated { .. }
            | Self::AccountReady { .. }
            | Self::AccountRestoreFailed { .. }
            | Self::QuickActionMessageLoaded { .. }
            | Self::QuickActionMessageError { .. }
            | Self::AttachmentFetched { .. }
            | Self::AttachmentFetchError { .. }
            | Self::SyncStateChanged { .. }
            | Self::MutationDeferred { .. }
            | Self::MutationSucceeded { .. }
            | Self::MutationFailed { .. }
            | Self::OutboxQueued { .. }
            | Self::QuickActionCompleted { .. }
            | Self::QuickActionStarted { .. }
            | Self::QuickActionCancelled { .. }
            | Self::QuickActionFailed { .. }
            | Self::QuickActionSendUncertain { .. }
            | Self::QuickActionMessageState { .. }
            | Self::MailCacheStats { .. }
            | Self::MailCacheCleared { .. }
            | Self::ThreadMessageLoaded { .. }
            | Self::ThreadMessageError { .. }
            | Self::Thread { .. }
            | Self::CalendarEvents { .. }
            | Self::IcalEvents { .. }
            | Self::IcalSyncState { .. }
            | Self::IcalFeedUpdated { .. }
            | Self::EventCreated { .. }
            | Self::EventCreateError { .. }
            | Self::CalendarEventUpdated { .. }
            | Self::CalendarEventUpdateError { .. }
            | Self::CalendarEventDeleted { .. }
            | Self::CalendarEventDeleteError { .. }
            | Self::CalendarEventMoved { .. }
            | Self::CalendarEventMoveError { .. }
            | Self::InvitationResponded { .. }
            | Self::InvitationResponseError { .. }
            | Self::SenderHistory { .. }
            | Self::SenderHistoryMore { .. }
            | Self::SenderHistoryError { .. }
            | Self::Contacts { .. }
            | Self::RecipientUsage { .. }
            | Self::Folders { .. }
            | Self::FolderCreated { .. }
            | Self::FolderRenamed { .. }
            | Self::FolderDeleted { .. }
            | Self::TagColorSet { .. }
            | Self::TagApplyError { .. }
            | Self::TagListing { .. }
            | Self::MailSent { .. }
            | Self::SentCopyResolved { .. }
            | Self::MailSendError { .. }
            | Self::DraftSaved { .. }
            | Self::DraftSaveError { .. }
            | Self::InlineImageFetched { .. }
            | Self::InlineImageFetchError { .. }
            | Self::Status { .. }
            | Self::Error { .. } => false,
        }
    }

    /// Whether the event applies whatever the selected account is. Anything
    /// else is filtered against the active account outside unified mode, so a
    /// misclassified event is dropped rather than misrendered — hence the
    /// exhaustive match.
    pub(crate) fn is_lifecycle(&self) -> bool {
        match self {
            Self::LanguageToolStatus { .. }
            | Self::LanguageToolChecked { .. }
            | Self::LanguageToolCheckFailed { .. }
            | Self::AiMailEditChunk { .. }
            | Self::AiMailEditFinished { .. }
            | Self::AiMailEditError { .. }
            | Self::DeviceCode { .. }
            | Self::GoogleAuthOpening { .. }
            | Self::Authenticated { .. }
            | Self::AccountReady { .. }
            | Self::AccountRestoreFailed { .. }
            | Self::QuickActionMessageLoaded { .. }
            | Self::QuickActionMessageError { .. }
            | Self::AttachmentFetched { .. }
            | Self::AttachmentFetchError { .. }
            | Self::SyncStateChanged { .. }
            | Self::MutationDeferred { .. }
            | Self::MutationSucceeded { .. }
            | Self::MutationFailed { .. }
            | Self::OutboxQueued { .. }
            | Self::QuickActionCompleted { .. }
            | Self::QuickActionStarted { .. }
            | Self::QuickActionCancelled { .. }
            | Self::QuickActionFailed { .. }
            | Self::QuickActionSendUncertain { .. }
            | Self::QuickActionMessageState { .. }
            | Self::MailCacheStats { .. }
            | Self::MailCacheCleared { .. }
            | Self::EventCreated { .. }
            | Self::EventCreateError { .. }
            | Self::CalendarEventUpdated { .. }
            | Self::CalendarEventUpdateError { .. }
            | Self::CalendarEventDeleted { .. }
            | Self::CalendarEventDeleteError { .. }
            | Self::CalendarEventMoved { .. }
            | Self::CalendarEventMoveError { .. }
            | Self::InvitationResponded { .. }
            | Self::InvitationResponseError { .. }
            | Self::RecipientUsage { .. }
            | Self::Folders { .. }
            | Self::FolderCreated { .. }
            | Self::FolderRenamed { .. }
            | Self::FolderDeleted { .. }
            | Self::MailSent { .. }
            | Self::SentCopyResolved { .. }
            | Self::MailSendError { .. }
            | Self::DraftSaved { .. }
            | Self::DraftSaveError { .. }
            | Self::LoggedOut { .. }
            | Self::Status { .. }
            | Self::Error { .. } => true,
            Self::Messages { .. }
            | Self::CachedMessages { .. }
            | Self::ConversationTotals { .. }
            | Self::MoreMessages { .. }
            | Self::UnifiedMessages { .. }
            | Self::UnifiedCachedMessages { .. }
            | Self::UnifiedMoreMessages { .. }
            | Self::NewMessages { .. }
            | Self::MessageChanges { .. }
            | Self::MessageOpened { .. }
            | Self::CachedMessageOpened { .. }
            | Self::ThreadMessageLoaded { .. }
            | Self::ThreadMessageError { .. }
            | Self::Thread { .. }
            | Self::SearchResults { .. }
            | Self::CalendarEvents { .. }
            | Self::IcalEvents { .. }
            | Self::IcalSyncState { .. }
            | Self::IcalFeedUpdated { .. }
            | Self::SenderHistory { .. }
            | Self::SenderHistoryMore { .. }
            | Self::SenderHistoryError { .. }
            | Self::Contacts { .. }
            | Self::MessageMoved { .. }
            | Self::Tags { .. }
            | Self::TagCreated { .. }
            | Self::TagRenamed { .. }
            | Self::TagDeleted { .. }
            | Self::TagColorSet { .. }
            | Self::TagApplied { .. }
            | Self::TagApplyError { .. }
            | Self::TagListing { .. }
            | Self::MessageDeleted { .. }
            | Self::MessageActionNoted { .. }
            | Self::InlineImageFetched { .. }
            | Self::InlineImageFetchError { .. } => false,
        }
    }

    /// Account to which the event applies, when it targets one. Keeping this
    /// classification with the protocol prevents views from duplicating the
    /// variant list whenever a new event type is added.
    pub(crate) fn account_id(&self) -> Option<&AccountId> {
        match self {
            Self::AccountReady(account) => Some(&account.id),
            Self::AccountRestoreFailed { account_id, .. }
            | Self::Messages { account_id, .. }
            | Self::CachedMessages { account_id, .. }
            | Self::ConversationTotals { account_id, .. }
            | Self::MoreMessages { account_id, .. }
            | Self::NewMessages { account_id, .. }
            | Self::MessageChanges { account_id, .. }
            | Self::MessageOpened { account_id, .. }
            | Self::CachedMessageOpened { account_id, .. }
            | Self::QuickActionMessageLoaded { account_id, .. }
            | Self::QuickActionMessageError { account_id, .. }
            | Self::AttachmentFetched { account_id, .. }
            | Self::AttachmentFetchError { account_id, .. }
            | Self::SyncStateChanged { account_id, .. }
            | Self::MutationDeferred { account_id, .. }
            | Self::MutationSucceeded { account_id, .. }
            | Self::MutationFailed { account_id, .. }
            | Self::OutboxQueued { account_id, .. }
            | Self::QuickActionCompleted { account_id, .. }
            | Self::QuickActionStarted { account_id, .. }
            | Self::QuickActionCancelled { account_id, .. }
            | Self::QuickActionFailed { account_id, .. }
            | Self::QuickActionSendUncertain { account_id, .. }
            | Self::QuickActionMessageState { account_id, .. }
            | Self::ThreadMessageLoaded { account_id, .. }
            | Self::ThreadMessageError { account_id, .. }
            | Self::Thread { account_id, .. }
            | Self::SearchResults { account_id, .. }
            | Self::CalendarEvents { account_id, .. }
            | Self::EventCreated { account_id, .. }
            | Self::EventCreateError { account_id, .. }
            | Self::CalendarEventUpdated { account_id, .. }
            | Self::CalendarEventUpdateError { account_id, .. }
            | Self::CalendarEventDeleted { account_id, .. }
            | Self::CalendarEventDeleteError { account_id, .. }
            | Self::CalendarEventMoved { account_id, .. }
            | Self::CalendarEventMoveError { account_id, .. }
            | Self::InvitationResponded { account_id, .. }
            | Self::InvitationResponseError { account_id, .. }
            | Self::SenderHistory { account_id, .. }
            | Self::SenderHistoryMore { account_id, .. }
            | Self::SenderHistoryError { account_id, .. }
            | Self::Contacts { account_id, .. }
            | Self::Folders { account_id, .. }
            | Self::FolderCreated { account_id, .. }
            | Self::FolderRenamed { account_id, .. }
            | Self::FolderDeleted { account_id, .. }
            | Self::MessageMoved { account_id, .. }
            | Self::Tags { account_id, .. }
            | Self::TagCreated { account_id, .. }
            | Self::TagRenamed { account_id, .. }
            | Self::TagDeleted { account_id, .. }
            | Self::TagColorSet { account_id, .. }
            | Self::TagApplied { account_id, .. }
            | Self::TagApplyError { account_id, .. }
            | Self::TagListing { account_id, .. }
            | Self::MessageDeleted { account_id, .. }
            | Self::MessageActionNoted { account_id, .. }
            | Self::MailSent { account_id, .. }
            | Self::SentCopyResolved { account_id, .. }
            | Self::MailSendError { account_id, .. }
            | Self::DraftSaved { account_id, .. }
            | Self::DraftSaveError { account_id, .. }
            | Self::LoggedOut { account_id } => Some(account_id),
            Self::DeviceCode { .. }
            | Self::LanguageToolStatus(_)
            | Self::LanguageToolChecked { .. }
            | Self::LanguageToolCheckFailed { .. }
            | Self::AiMailEditChunk { .. }
            | Self::AiMailEditFinished { .. }
            | Self::AiMailEditError { .. }
            | Self::GoogleAuthOpening { .. }
            | Self::Authenticated
            | Self::UnifiedMessages { .. }
            | Self::UnifiedCachedMessages { .. }
            | Self::UnifiedMoreMessages { .. }
            | Self::InlineImageFetched { .. }
            | Self::InlineImageFetchError { .. }
            | Self::RecipientUsage { .. }
            | Self::IcalEvents { .. }
            | Self::IcalSyncState { .. }
            | Self::IcalFeedUpdated { .. }
            | Self::MailCacheStats { .. }
            | Self::MailCacheCleared
            | Self::Status(_)
            | Self::Error(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three classifiers used to be `matches!` lists inlined in
    /// `ui::events::handle_event`, where an unlisted event silently fell into
    /// the default branch. They are exhaustive matches now; these cases pin the
    /// behaviour the UI depends on.
    #[test]
    fn event_classification_matches_what_the_views_expect() {
        let messages = Evt::Messages {
            account_id: AccountId("account@example.test".into()),
            messages: Vec::new(),
        };
        assert!(messages.invalidates_message_list());
        assert!(messages.prunes_message_row_hover());
        assert!(!messages.is_lifecycle());

        let failure = Evt::Error("synthetic".into());
        assert!(failure.is_lifecycle());
        assert!(!failure.invalidates_message_list());

        // Reaches the compose that issued it, whichever account is selected.
        let sent = Evt::MailSendError {
            account_id: AccountId("account@example.test".into()),
            compose_id: 1,
            error: "synthetic".into(),
        };
        assert!(sent.is_lifecycle());
    }
}
