//! Provider dispatch. Each runtime task builds a [`Session`] — the pairing
//! of a backend with its live credentials — and calls the unified API
//! surface on it; the session routes to the Microsoft Graph, Gmail, or
//! IMAP/SMTP backend.
//!
//! This keeps the runtime layer free of `match account.provider { … }`
//! noise, and makes a provider/credentials mismatch unrepresentable: a
//! `Session::Graph` *is* a client + bearer token (+ tenant), a
//! `Session::Imap` *is* host/credentials. Construction happens in one
//! place per flow (`BgAccount::session`, or inline during login before an
//! account exists).

use crate::auth::{ImapConfig, NetSecurity};
use crate::model::{
    Account, CalendarEvent, Contact, InvitationResponse, MailFolder, Message, MessageHeader, Tag,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment, Message as LettreMessage, MultiPart, SinglePart};

pub mod error;
pub mod gmail;
pub mod graph;
pub mod html;
pub mod imap;

pub(crate) use error::{http_error, status_of};

/// One page of incremental folder changes. Cursors remain opaque to the
/// runtime and are persisted unchanged in SQLite.
#[derive(Default)]
pub struct MailSyncPage {
    pub upserts: Vec<MessageHeader>,
    pub deleted: Vec<String>,
    pub removed_from_folder: Vec<String>,
    pub next: Option<String>,
    pub cursor: Option<String>,
}

/// Chronological page for a folder. `next` is an opaque cursor whose format
/// is known only by the backend that produced it.
pub struct MessagePage {
    pub messages: Vec<MessageHeader>,
    pub next: Option<String>,
}

/// Online-meeting kind a user can attach when creating a calendar event.
/// Mapped to `teamsForBusiness` on Graph and to a Google Meet
/// `conferenceData.createRequest` on Google Calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineMeetingKind {
    Teams,
    GoogleMeet,
}

/// Draft event for `create_event`. UTC instants — the UI converts the
/// user's local-time inputs before sending. `all_day` events ignore the
/// time-of-day component (each backend re-derives the date from `start`).
pub struct NewCalendarEvent<'a> {
    pub subject: &'a str,
    pub description: &'a str,
    pub location: &'a str,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub online_meeting: Option<OnlineMeetingKind>,
    /// Email addresses to invite. Each backend triggers its native
    /// invitation flow: Graph emails the attendees automatically, Gmail
    /// requires `sendUpdates=all` on the create call.
    pub attendees: &'a [String],
}

/// Provider-agnostic outgoing message. Each backend serialises this into
/// its own wire format (Graph JSON, RFC822-over-base64url for Gmail, raw
/// MIME for SMTP).
pub struct OutgoingMessage<'a> {
    pub from: &'a str,
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub body_is_html: bool,
    /// Inline images embedded in the body via `cid:`.
    pub attachments: &'a [crate::model::InlineImage],
    /// Non-inline file attachments — the recipient sees these as
    /// downloadable attachments. Bytes are required (`bytes: Some(_)`)
    /// for outgoing messages; runtime fetches them when the user picks a
    /// file or forwards an existing attachment.
    pub files: &'a [crate::model::Attachment],
}

/// Move target meaning "archive this message", whatever the backend's own
/// notion of archiving is. Every backend resolves it itself: Graph treats it
/// as the well-known `archive` folder, Gmail translates it into dropping
/// `INBOX` (it has no Archive label), and IMAP looks up the mailbox flagged
/// `\Archive`, failing with a translated error when the server has none.
///
/// The UI passes this alias rather than a resolved folder id so a single
/// call site works across the three providers.
pub const ARCHIVE_FOLDER_ALIAS: &str = "archive";

/// Move target meaning "this is junk". Graph knows `junkemail` as a well-known
/// folder, Gmail translates it into its `SPAM` label, and IMAP resolves the
/// mailbox flagged `\Junk` — failing with a translated error when the server
/// has none, since unlike archiving there is no sensible fallback.
///
/// Same contract as [`ARCHIVE_FOLDER_ALIAS`]: the UI passes the account's own
/// junk folder id when it knows it, this alias otherwise.
pub const JUNK_FOLDER_ALIAS: &str = "junkemail";

