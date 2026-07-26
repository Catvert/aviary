use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identity used by the UI session. Message bodies and binary payloads
/// live exclusively in the SQLite mail cache and are loaded from this pair.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRef {
    pub account_id: AccountId,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Provider {
    #[default]
    Microsoft,
    Google,
    Imap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub email: String,
    pub display_name: String,
    pub tenant: String,
    pub provider: Provider,
}

/// User's latest action on a received message, Outlook-style (for example,
/// "You forwarded this message on ..."). Populated by:
/// - **Microsoft Graph**: the MAPI `PidTagLastVerbExecuted` property
///   (`Integer 0x1081`: 102 = reply, 103 = reply all,
///   104 = forward), dated by `PidTagLastVerbExecutionTime`
///   (`SystemTime 0x1082`).
/// - **IMAP**: the `\Answered` flag and `$Forwarded` keyword (without a date).
/// - **Gmail**: unsupported because the API does not expose these actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LastAction {
    Replied,
    RepliedAll,
    Forwarded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageHeader {
    pub id: String,
    /// Provider helpers leave this as `AccountId::default()`; the runtime tags
    /// it on every event so the UI can route open/delete/flag/reply requests
    /// back to the right account in unified-inbox mode.
    pub account_id: AccountId,
    pub subject: String,
    pub from: String,
    pub received: DateTime<Utc>,
    pub preview: String,
    pub is_read: bool,
    pub is_flagged: bool,
    /// True if the message has any non-inline attachments. Surfaced from the
    /// listing endpoint (Graph: `hasAttachments`; Gmail: derived from the
    /// payload parts; IMAP: derived from BODYSTRUCTURE) so the inbox row can
    /// show a paperclip icon without fetching the full body.
    pub has_attachments: bool,
    /// Tag identifiers applied to this message, in the same per-provider
    /// format as `Message::tags` (Graph: category display name, Gmail: label
    /// id, IMAP: keyword). Surfaced from the listing endpoint so the inbox
    /// row can render pill chips alongside the subject without a per-row
    /// metadata fetch.
    pub tags: Vec<String>,
    /// Latest reply/forward action performed on this message (see
    /// [`LastAction`]). `None` = aucune action connue — soit il n'y en a
    /// not happen, or the source did not carry the information (Graph delta, Gmail).
    pub last_action: Option<LastAction>,
    /// Timestamp for `last_action` when provided by the provider (Graph
    /// only; IMAP has no date associated with its flags).
    pub last_action_at: Option<DateTime<Utc>>,
    /// Thread this message belongs to, in the provider's own namespace
    /// (Graph `conversationId`, Gmail `threadId`, IMAP: the root
    /// `Message-ID` derived from `References`/`In-Reply-To`). Identifiers are
    /// only comparable **within one account**, so every consumer keys on
    /// `(account_id, conversation_id)`.
    ///
    /// This lives on the header, not on `Message`, because the message list
    /// never loads bodies: grouping a folder by conversation has to work from
    /// listing data alone.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// RFC 5322 `Message-ID`, brackets stripped.
    ///
    /// It identifies the *mail*, where `id` identifies one mailbox copy of
    /// it. A message you send exists twice — in Sent Items and wherever it
    /// was delivered — with two provider ids but one `Message-ID`, and a
    /// thread query that spans folders returns both. This is what lets the
    /// reader tell "the same mail again" from "another reply".
    ///
    /// `None` when the backend does not surface it; consumers must fall back
    /// to `id` rather than treating two `None`s as equal.
    #[serde(default)]
    pub internet_message_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyFormat {
    Markdown,
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineImage {
    pub cid: String,
    pub mime: String,
    #[serde(with = "b64_bytes")]
    pub bytes: Vec<u8>,
}

/// A non-inline attachment delivered alongside an incoming message, or
/// queued for an outgoing one. Inline images live in `InlineImage`
/// (rendered via `cid:` in the HTML body) — this struct is for files the
/// recipient sees as an attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Provider-specific id used to fetch the bytes lazily for received
    /// messages. For outgoing attachments the runtime sets this to an empty
    /// string and the bytes are sent inline.
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    /// `Some` once the bytes have been fetched (received) or set at attach
    /// time (outgoing). `None` for received attachments that haven't been
    /// downloaded yet.
    #[serde(with = "b64_opt_bytes")]
    pub bytes: Option<Vec<u8>>,
}

/// The attendee's current answer to a calendar invitation.
///
/// The same four states are exposed by Microsoft Graph and Google Calendar;
/// keeping them in the message model lets cached invitations render without
/// contacting the provider again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InvitationResponse {
    #[default]
    NeedsAction,
    Accepted,
    Tentative,
    Declined,
}

/// Calendar request associated with an incoming email.
///
/// `event_id` is always the provider-native calendar event id, not the mail
/// message id nor an RFC 5545 UID. Invitations for which the provider does not
/// expose a writable event are intentionally left as ordinary attachments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarInvitation {
    pub event_id: String,
    pub subject: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub location: String,
    pub organizer: String,
    pub response: InvitationResponse,
}