/// Move target the "not junk" action restores a message to. Every backend
/// already resolves it (`inbox` well-known for Graph, `INBOX` label for Gmail,
/// the `INBOX` mailbox for IMAP), so it needs no per-provider handling — it is
/// named here only so the intent reads at the call site.
pub const INBOX_FOLDER_ALIAS: &str = "inbox";

/// Colors a tag may take on the given provider, as packed sRGB
/// (`0xRRGGBB`). Outlook categories only accept their 25 presets; Gmail
/// labels only accept colors from a documented allowed list (curated subset
/// here). Empty for IMAP — keywords carry no color, pickers should hide.
pub fn tag_color_palette(provider: crate::model::Provider) -> &'static [u32] {
    match provider {
        crate::model::Provider::Microsoft => &graph::PRESET_PALETTE,
        crate::model::Provider::Google => &gmail::LABEL_PALETTE,
        crate::model::Provider::Imap => &[],
    }
}

/// Provider-side effects of renaming a tag.
///
/// Gmail keeps both its label id and the ids stored on messages. Microsoft
/// Graph cannot update an Outlook category's display name, so its backend
/// replaces the master category and rewrites the display-name strings stored
/// on messages.
#[derive(Debug, Default)]
pub struct TagRename {
    pub new_id: Option<String>,
    pub message_tag_rename: Option<(String, String)>,
}

/// Identifiers of the Sent-items copy, returned by the send APIs when the
/// backend provides them. Either field may be `None`; both `None` (SMTP)
/// means the copy cannot be recovered later and the local snapshot stays.
#[derive(Debug, Default, Clone)]
pub struct SentIds {
    /// Provider-native id of the sent message. Gmail returns it directly
    /// from `messages.send`. Graph does not (the draft id changes when the
    /// message moves to Sent Items), so it stays `None` there.
    pub message_id: Option<String>,
    /// RFC 5322 `Message-ID` header of the outgoing mail. Stable across
    /// folder moves, so it can be used to find the Sent-items copy later
    /// (Graph `$filter=internetMessageId eq …`).
    pub internet_message_id: Option<String>,
}

/// Builds the canonical RFC822/MIME representation shared by Gmail raw
/// messages and IMAP/SMTP. `keep_bcc` is true for Gmail because its raw API
/// derives recipients from headers; SMTP derives them from Lettre's envelope
/// and must omit the private header on the wire.
pub(crate) fn build_rfc822(
    msg: &OutgoingMessage<'_>,
    extra_headers: &[(&str, String)],
    keep_bcc: bool,
) -> Result<LettreMessage> {
    let from = msg.from.parse().with_context(|| {
        tr!("mail-error-invalid-from", {
            address: msg.from
        })
    })?;
    let mut builder = LettreMessage::builder().from(from).subject(msg.subject);
    for to in msg.to {
        builder = builder.to(to.parse().with_context(|| {
            tr!("mail-error-invalid-recipient", {
                address: to
            })
        })?);
    }
    for cc in msg.cc {
        builder = builder.cc(cc.parse().with_context(|| {
            tr!("mail-error-invalid-recipient", {
                address: cc
            })
        })?);
    }
    for bcc in msg.bcc {
        builder = builder.bcc(bcc.parse().with_context(|| {
            tr!("mail-error-invalid-recipient", {
                address: bcc
            })
        })?);
    }
    if keep_bcc {
        builder = builder.keep_bcc();
    }
    for (name, value) in extra_headers {
        let name = HeaderName::new_from_ascii((*name).to_string())
            .context(tr!("mail-error-invalid-header"))?;
        builder = builder.raw_header(HeaderValue::new(name, value.clone()));
    }

    let has_inline = !msg.attachments.is_empty();
    let has_files = !msg.files.is_empty();
    let body_bundle = if has_inline {
        let body = if msg.body_is_html {
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(msg.body.to_string())
        } else {
            SinglePart::plain(msg.body.to_string())
        };
        let mut related = MultiPart::related().singlepart(body);
        for image in msg.attachments {
            let mime: ContentType = image
                .mime
                .parse()
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            related = related.singlepart(
                Attachment::new_inline(image.cid.clone()).body(image.bytes.clone(), mime),
            );
        }
        related
    } else if msg.body_is_html {
        MultiPart::alternative_plain_html(strip_html(msg.body), msg.body.to_string())
    } else if !has_files {
        return builder
            .body(msg.body.to_string())
            .context(tr!("mail-error-build-message"));
    } else {
        MultiPart::mixed().singlepart(SinglePart::plain(msg.body.to_string()))
    };

    if !has_files {
        return builder
            .multipart(body_bundle)
            .context(tr!("mail-error-build-multipart"));
    }

    let mut mixed = MultiPart::mixed().multipart(body_bundle);
    for file in msg.files {
        let mime: ContentType = file
            .mime
            .parse()
            .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
        mixed = mixed.singlepart(
            Attachment::new(file.filename.clone())
                .body(file.bytes.clone().unwrap_or_default(), mime),
        );
    }
    builder
        .multipart(mixed)
        .context(tr!("mail-error-build-multipart-mixed"))
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out
}

/// Borrowed snapshot of an IMAP+SMTP account's runtime credentials. The
/// runtime constructs this on every call by merging `ImapConfig` with the
/// password cached by the runtime after loading it from the keyring.
pub struct ImapAuth<'a> {
    pub email: &'a str,
    pub display_name: &'a str,

    pub imap_host: &'a str,
    pub imap_port: u16,
    pub imap_security: NetSecurity,
    pub imap_username: &'a str,

    pub smtp_host: &'a str,
    pub smtp_port: u16,
    pub smtp_security: NetSecurity,
    pub smtp_username: &'a str,

    pub password: &'a str,
}

impl<'a> ImapAuth<'a> {
    /// Build a borrowed view from a stored `ImapConfig` plus its cached
    /// keyring password.
    pub fn from_config(cfg: &'a ImapConfig, password: &'a str) -> Self {
        Self {
            email: &cfg.email,
            display_name: &cfg.display_name,
            imap_host: &cfg.imap_host,
            imap_port: cfg.imap_port,
            imap_security: cfg.imap_security,
            imap_username: &cfg.imap_username,
            smtp_host: &cfg.smtp_host,
            smtp_port: cfg.smtp_port,
            smtp_security: cfg.smtp_security,
            smtp_username: &cfg.smtp_username,
            password,
        }
    }
}

/// One backend + the live credentials needed to talk to it, borrowed for
/// the duration of a single runtime task. Every provider capability is a
/// method on this enum — the only `match provider` in the codebase.
pub enum Session<'a> {
    Graph {
        client: graph::Client<'a>,
        /// Azure AD tenant the account was issued against (used by `/me`).
        tenant: &'a str,
    },
    Gmail {
        client: &'a reqwest::Client,
        token: &'a str,
    },
    Imap(ImapAuth<'a>),
}