/// JSON serialization of binary payloads as base64 strings. serde_json's
/// derived form for `Vec<u8>` is an integer array (~4 characters per byte,
/// and one *line* per byte when pretty-printed), which is what let
/// settings.json grow to hundreds of MB.
pub mod b64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(de)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// `Option<Vec<u8>>` variant of [`b64_bytes`].
pub mod b64_opt_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, ser: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => ser.serialize_some(&STANDARD.encode(bytes)),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Vec<u8>>, D::Error> {
        match Option::<String>::deserialize(de)? {
            Some(text) => STANDARD
                .decode(text)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub header: MessageHeader,
    pub body: String,
    pub format: BodyFormat,
    pub inline_images: Vec<InlineImage>,
    /// Files attached to the message but not embedded in the body. Same
    /// shape for received and outgoing — for received messages the bytes
    /// arrive eagerly with the body fetch.
    pub attachments: Vec<Attachment>,
    /// Tag identifiers currently applied to this message. Format mirrors
    /// `Cmd::AddTag::tag_id` per provider — Outlook category *display name*
    /// for Graph (that's what messages reference), Gmail label id, IMAP
    /// keyword string. Used by the viewer to render which tags are set and
    /// drive the toggle UI.
    pub tags: Vec<String>,
    /// Original body before our HTML→Markdown conversion. Populated for
    /// `BodyFormat::Markdown` messages (where `body` is the converted
    /// markdown), so the viewer's "Source" toggle can show the raw HTML the
    /// server actually sent. `None` for plain-text messages, where `body`
    /// is already the source.
    pub raw_body: Option<String>,
    /// To recipients, as raw "Display <addr@host>" strings. Populated for
    /// every fetched message so the compose can pre-fill addresses when
    /// editing a draft. The viewer doesn't render these (it shows `from`).
    pub to: Vec<String>,
    /// Cc recipients, same shape as `to`.
    pub cc: Vec<String>,
    /// Bcc recipients. Normally populated only for drafts/sent messages,
    /// since received messages do not expose other blind recipients.
    pub bcc: Vec<String>,
    /// Set when the fetched message is a draft. Carries the provider-native
    /// id used by `save_draft(replace_id)` and `delete_draft`:
    /// - Graph: same as `header.id` (drafts are regular messages with `isDraft`).
    /// - Gmail: the `draft.id` (distinct from `message.id`), looked up via
    ///   `drafts.list` when the DRAFT label is present.
    /// - IMAP: same as `header.id` (the `<folder>:<uid>` encoding); replacing
    ///   means APPEND new + delete old.
    ///
    /// `None` for non-drafts. The compose UI auto-opens drafts in edit mode.
    pub draft_id: Option<String>,
    /// Actionable meeting request represented by this email. This is absent
    /// for cancellations, organizer responses and providers that cannot map
    /// the mail to a writable calendar event.
    #[serde(default)]
    pub invitation: Option<CalendarInvitation>,
}

impl From<&Message> for MessageRef {
    fn from(message: &Message) -> Self {
        Self {
            account_id: message.header.account_id.clone(),
            id: message.header.id.clone(),
        }
    }
}

/// Local snapshot of a reply or forward after the provider accepted it.
///
/// Some send APIs do not return the provider id of the created Sent item
/// (notably SMTP). Keeping the exact outgoing payload lets the reader still
/// expose the new message above its source, including its recipients, body,
/// inline images, and files. When the provider hands back enough identifiers
/// ([`sent_id`](Self::sent_id) /
/// [`internet_message_id`](Self::internet_message_id)), the runtime later
/// swaps `message` for the real Sent-items copy (`Evt::SentCopyResolved`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentMessage {
    /// Provider id of the original message that was replied to or forwarded.
    pub related_to: String,
    pub action: LastAction,
    pub message: Box<Message>,
    /// Provider id of the Sent-items copy, when known. Gmail returns it on
    /// send; for Graph it is filled once the copy has been resolved through
    /// `internet_message_id`.
    #[serde(default)]
    pub sent_id: Option<String>,
    /// RFC 5322 `Message-ID` of the outgoing mail, used to look up the
    /// Sent-items copy when the send API returns no provider id (Graph).
    #[serde(default)]
    pub internet_message_id: Option<String>,
}

impl SentMessage {
    /// True while `message` is still the locally rebuilt snapshot rather
    /// than the provider's Sent-items copy.
    pub fn is_snapshot(&self) -> bool {
        self.message.header.id.starts_with("aviary-sent-")
    }