impl Session<'_> {
    pub async fn disconnect(&self) -> Result<()> {
        match self {
            Session::Imap(imap) => imap::close_session(imap).await,
            Session::Graph { .. } | Session::Gmail { .. } => Ok(()),
        }
    }

    pub async fn get_me(&self) -> Result<Account> {
        match self {
            Session::Graph { client, tenant } => graph::get_me(client, tenant).await,
            Session::Gmail { client, token } => gmail::get_me(client, token).await,
            Session::Imap(imap) => imap::get_me(imap).await,
        }
    }

    pub async fn list_folder_messages(
        &self,
        folder_id: Option<&str>,
        top: usize,
        skip: usize,
    ) -> Result<Vec<MessageHeader>> {
        match self {
            Session::Graph { client, .. } => {
                graph::list_folder_messages(client, folder_id, top, skip).await
            }
            Session::Gmail { client, token } => {
                gmail::list_folder_messages(client, token, folder_id, top, skip).await
            }
            Session::Imap(imap) => imap::list_folder_messages(imap, folder_id, top, skip).await,
        }
    }

    pub async fn list_folder_messages_page(
        &self,
        folder_id: Option<&str>,
        top: usize,
    ) -> Result<MessagePage> {
        match self {
            Session::Graph { client, .. } => {
                graph::list_folder_messages_page(client, folder_id, top).await
            }
            Session::Gmail { client, token } => {
                gmail::list_folder_messages_page(client, token, folder_id, top).await
            }
            Session::Imap(imap) => imap::list_folder_messages_page(imap, folder_id, top).await,
        }
    }

    pub async fn sync_folder_messages(
        &self,
        folder_id: Option<&str>,
        cursor_or_page: Option<&str>,
    ) -> Result<MailSyncPage> {
        match self {
            Session::Graph { client, .. } => {
                graph::sync_folder_messages(client, folder_id, cursor_or_page).await
            }
            Session::Gmail { client, token } => {
                gmail::sync_folder_messages(client, token, folder_id, cursor_or_page).await
            }
            Session::Imap(imap) => {
                imap::sync_folder_messages(imap, folder_id, cursor_or_page).await
            }
        }
    }

    pub async fn get_message(&self, id: &str) -> Result<Message> {
        let mut message = match self {
            Session::Graph { client, .. } => graph::get_message(client, id).await,
            Session::Gmail { client, token } => gmail::get_message(client, token, id).await,
            Session::Imap(imap) => imap::get_message(imap, id).await,
        }?;
        remove_embedded_image_attachments(&mut message);
        Ok(message)
    }

    /// Fetch the binary payload of one regular attachment. Attachment ids are
    /// opaque provider-native values surfaced by `get_message`.
    pub async fn fetch_attachment(&self, message_id: &str, attachment_id: &str) -> Result<Vec<u8>> {
        match self {
            Session::Graph { client, .. } => {
                graph::fetch_attachment(client, message_id, attachment_id).await
            }
            Session::Gmail { client, token } => {
                gmail::fetch_attachment(client, token, message_id, attachment_id).await
            }
            Session::Imap(imap) => imap::fetch_attachment(imap, message_id, attachment_id).await,
        }
    }

    pub async fn delete_message(&self, id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::delete_message(client, id).await,
            Session::Gmail { client, token } => gmail::delete_message(client, token, id).await,
            Session::Imap(imap) => imap::delete_message(imap, id).await,
        }
    }

    pub async fn set_flag(&self, id: &str, flagged: bool) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::set_flag(client, id, flagged).await,
            Session::Gmail { client, token } => gmail::set_flag(client, token, id, flagged).await,
            Session::Imap(imap) => imap::set_flag(imap, id, flagged).await,
        }
    }

    /// Records on the server that message `id` was just replied to or
    /// forwarded, so the status survives restarts and remains visible in
    /// other clients (Outlook reads the same MAPI property, and IMAP MUAs
    /// read the same flags). Returns `false` when the provider cannot persist
    /// this information (Gmail).
    pub async fn note_last_action(
        &self,
        id: &str,
        action: crate::model::LastAction,
    ) -> Result<bool> {
        match self {
            Session::Graph { client, .. } => graph::note_last_action(client, id, action)
                .await
                .map(|()| true),
            // L'API Gmail n'expose ni \Answered ni $Forwarded.
            Session::Gmail { .. } => Ok(false),
            Session::Imap(imap) => imap::note_last_action(imap, id, action)
                .await
                .map(|()| true),
        }
    }

    pub async fn mark_read(&self, id: &str, read: bool) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::mark_read(client, id, read).await,
            Session::Gmail { client, token } => gmail::mark_read(client, token, id, read).await,
            Session::Imap(imap) => imap::mark_read(imap, id, read).await,
        }
    }

    /// Provider-side search for an already parsed query.
    ///
    /// Each backend renders the operators into its own dialect, and each drops
    /// what it cannot express — Graph cannot combine `$search` with a date
    /// `$filter`, IMAP has no attachment predicate. Callers must therefore
    /// re-apply [`SearchQuery::matches`] to the results; the runtime does.
    /// `folder_id` restricts the search to one folder, with `None` meaning the
    /// inbox as everywhere else; `None` for the whole `Option` searches every
    /// folder.
    pub async fn search(
        &self,
        query: &crate::search_query::SearchQuery,
        folder_id: Option<Option<&str>>,
        limit: usize,
    ) -> Result<Vec<MessageHeader>> {
        match self {
            Session::Graph { client, .. } => graph::search(client, query, folder_id, limit).await,
            Session::Gmail { client, token } => {
                gmail::search(client, token, query, folder_id, limit).await
            }
            Session::Imap(imap) => imap::search(imap, query, folder_id, limit).await,
        }
    }

    pub async fn list_from_sender(
        &self,
        email: &str,
        top: usize,
    ) -> Result<(Vec<MessageHeader>, Option<String>)> {
        match self {
            Session::Graph { client, .. } => graph::list_from_sender(client, email, top).await,
            Session::Gmail { client, token } => {
                gmail::list_from_sender(client, token, email, top).await
            }
            Session::Imap(imap) => imap::list_from_sender(imap, email, top).await,
        }
    }

    pub async fn list_thread(&self, conversation_id: &str) -> Result<Vec<MessageHeader>> {
        match self {
            Session::Graph { client, .. } => graph::list_thread(client, conversation_id).await,
            Session::Gmail { client, token } => {
                gmail::list_thread(client, token, conversation_id).await
            }
            Session::Imap(imap) => imap::list_thread(imap, conversation_id).await,
        }
    }

    pub async fn fetch_messages_page(
        &self,
        url: &str,
    ) -> Result<(Vec<MessageHeader>, Option<String>)> {
        match self {
            Session::Graph { client, .. } => graph::fetch_messages_page(client, url).await,
            Session::Gmail { client, token } => {
                gmail::fetch_messages_page(client, token, url).await
            }
            Session::Imap(imap) => imap::fetch_messages_page(imap, url).await,
        }
    }

    pub async fn list_folders(&self) -> Result<Vec<MailFolder>> {
        match self {
            Session::Graph { client, .. } => graph::list_folders(client).await,
            Session::Gmail { client, token } => gmail::list_folders(client, token).await,
            Session::Imap(imap) => imap::list_folders(imap).await,
        }
    }

    pub async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>> {
        match self {
            Session::Graph { client, .. } => graph::list_events(client, from, to).await,
            Session::Gmail { client, token } => gmail::list_events(client, token, from, to).await,
            // IMAP has no calendar — return empty.
            Session::Imap(_) => Ok(Vec::new()),
        }
    }

    pub async fn create_event(&self, event: &NewCalendarEvent<'_>) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::create_event(client, event).await,
            Session::Gmail { client, token } => gmail::create_event(client, token, event).await,
            Session::Imap(_) => bail!("{}", crate::tr!("calendar-error-imap-unsupported")),
        }
    }

    pub async fn respond_to_invitation(
        &self,
        event_id: &str,
        response: InvitationResponse,
    ) -> Result<()> {
        match self {
            Session::Graph { client, .. } => {
                graph::respond_to_invitation(client, event_id, response).await
            }
            Session::Gmail { client, token } => {
                gmail::respond_to_invitation(client, token, event_id, response).await
            }
            Session::Imap(_) => bail!("{}", crate::tr!("invitation-error-imap-unsupported")),
        }
    }

    pub async fn update_event(&self, event_id: &str, event: &NewCalendarEvent<'_>) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::update_event(client, event_id, event).await,
            Session::Gmail { client, token } => {
                gmail::update_event(client, token, event_id, event).await
            }
            Session::Imap(_) => bail!("{}", crate::tr!("calendar-error-imap-unsupported")),
        }
    }

    pub async fn delete_event(&self, event_id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::delete_event(client, event_id).await,
            Session::Gmail { client, token } => gmail::delete_event(client, token, event_id).await,
            Session::Imap(_) => bail!("{}", crate::tr!("calendar-error-imap-unsupported")),
        }
    }

    pub async fn move_event(
        &self,
        event_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        all_day: bool,
    ) -> Result<()> {
        match self {
            Session::Graph { client, .. } => {
                graph::move_event(client, event_id, start, end, all_day).await
            }
            Session::Gmail { client, token } => {
                gmail::move_event(client, token, event_id, start, end, all_day).await
            }
            Session::Imap(_) => bail!("{}", crate::tr!("calendar-error-imap-unsupported")),
        }
    }

    pub async fn list_people(&self, top: usize) -> Result<Vec<Contact>> {
        match self {
            Session::Graph { client, .. } => graph::list_people(client, top).await,
            Session::Gmail { client, token } => gmail::list_people(client, token, top).await,
            // IMAP has no contacts — return empty.
            Session::Imap(_) => Ok(Vec::new()),
        }
    }

    /// Send a new message. `track_sent` asks the backend for identifiers of
    /// the Sent-items copy when obtaining them has a cost: Graph then goes
    /// through a draft (create → send) instead of the single `sendMail`
    /// call, so the `Message-ID` can be read before sending. Gmail returns
    /// its ids for free either way; SMTP never has any.
    pub async fn send_mail(&self, msg: &OutgoingMessage<'_>, track_sent: bool) -> Result<SentIds> {
        match self {
            Session::Graph { client, .. } => {
                if track_sent {
                    graph::send_mail_tracked(client, &graph_outgoing(msg)).await
                } else {
                    graph::send_mail(client, &graph_outgoing(msg))
                        .await
                        .map(|()| SentIds::default())
                }
            }
            Session::Gmail { client, token } => gmail::send_mail(client, token, msg).await,
            Session::Imap(imap) => imap::send_mail(imap, msg)
                .await
                .map(|()| SentIds::default()),
        }
    }

    pub async fn send_reply(
        &self,
        reply_to_id: &str,
        msg: &OutgoingMessage<'_>,
    ) -> Result<SentIds> {
        match self {
            Session::Graph { client, .. } => {
                graph::send_reply(client, reply_to_id, &graph_outgoing(msg)).await
            }
            Session::Gmail { client, token } => {
                gmail::send_reply(client, token, reply_to_id, msg).await
            }
            Session::Imap(imap) => imap::send_reply(imap, reply_to_id, msg)
                .await
                .map(|()| SentIds::default()),
        }
    }

    /// Find the provider id of the Sent-items copy whose RFC 5322
    /// `Message-ID` is `internet_message_id`. `Ok(None)` when the copy is
    /// not there (yet) or the backend has no such lookup.
    pub async fn find_sent_message(&self, internet_message_id: &str) -> Result<Option<String>> {
        match self {
            Session::Graph { client, .. } => {
                graph::find_sent_copy_id(client, internet_message_id).await
            }
            // Gmail replies already carry the sent copy's id from
            // `messages.send`; IMAP has no server-side lookup.
            Session::Gmail { .. } | Session::Imap(_) => Ok(None),
        }
    }

    /// Persist `msg` as a draft on the provider. When `replace_id` is `Some`
    /// the existing draft is updated in place (Graph PATCH, Gmail PUT, IMAP
    /// APPEND-then-EXPUNGE-old). When `None`, a fresh draft is created.
    /// Returns the provider-native draft id of the resulting draft when
    /// available — `None` only on IMAP servers without UIDPLUS, where we
    /// can't read back the new UID after APPEND.
    pub async fn save_draft(
        &self,
        msg: &OutgoingMessage<'_>,
        replace_id: Option<&str>,
    ) -> Result<Option<String>> {
        match self {
            Session::Graph { client, .. } => {
                graph::save_draft(client, &graph_outgoing(msg), replace_id)
                    .await
                    .map(Some)
            }
            Session::Gmail { client, token } => gmail::save_draft(client, token, msg, replace_id)
                .await
                .map(Some),
            Session::Imap(imap) => imap::save_draft(imap, msg, replace_id).await,
        }
    }

    /// Delete a draft by its provider-native draft id. Called by the runtime
    /// after a successful send to clean up the originating draft.
    pub async fn delete_draft(&self, draft_id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::delete_draft(client, draft_id).await,
            Session::Gmail { client, token } => gmail::delete_draft(client, token, draft_id).await,
            Session::Imap(imap) => imap::delete_draft(imap, draft_id).await,
        }
    }

    pub async fn create_folder(&self, name: &str, parent_id: Option<&str>) -> Result<MailFolder> {
        match self {
            Session::Graph { client, .. } => graph::create_folder(client, name, parent_id).await,
            Session::Gmail { client, token } => {
                gmail::create_folder(client, token, name, parent_id).await
            }
            Session::Imap(imap) => imap::create_folder(imap, name.to_string(), parent_id).await,
        }
    }

    /// Rename a folder and return its new provider id when the backend uses
    /// path-based ids (IMAP). Graph and Gmail keep stable ids and return None.
    pub async fn rename_folder(&self, id: &str, new_name: &str) -> Result<Option<String>> {
        match self {
            Session::Graph { client, .. } => graph::rename_folder(client, id, new_name)
                .await
                .map(|()| None),
            Session::Gmail { client, token } => gmail::rename_folder(client, token, id, new_name)
                .await
                .map(|()| None),
            Session::Imap(imap) => imap::rename_folder(imap, id.to_string(), new_name.to_string())
                .await
                .map(Some),
        }
    }

    pub async fn delete_folder(&self, id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::delete_folder(client, id).await,
            Session::Gmail { client, token } => gmail::delete_folder(client, token, id).await,
            Session::Imap(imap) => imap::delete_folder(imap, id.to_string()).await,
        }
    }

    /// Move `id` into `target_folder_id`. Returns the new message id when the
    /// provider reassigns one (Graph always does, IMAP MOVE may), `None` when
    /// the id stays stable (Gmail labels). `source_folder_id` is only
    /// consulted by Gmail to know which label to drop.
    pub async fn move_message(
        &self,
        id: &str,
        source_folder_id: Option<&str>,
        target_folder_id: &str,
    ) -> Result<Option<String>> {
        match self {
            Session::Graph { client, .. } => {
                graph::move_message(client, id, target_folder_id).await
            }
            Session::Gmail { client, token } => {
                gmail::move_message(client, token, id, source_folder_id, target_folder_id).await
            }
            Session::Imap(imap) => imap::move_message(imap, id, target_folder_id).await,
        }
    }

    pub async fn list_tags(&self) -> Result<Vec<Tag>> {
        match self {
            Session::Graph { client, .. } => graph::list_tags(client).await,
            Session::Gmail { client, token } => gmail::list_tags(client, token).await,
            Session::Imap(imap) => imap::list_tags(imap).await,
        }
    }

    pub async fn create_tag(&self, name: &str, color: Option<u32>) -> Result<Tag> {
        match self {
            Session::Graph { client, .. } => graph::create_tag(client, name, color).await,
            Session::Gmail { client, token } => gmail::create_tag(client, token, name).await,
            Session::Imap(imap) => imap::create_tag(imap, name).await,
        }
    }

    pub async fn rename_tag(&self, id: &str, new_name: &str) -> Result<TagRename> {
        match self {
            Session::Graph { client, .. } => graph::rename_tag(client, id, new_name).await,
            Session::Gmail { client, token } => {
                gmail::rename_tag(client, token, id, new_name).await
            }
            Session::Imap(imap) => imap::rename_tag(imap, id, new_name).await,
        }
    }

    pub async fn delete_tag(&self, id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::delete_tag(client, id).await,
            Session::Gmail { client, token } => gmail::delete_tag(client, token, id).await,
            Session::Imap(imap) => imap::delete_tag(imap, id).await,
        }
    }

    /// Change a tag's color. `color` should come from
    /// [`tag_color_palette`] for the account's provider — Gmail rejects
    /// values outside its allowed list, Graph snaps to the nearest preset.
    /// IMAP keywords carry no color.
    pub async fn set_tag_color(&self, id: &str, color: u32) -> Result<()> {
        match self {
            Session::Graph { client, .. } => graph::set_tag_color(client, id, color).await,
            Session::Gmail { client, token } => {
                gmail::set_tag_color(client, token, id, color).await
            }
            Session::Imap(_) => bail!(tr!("imap-error-tag-color-unsupported")),
        }
    }

    pub async fn add_tag_to_message(&self, message_id: &str, tag_id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => {
                graph::add_tag_to_message(client, message_id, tag_id).await
            }
            Session::Gmail { client, token } => {
                gmail::add_tag_to_message(client, token, message_id, tag_id).await
            }
            Session::Imap(imap) => imap::add_tag_to_message(imap, message_id, tag_id).await,
        }
    }

    pub async fn remove_tag_from_message(&self, message_id: &str, tag_id: &str) -> Result<()> {
        match self {
            Session::Graph { client, .. } => {
                graph::remove_tag_from_message(client, message_id, tag_id).await
            }
            Session::Gmail { client, token } => {
                gmail::remove_tag_from_message(client, token, message_id, tag_id).await
            }
            Session::Imap(imap) => imap::remove_tag_from_message(imap, message_id, tag_id).await,
        }
    }

    pub async fn list_messages_tagged(
        &self,
        tag_id: &str,
        top: usize,
    ) -> Result<Vec<MessageHeader>> {
        match self {
            Session::Graph { client, .. } => graph::list_messages_tagged(client, tag_id, top).await,
            Session::Gmail { client, token } => {
                gmail::list_messages_tagged(client, token, tag_id, top).await
            }
            Session::Imap(imap) => imap::list_messages_tagged(imap, tag_id, top).await,
        }
    }
}

/// Enforce the provider abstraction's invariant that images embedded through
/// `cid:` never leak into the regular attachment list.
///
/// Backends normally classify MIME parts before constructing [`Message`].
/// This final filename-based guard covers malformed provider metadata and old
/// cached messages while keeping non-image files untouched.
pub(crate) fn remove_embedded_image_attachments(message: &mut Message) {
    let Some(html) = message.raw_body.as_deref() else {
        return;
    };
    let html_cids = html::extract_cids_from_html(html);
    if html_cids.is_empty() {
        return;
    }
    message.attachments.retain(|attachment| {
        !attachment_looks_like_image(attachment)
            || !html_cids
                .iter()
                .any(|reference| html::cid_references_name(reference, &attachment.filename))
    });
    message.header.has_attachments = !message.attachments.is_empty();
}

fn attachment_looks_like_image(attachment: &crate::model::Attachment) -> bool {
    attachment.mime.to_ascii_lowercase().starts_with("image/")
        || matches!(
            attachment
                .filename
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
                .as_deref(),
            Some(
                "apng"
                    | "avif"
                    | "bmp"
                    | "gif"
                    | "heic"
                    | "heif"
                    | "ico"
                    | "jpg"
                    | "jpeg"
                    | "png"
                    | "svg"
                    | "tif"
                    | "tiff"
                    | "webp"
            )
        )
}