    /// Snapshot that carries enough identifiers to fetch the real copy.
    pub fn needs_resolution(&self) -> bool {
        self.is_snapshot() && (self.sent_id.is_some() || self.internet_message_id.is_some())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub email: String,
    pub score: f32,
}

/// User-defined classification applied to a message. Maps onto the
/// provider's native concept:
/// - **Microsoft Graph**: Outlook *categories* — referenced by `display_name`
///   on the message (`message.categories: string[]`). The master list lives
///   under `/me/outlook/masterCategories` with a name + a preset color id.
/// - **Gmail**: user labels (excluding system labels like INBOX/SENT/…). A
///   single label can act as both a "folder" and a tag because Gmail's
///   storage model is flat.
/// - **IMAP**: per-message custom flags (RFC 3501 keywords, e.g. `$Work`).
///   Limited to a 7-bit ASCII identifier; no display name customisation, no
///   color. Persisted only when the server advertises `\*` in
///   `PERMANENTFLAGS`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    /// Provider-specific identifier used by add/remove operations. For Graph
    /// this is the master category id; for Gmail the label id; for IMAP the
    /// keyword string.
    pub id: String,
    /// User-visible name. May equal `id` (IMAP keywords).
    pub display_name: String,
    /// Packed sRGB (`0xRRGGBB`). `None` when the provider does not expose a
    /// color (Gmail, IMAP) — the UI derives one from the name hash.
    pub color: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailFolder {
    pub id: String,
    pub display_name: String,
    /// Provider-native id of the parent folder. `None` means that the folder
    /// is a root of the navigation tree.
    pub parent_id: Option<String>,
    pub well_known_name: Option<String>,
    pub total_item_count: u32,
    pub unread_item_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    pub is_default: bool,
    pub position: i64,
    pub blocks: Vec<crate::blocks::Block>,
    /// Bytes for any Image blocks the signature contains. Persisted alongside
    /// the blocks so logos survive a restart.
    pub images: Vec<InlineImage>,
}

/// Reusable email fragment, selectable from the composer or inserted
/// automatically when it is the mailbox's default template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    pub is_default: bool,
    pub position: i64,
    pub blocks: Vec<crate::blocks::Block>,
    /// Bytes for any Image blocks the template contains.
    pub images: Vec<InlineImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    /// Native account id for writable provider events, or `ical:<id>` for a
    /// read-only subscribed feed. The prefix makes event/source keys globally
    /// unique without pretending an iCal feed is an authenticated account.
    pub account_id: AccountId,
    #[serde(default)]
    pub read_only: bool,
    pub subject: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub location: String,
    pub organizer: String,
    pub preview: String,
    pub is_cancelled: bool,
    pub online_meeting_url: Option<String>,
    pub web_link: Option<String>,
}

/// Refresh cadence for a subscribed iCalendar feed. The explicit enum keeps
/// persisted values bounded and makes the settings UI deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IcalRefreshInterval {
    Manual,
    FifteenMinutes,
    ThirtyMinutes,
    #[default]
    OneHour,
    SixHours,
    OneDay,
}

impl IcalRefreshInterval {
    pub const ALL: &'static [Self] = &[
        Self::Manual,
        Self::FifteenMinutes,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::SixHours,
        Self::OneDay,
    ];

    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Manual => None,
            Self::FifteenMinutes => Some(15 * 60),
            Self::ThirtyMinutes => Some(30 * 60),
            Self::OneHour => Some(60 * 60),
            Self::SixHours => Some(6 * 60 * 60),
            Self::OneDay => Some(24 * 60 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcalSubscription {
    pub id: String,
    pub name: String,
    /// HTTPS URL. `webcal://` input is normalized before persistence.
    pub url: String,
    /// Packed `0xRRGGBB`, matching account color overrides.
    pub color: u32,
    pub refresh: IcalRefreshInterval,
}

impl IcalSubscription {
    pub fn account_id(&self) -> AccountId {
        AccountId(format!("ical:{}", self.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base64 form divides file sizes (mail cache, session) by ~4.
    #[test]
    fn binary_payloads_serialize_as_base64() {
        let image = InlineImage {
            cid: "c1".into(),
            mime: "image/png".into(),
            bytes: vec![1, 2, 255],
        };
        let json = serde_json::to_string(&image).expect("serialization");
        assert!(json.contains("\"AQL/\""), "expected base64 payload: {json}");
        let back: InlineImage = serde_json::from_str(&json).expect("deserialization");
        assert_eq!(back, image);

        let attachment = Attachment {
            id: String::new(),
            filename: "a.bin".into(),
            mime: "application/octet-stream".into(),
            size: 3,
            bytes: Some(vec![1, 2, 255]),
        };
        let json = serde_json::to_string(&attachment).expect("serialization");
        assert!(json.contains("\"AQL/\""), "expected base64 payload: {json}");
        let back: Attachment = serde_json::from_str(&json).expect("deserialization");
        assert_eq!(back.bytes, Some(vec![1, 2, 255]));

        let none = r#"{"id":"","filename":"a.bin","mime":"application/octet-stream","size":3,"bytes":null}"#;
        let back: Attachment = serde_json::from_str(none).expect("null deserialization");
        assert_eq!(back.bytes, None);
    }
}