fn graph_outgoing<'a>(msg: &'a OutgoingMessage<'a>) -> graph::OutgoingMessage<'a> {
    graph::OutgoingMessage {
        to: msg.to,
        cc: msg.cc,
        bcc: msg.bcc,
        subject: msg.subject,
        body: msg.body,
        body_is_html: msg.body_is_html,
        attachments: msg.attachments,
        files: msg.files,
    }
}

#[cfg(test)]
mod mime_tests {
    use super::*;
    use crate::model::{
        AccountId, Attachment as FileAttachment, BodyFormat, InlineImage, MessageHeader,
    };

    #[test]
    fn shared_rfc822_builder_preserves_gmail_bcc_and_mime_layers() {
        let to = vec!["to@example.com".to_string()];
        let bcc = vec!["hidden@example.com".to_string()];
        let images = vec![InlineImage {
            cid: "logo".to_string(),
            mime: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        }];
        let files = vec![FileAttachment {
            id: String::new(),
            filename: "notes.txt".to_string(),
            mime: "text/plain".to_string(),
            size: 3,
            bytes: Some(b"abc".to_vec()),
        }];
        let message = OutgoingMessage {
            from: "from@example.com",
            to: &to,
            cc: &[],
            bcc: &bcc,
            subject: "Réunion",
            body: r#"<p><img src="cid:logo"></p>"#,
            body_is_html: true,
            attachments: &images,
            files: &files,
        };

        let gmail = String::from_utf8(
            build_rfc822(
                &message,
                &[("In-Reply-To", "<message@example.com>".to_string())],
                true,
            )
            .unwrap()
            .formatted(),
        )
        .unwrap();
        assert!(gmail.contains("Bcc: hidden@example.com"));
        assert!(gmail.contains("In-Reply-To: <message@example.com>"));
        assert!(gmail.contains("multipart/mixed"));
        assert!(gmail.contains("multipart/related"));
        assert!(gmail.contains("Content-ID: <logo>"));
        assert!(gmail.contains("filename=\"notes.txt\""));

        let smtp =
            String::from_utf8(build_rfc822(&message, &[], false).unwrap().formatted()).unwrap();
        assert!(!smtp.contains("Bcc: hidden@example.com"));
    }

    #[test]
    fn provider_guard_removes_cid_image_from_file_list() {
        let mut message = Message {
            header: MessageHeader {
                account_id: AccountId("test".into()),
                id: "message-1".into(),
                subject: "Synthetic subject".into(),
                from: "Contact A <contact-a@example.test>".into(),
                received: Utc::now(),
                preview: String::new(),
                is_read: true,
                is_flagged: false,
                has_attachments: true,
                tags: Vec::new(),
                last_action: None,
                last_action_at: None,
                conversation_id: None,
                internet_message_id: None,
            },
            body: String::new(),
            format: BodyFormat::Markdown,
            inline_images: Vec::new(),
            attachments: vec![FileAttachment {
                id: "inline-1".into(),
                filename: "inline-mark.png".into(),
                mime: "application/octet-stream".into(),
                size: 12,
                bytes: None,
            }],
            tags: Vec::new(),
            raw_body: Some(r#"<img src="cid:inline-mark.png">"#.into()),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            draft_id: None,
            invitation: None,
        };

        remove_embedded_image_attachments(&mut message);

        assert!(message.attachments.is_empty());
        assert!(!message.header.has_attachments);
    }
}
