use super::attachments::fetch_attachments;
use super::{from_label, patch_json, post_json, Client, GraphList, GraphRecipient, BASE};
use crate::model::{
    AccountId, Attachment, BodyFormat, CalendarInvitation, InlineImage, InvitationResponse,
    LastAction, Message, MessageHeader,
};
use crate::providers::html::{collapse_blank_lines, convert_email_html, extract_cids_from_html};
use crate::providers::{MailSyncPage, MessagePage, SentIds};
use crate::search_query::SearchQuery;
use anyhow::Result;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Deserialize)]
struct GraphMessage {
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
    id: String,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    #[serde(rename = "receivedDateTime")]
    received_date_time: DateTime<Utc>,
    #[serde(rename = "bodyPreview")]
    body_preview: Option<String>,
    #[serde(rename = "isRead")]
    is_read: bool,
    body: Option<GraphBody>,
    #[serde(rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(rename = "internetMessageId")]
    internet_message_id: Option<String>,
    flag: Option<GraphFlag>,
    #[serde(default, rename = "hasAttachments")]
    has_attachments: bool,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default, rename = "toRecipients")]
    to_recipients: Vec<GraphRecipient>,
    #[serde(default, rename = "ccRecipients")]
    cc_recipients: Vec<GraphRecipient>,
    #[serde(default, rename = "bccRecipients")]
    bcc_recipients: Vec<GraphRecipient>,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
    #[serde(default, rename = "singleValueExtendedProperties")]
    extended_properties: Vec<GraphExtendedProperty>,
    #[serde(rename = "meetingMessageType")]
    meeting_message_type: Option<String>,
    event: Option<GraphInvitationEvent>,
}

#[derive(Deserialize)]
struct GraphInvitationEvent {
    id: String,
    subject: Option<String>,
    start: Option<GraphInvitationDateTime>,
    end: Option<GraphInvitationDateTime>,
    location: Option<GraphInvitationLocation>,
    organizer: Option<GraphRecipient>,
    #[serde(default, rename = "isAllDay")]
    is_all_day: bool,
    #[serde(default, rename = "isCancelled")]
    is_cancelled: bool,
    #[serde(rename = "responseStatus")]
    response_status: Option<GraphInvitationResponse>,
}

#[derive(Deserialize)]
struct GraphInvitationDateTime {
    #[serde(rename = "dateTime")]
    date_time: String,
}

#[derive(Deserialize)]
struct GraphInvitationLocation {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct GraphInvitationResponse {
    response: String,
}

fn parse_invitation_datetime(value: &GraphInvitationDateTime) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value.date_time)
        .ok()
        .map(|date| date.with_timezone(&Utc))
        .or_else(|| {
            let raw = value.date_time.trim_end_matches('Z');
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|date| DateTime::<Utc>::from_naive_utc_and_offset(date, Utc))
        })
}

fn invitation_from_graph_message(message: &mut GraphMessage) -> Option<CalendarInvitation> {
    let request = message
        .odata_type
        .as_deref()
        .is_some_and(|kind| kind.ends_with("eventMessageRequest"))
        || message
            .meeting_message_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("meetingRequest"));
    if !request {
        return None;
    }
    let event = message.event.take()?;
    if event.is_cancelled {
        return None;
    }
    let start = event.start.as_ref().and_then(parse_invitation_datetime)?;
    let mut end = event
        .end
        .as_ref()
        .and_then(parse_invitation_datetime)
        .unwrap_or(start);
    if event.is_all_day && end > start {
        end -= chrono::Duration::days(1);
    }
    let response = match event
        .response_status
        .as_ref()
        .map(|status| status.response.as_str())
        .unwrap_or_default()
    {
        value if value.eq_ignore_ascii_case("accepted") => InvitationResponse::Accepted,
        value
            if value.eq_ignore_ascii_case("tentativelyAccepted")
                || value.eq_ignore_ascii_case("tentative") =>
        {
            InvitationResponse::Tentative
        }
        value if value.eq_ignore_ascii_case("declined") => InvitationResponse::Declined,
        _ => InvitationResponse::NeedsAction,
    };
    Some(CalendarInvitation {
        event_id: event.id,
        subject: event
            .subject
            .filter(|subject| !subject.is_empty())
            .or_else(|| message.subject.clone())
            .unwrap_or_default(),
        start,
        end,
        all_day: event.is_all_day,
        location: event
            .location
            .and_then(|location| location.display_name)
            .unwrap_or_default(),
        organizer: from_label(event.organizer),
        response,
    })
}

#[derive(Deserialize)]
struct GraphExtendedProperty {
    id: String,
    value: String,
}

/// Decodes `PidTagLastVerbExecuted` (0x1081) and
/// `PidTagLastVerbExecutionTime` (0x1082) from the extended properties joined
/// by [`MESSAGE_EXPAND`]. Verbs other than reply/reply-all/forward are ignored.
fn parse_last_verb(props: &[GraphExtendedProperty]) -> (Option<LastAction>, Option<DateTime<Utc>>) {
    let mut action = None;
    let mut at = None;
    for p in props {
        let id = p.id.to_ascii_lowercase();
        if id.ends_with("0x1081") {
            action = match p.value.trim() {
                "102" => Some(LastAction::Replied),
                "103" => Some(LastAction::RepliedAll),
                "104" => Some(LastAction::Forwarded),
                _ => None,
            };
        } else if id.ends_with("0x1082") {
            at = DateTime::parse_from_rfc3339(p.value.trim())
                .ok()
                .map(|d| d.with_timezone(&Utc));
        }
    }
    (action, at.filter(|_| action.is_some()))
}

#[derive(Deserialize)]
struct GraphFlag {
    #[serde(rename = "flagStatus")]
    flag_status: Option<String>,
}

#[derive(Deserialize)]
struct GraphBody {
    #[serde(rename = "contentType")]
    content_type: String,
    content: String,
}

impl From<GraphMessage> for MessageHeader {
    fn from(mut m: GraphMessage) -> Self {
        let is_flagged = m
            .flag
            .as_ref()
            .and_then(|f| f.flag_status.as_deref())
            .is_some_and(|s| s.eq_ignore_ascii_case("flagged"));
        let tags = std::mem::take(&mut m.categories);
        let (last_action, last_action_at) = parse_last_verb(&m.extended_properties);
        Self {
            id: m.id,
            account_id: AccountId::default(),
            subject: m.subject.unwrap_or_default(),
            from: from_label(m.from),
            received: m.received_date_time,
            preview: m.body_preview.unwrap_or_default(),
            is_read: m.is_read,
            is_flagged,
            has_attachments: m.has_attachments,
            tags,
            last_action,
            last_action_at,
            conversation_id: m.conversation_id.filter(|id| !id.is_empty()),
            internet_message_id: m
                .internet_message_id
                .map(|id| id.trim_matches(['<', '>']).to_string())
                .filter(|id| !id.is_empty()),
        }
    }
}

pub struct OutgoingMessage<'a> {
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub body_is_html: bool,
    /// Inline images embedded in the body via `cid:`.
    pub attachments: &'a [InlineImage],
    /// Non-inline attachments (regular files the recipient sees as
    /// downloadable attachments).
    pub files: &'a [Attachment],
}

const MESSAGE_SELECT: &str =
    "id,subject,from,receivedDateTime,bodyPreview,isRead,flag,hasAttachments,categories,toRecipients,ccRecipients,isDraft,conversationId,internetMessageId";

/// Includes the latest reply/forward verb (see [`parse_last_verb`]) in list
/// and read requests. Do not add it to the `sync_folder_messages` delta flow:
/// Graph does not support `$expand` there.
const MESSAGE_EXPAND: &str = "singleValueExtendedProperties($filter=(id eq 'Integer 0x1081') or (id eq 'SystemTime 0x1082'))";

fn build_message_json(msg: &OutgoingMessage<'_>, include_subject: bool) -> serde_json::Value {
    let recipient = |a: &String| serde_json::json!({ "emailAddress": { "address": a } });
    let engine = base64::engine::general_purpose::STANDARD;
    let mut attachments_json: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .enumerate()
        .map(|(i, a)| {
            serde_json::json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": format!("{}.{}", a.cid, ext_for_mime(&a.mime)).replace(['/', ' '], "_"),
                "contentType": a.mime,
                "contentId": a.cid,
                "isInline": true,
                "contentBytes": engine.encode(&a.bytes),
                "id": format!("att-{i}"),
            })
        })
        .collect();
    for (i, f) in msg.files.iter().enumerate() {
        let bytes = f.bytes.as_deref().unwrap_or_default();
        attachments_json.push(serde_json::json!({
            "@odata.type": "#microsoft.graph.fileAttachment",
            "name": f.filename,
            "contentType": f.mime,
            "isInline": false,
            "contentBytes": engine.encode(bytes),
            "id": format!("file-{i}"),
        }));
    }
    let mut message = serde_json::json!({
        "body": {
            "contentType": if msg.body_is_html { "HTML" } else { "Text" },
            "content": msg.body,
        },
        "toRecipients": msg.to.iter().map(&recipient).collect::<Vec<_>>(),
        "ccRecipients": msg.cc.iter().map(&recipient).collect::<Vec<_>>(),
        "bccRecipients": msg.bcc.iter().map(&recipient).collect::<Vec<_>>(),
    });
    if include_subject {
        message["subject"] = serde_json::Value::String(msg.subject.to_string());
    }
    if !attachments_json.is_empty() {
        message["attachments"] = serde_json::Value::Array(attachments_json);
    }
    message
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

pub async fn send_mail(client: &Client<'_>, msg: &OutgoingMessage<'_>) -> Result<()> {
    let url = format!("{BASE}/me/sendMail");
    let payload = serde_json::json!({
        "message": build_message_json(msg, true),
        "saveToSentItems": true,
    });
    post_json(client, &url, &payload, "sendMail").await
}

/// Ids returned when Graph creates a draft (`POST /me/messages`,
/// `createReply`). `internetMessageId` is assigned at creation and survives
/// the move to Sent Items, unlike the mutable `id`.
#[derive(Deserialize)]
struct DraftIds {
    id: String,
    #[serde(default, rename = "internetMessageId")]
    internet_message_id: Option<String>,
}

async fn post_draft(
    client: &Client<'_>,
    url: &str,
    payload: &serde_json::Value,
    label: &str,
) -> Result<DraftIds> {
    let resp = client.post(url).json(payload).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, &format!("graph {label} failed")).await);
    }
    Ok(resp.json().await?)
}

/// Split the `attachments` array out of a [`build_message_json`] payload:
/// Graph does not accept attachments through message create/PATCH, they must
/// be POSTed one by one to `/me/messages/{id}/attachments` (which also
/// rejects the client-side `id` hint used by `sendMail`).
fn take_attachments(payload: &mut serde_json::Value) -> Vec<serde_json::Value> {
    let Some(serde_json::Value::Array(list)) = payload
        .as_object_mut()
        .and_then(|obj| obj.remove("attachments"))
    else {
        return Vec::new();
    };
    list.into_iter()
        .map(|mut attachment| {
            if let Some(obj) = attachment.as_object_mut() {
                obj.remove("id");
            }
            attachment
        })
        .collect()
}

/// Upload attachments then send the prepared draft. On failure the draft is
/// deleted (best effort) so it does not linger in the Drafts folder.
async fn finish_draft_send(
    client: &Client<'_>,
    draft_id: &str,
    attachments: Vec<serde_json::Value>,
) -> Result<()> {
    let result: Result<()> = async {
        for attachment in &attachments {
            let url = format!("{BASE}/me/messages/{draft_id}/attachments");
            post_json(client, &url, attachment, "addAttachment").await?;
        }
        let url = format!("{BASE}/me/messages/{draft_id}/send");
        post_json(client, &url, &serde_json::json!({}), "sendDraft").await
    }
    .await;
    if let Err(e) = result {
        if let Err(cleanup) = delete_message(client, draft_id).await {
            log::warn!("cleanup of unsent graph draft failed: {cleanup:#}");
        }
        return Err(e);
    }
    Ok(())
}

/// Same outcome as [`send_mail`] but through a draft (create → attach →
/// send), so the `Message-ID` of the outgoing mail is known before it goes
/// out and the Sent-items copy can be recovered later via
/// [`find_sent_copy_id`].
pub async fn send_mail_tracked(client: &Client<'_>, msg: &OutgoingMessage<'_>) -> Result<SentIds> {
    let mut payload = build_message_json(msg, true);
    let attachments = take_attachments(&mut payload);
    let url = format!("{BASE}/me/messages");
    let draft = post_draft(client, &url, &payload, "createDraft").await?;
    finish_draft_send(client, &draft.id, attachments).await?;
    Ok(SentIds {
        message_id: None,
        internet_message_id: draft.internet_message_id,
    })
}

/// Find the Sent-items copy of a message by its RFC 5322 `Message-ID`.
/// `Ok(None)` while the copy has not landed in the folder yet — Graph
/// documents a possible delay after sending.
pub async fn find_sent_copy_id(
    client: &Client<'_>,
    internet_message_id: &str,
) -> Result<Option<String>> {
    let filter = format!(
        "internetMessageId eq '{}'",
        internet_message_id.replace('\'', "''")
    );
    let url = format!("{BASE}/me/mailFolders/sentitems/messages");
    let resp = client
        .get(&url)
        .query(&[
            ("$filter", filter.as_str()),
            ("$select", "id"),
            ("$top", "1"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph sent lookup failed").await);
    }
    #[derive(Deserialize)]
    struct IdOnly {
        id: String,
    }
    let list: GraphList<IdOnly> = resp.json().await?;
    Ok(list.value.into_iter().next().map(|m| m.id))
}

/// Save the outgoing message as a draft. When `replace_id` is `Some`, we
/// `PATCH /me/messages/{id}` so the existing draft is updated in place
/// (Outlook and the Graph mailbox both prefer this to a delete+create dance,
/// which would lose the conversation thread). When `None`, `POST
/// /me/messages` creates a fresh draft. Returns the message id of the
/// resulting draft so the caller can keep editing it.
pub async fn save_draft(
    client: &Client<'_>,
    msg: &OutgoingMessage<'_>,
    replace_id: Option<&str>,
) -> Result<String> {
    let payload = build_message_json(msg, true);
    if let Some(id) = replace_id {
        let url = format!("{BASE}/me/messages/{id}");
        let resp = client.patch(&url).json(&payload).send().await?;
        if !resp.status().is_success() {
            return Err(crate::providers::http_error(resp, "graph saveDraft (patch) failed").await);
        }
        // Graph keeps the same id on PATCH; reading the body is optional.
        Ok(id.to_string())
    } else {
        let url = format!("{BASE}/me/messages");
        let resp = client.post(&url).json(&payload).send().await?;
        if !resp.status().is_success() {
            return Err(crate::providers::http_error(resp, "graph saveDraft failed").await);
        }
        #[derive(Deserialize)]
        struct CreateResp {
            id: String,
        }
        let body: CreateResp = resp.json().await?;
        Ok(body.id)
    }
}

/// Reply through a draft: `createReply` seeds the threading headers and the
/// conversation id server-side, the PATCH swaps in Aviary's body and
/// recipients (covers reply-all — recipients are computed client-side), and
/// the send happens last so the draft's `Message-ID` is already known. The
/// previous one-shot `/reply` action returned `202 Accepted` with no body,
/// leaving no way to recover the Sent-items copy.
pub async fn send_reply(
    client: &Client<'_>,
    reply_to_id: &str,
    msg: &OutgoingMessage<'_>,
) -> Result<SentIds> {
    let url = format!("{BASE}/me/messages/{reply_to_id}/createReply");
    let draft = post_draft(client, &url, &serde_json::json!({}), "createReply").await?;
    let mut payload = build_message_json(msg, false);
    let attachments = take_attachments(&mut payload);
    let patch_url = format!("{BASE}/me/messages/{}", draft.id);
    if let Err(e) = patch_json(client, &patch_url, &payload, "updateReply").await {
        if let Err(cleanup) = delete_message(client, &draft.id).await {
            log::warn!("cleanup of unsent graph reply draft failed: {cleanup:#}");
        }
        return Err(e);
    }
    finish_draft_send(client, &draft.id, attachments).await?;
    Ok(SentIds {
        message_id: None,
        internet_message_id: draft.internet_message_id,
    })
}

pub async fn list_folder_messages(
    client: &Client<'_>,
    folder_id: Option<&str>,
    top: usize,
    skip: usize,
) -> Result<Vec<MessageHeader>> {
    let folder = folder_id.unwrap_or("inbox");
    let url = format!(
        "{BASE}/me/mailFolders/{folder}/messages?$top={top}&$skip={skip}&$orderby=receivedDateTime desc&$select={MESSAGE_SELECT}&$expand={MESSAGE_EXPAND}"
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph list failed").await);
    }
    let list: GraphList<GraphMessage> = resp.json().await?;
    Ok(list.value.into_iter().map(Into::into).collect())
}

pub async fn list_folder_messages_page(
    client: &Client<'_>,
    folder_id: Option<&str>,
    top: usize,
) -> Result<MessagePage> {
    let folder = folder_id.unwrap_or("inbox");
    let url = format!(
        "{BASE}/me/mailFolders/{folder}/messages?$top={top}&$orderby=receivedDateTime desc&$select={MESSAGE_SELECT}&$expand={MESSAGE_EXPAND}"
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph list failed").await);
    }
    let list: GraphList<GraphMessage> = resp.json().await?;
    Ok(MessagePage {
        messages: list.value.into_iter().map(Into::into).collect(),
        next: list.next_link,
    })
}

#[derive(Deserialize)]
struct GraphDeltaPage {
    #[serde(default)]
    value: Vec<serde_json::Value>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

/// Delta synchronization for an Outlook folder. The first call
/// scans the folder and eventually produces a `deltaLink`; subsequent calls
/// return only changed or deleted objects.
pub async fn sync_folder_messages(
    client: &Client<'_>,
    folder_id: Option<&str>,
    cursor_or_page: Option<&str>,
) -> Result<MailSyncPage> {
    let url = cursor_or_page.map(str::to_string).unwrap_or_else(|| {
        let folder = folder_id.unwrap_or("inbox");
        format!(
            "{BASE}/me/mailFolders/{folder}/messages/delta?$select={MESSAGE_SELECT}&$orderby=receivedDateTime%20desc"
        )
    });
    let resp = client
        .get(url)
        .header("Prefer", "odata.maxpagesize=100")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph delta failed").await);
    }
    let page: GraphDeltaPage = resp.json().await?;
    let mut result = MailSyncPage {
        next: page.next_link,
        cursor: page.delta_link,
        ..MailSyncPage::default()
    };
    for value in page.value {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if value.get("@removed").is_some() {
            if !id.is_empty() {
                result.deleted.push(id);
            }
            continue;
        }
        match serde_json::from_value::<GraphMessage>(value) {
            Ok(message) => result.upserts.push(message.into()),
            Err(e) => log::warn!("ignored malformed Graph delta message: {e:#}"),
        }
    }
    Ok(result)
}

pub async fn get_message(client: &Client<'_>, id: &str) -> Result<Message> {
    // Same breakdown as the Gmail path, on purpose: `fetch_ms` alone cannot
    // tell a slow round trip from a slow HTML conversion, and comparing the
    // two backends is only meaningful with identical stage names.
    let started = std::time::Instant::now();
    let url = format!("{BASE}/me/messages/{id}");
    let expand = format!("{MESSAGE_EXPAND},microsoft.graph.eventMessage/event");
    let resp = client
        .get(&url)
        .query(&[("$expand", expand.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph get failed").await);
    }
    let mut m: GraphMessage = resp.json().await?;
    let payload_elapsed = started.elapsed();
    let invitation = invitation_from_graph_message(&mut m);
    let body = m.body.take();
    let has_any_attachments = m.has_attachments;
    let tags = std::mem::take(&mut m.categories);
    let to: Vec<String> = std::mem::take(&mut m.to_recipients)
        .into_iter()
        .map(|r| from_label(Some(r)))
        .filter(|s| !s.is_empty())
        .collect();
    let cc: Vec<String> = std::mem::take(&mut m.cc_recipients)
        .into_iter()
        .map(|r| from_label(Some(r)))
        .filter(|s| !s.is_empty())
        .collect();
    let bcc: Vec<String> = std::mem::take(&mut m.bcc_recipients)
        .into_iter()
        .map(|r| from_label(Some(r)))
        .filter(|s| !s.is_empty())
        .collect();
    let is_draft = m.is_draft;
    let header: MessageHeader = m.into();
    let draft_id = is_draft.then(|| header.id.clone());

    // Collect the `cid:` references the body actually points to *before*
    // fetching the attachment list. The classifier needs them to decide
    // which parts are inline images vs. downloadable files — Graph's own
    // `isInline` flag is too unreliable to use on its own.
    let html_cids = match &body {
        Some(b) if b.content_type.eq_ignore_ascii_case("html") => {
            extract_cids_from_html(&b.content)
        }
        _ => Vec::new(),
    };
    log::debug!(
        "Graph message HTML contains {} cid reference(s)",
        html_cids.len()
    );

    // We pull every attachment (inline + files) in one call and split them.
    // Inline images get rewritten into the body's `cid:` URIs; files come
    // back through `Message::attachments` for the viewer to list.
    //
    // Important: Graph's `hasAttachments` field is documented to ignore
    // inline attachments — a message that only carries an embedded
    // signature logo will report `hasAttachments: false`. Fall back to
    // querying when the body contains `cid:` references, otherwise those
    // images render as "missing inline image".
    let needs_attachments = has_any_attachments || !html_cids.is_empty();
    let attachments_started = std::time::Instant::now();
    let (inline_all, file_attachments) = if needs_attachments {
        match fetch_attachments(client, id, &html_cids).await {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("failed to fetch attachments: {e:#}");
                (Vec::new(), Vec::new())
            }
        }
    } else {
        (Vec::new(), Vec::new())
    };
    // Graph pulls inline images and files in the same call, so this single
    // duration is the counterpart of Gmail's per-attachment fan-out.
    let inline_elapsed = attachments_started.elapsed();

    let mut convert_elapsed = std::time::Duration::ZERO;
    let (content, format, inline_images, raw_body) = match body {
        Some(b) if b.content_type.eq_ignore_ascii_case("html") => {
            let inline = inline_all;
            let raw_html = b.content;
            let html = raw_html.clone();
            let convert_started = std::time::Instant::now();
            let md = tokio::task::spawn_blocking(move || convert_email_html(&html))
                .await
                .map_err(|error| {
                    anyhow::anyhow!(tr!("message-error-html-conversion", {
                        error: error
                    }))
                })?;
            let md = inline.iter().fold(md, |acc, img| {
                acc.replace(
                    &format!("cid:{}", img.cid),
                    &format!("bytes://cid-{}", img.cid),
                )
            });
            let md = drop_unresolved_cid_images(&md);
            let content = collapse_blank_lines(&md);
            convert_elapsed = convert_started.elapsed();
            (content, BodyFormat::Markdown, inline, Some(raw_html))
        }
        Some(b) => (b.content, BodyFormat::Text, Vec::new(), None),
        None => (String::new(), BodyFormat::Text, Vec::new(), None),
    };

    log::debug!(
        "graph get_message in {} ms \
         (payload_ms={}, inline_ms={}, inline_images={}, convert_ms={}, \
         attachments_fetched={})",
        started.elapsed().as_millis(),
        payload_elapsed.as_millis(),
        inline_elapsed.as_millis(),
        inline_images.len(),
        convert_elapsed.as_millis(),
        needs_attachments
    );

    Ok(Message {
        header,
        body: content,
        format,
        inline_images,
        attachments: file_attachments,
        tags,
        raw_body,
        to,
        cc,
        bcc,
        draft_id,
        invitation,
    })
}

pub async fn delete_message(client: &Client<'_>, id: &str) -> Result<()> {
    let url = format!("{BASE}/me/messages/{id}");
    let resp = client.delete(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph delete failed").await);
    }
    Ok(())
}

/// Move a message into a different mail folder. Graph reassigns the message
/// a new id when it crosses folders; we return that id so the caller can
/// update its in-memory list. `target_folder_id` may be a real folder id or
/// a well-known alias (`"inbox"`, `"deleteditems"`, …).
pub async fn move_message(
    client: &Client<'_>,
    id: &str,
    target_folder_id: &str,
) -> Result<Option<String>> {
    let url = format!("{BASE}/me/messages/{id}/move");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "destinationId": target_folder_id }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph move failed").await);
    }
    #[derive(serde::Deserialize)]
    struct MoveResp {
        id: Option<String>,
    }
    let body: MoveResp = resp.json().await.unwrap_or(MoveResp { id: None });
    Ok(body.id)
}

/// Marks `id` as replied to or forwarded by writing `PidTagLastVerbExecuted`
/// and `PidTagLastVerbExecutionTime`, the same MAPI properties used by
/// Outlook, so the state remains visible in Outlook/OWA. Called after a
/// successful send; Graph's `/reply` action already sets the server-side verb,
/// but it is always rewritten here to avoid relying on that behavior.
pub async fn note_last_action(client: &Client<'_>, id: &str, action: LastAction) -> Result<()> {
    let verb = match action {
        LastAction::Replied => 102,
        LastAction::RepliedAll => 103,
        LastAction::Forwarded => 104,
    };
    let url = format!("{BASE}/me/messages/{id}");
    let payload = serde_json::json!({
        "singleValueExtendedProperties": [
            { "id": "Integer 0x1081", "value": verb.to_string() },
            {
                "id": "SystemTime 0x1082",
                "value": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        ]
    });
    patch_json(client, &url, &payload, "note_last_action").await
}

pub async fn set_flag(client: &Client<'_>, id: &str, flagged: bool) -> Result<()> {
    let url = format!("{BASE}/me/messages/{id}");
    let payload = serde_json::json!({
        "flag": { "flagStatus": if flagged { "flagged" } else { "notFlagged" } }
    });
    patch_json(client, &url, &payload, "set_flag").await
}

pub async fn mark_read(client: &Client<'_>, id: &str, read: bool) -> Result<()> {
    let url = format!("{BASE}/me/messages/{id}");
    let payload = serde_json::json!({ "isRead": read });
    patch_json(client, &url, &payload, "mark_read").await
}

/// Renders a parsed query into the value of Graph's `$search`.
///
/// **The whole expression is wrapped in double quotes and the KQL lives
/// inside**: Graph expects `$search="from:alice"`, not `$search=from:alice`.
/// An unwrapped value is parsed as a bare literal and the colon is rejected
/// outright — `Syntax error: character ':' is not valid at position 4`. Which
/// means no inner quotes are available either: a multi-word value has to be
/// grouped with KQL parentheses (`subject:(bon AND de AND commande)`) rather
/// than quoted.
///
/// Graph also rejects `$search` combined with `$filter`, so date bounds cannot
/// be expressed here — KQL's `received:` range syntax is not honoured on
/// `/me/messages`. They are left to the caller's local filter, which is why the
/// runtime re-checks every result.
///
/// Returns an empty string when nothing textual can be sent, since an empty
/// `$search` is itself an error.
fn search_expression(query: &SearchQuery) -> String {
    /// One KQL operand, with everything that could reopen the enclosing quotes
    /// or start a nested clause removed.
    fn operand(value: &str) -> Option<String> {
        let words: Vec<String> = value
            .split_whitespace()
            .map(|word| {
                word.chars()
                    .filter(|c| !matches!(c, '"' | '(' | ')' | ':'))
                    .collect::<String>()
            })
            .filter(|word| !word.is_empty())
            .collect();
        match words.len() {
            0 => None,
            1 => Some(words.into_iter().next().expect("one word")),
            // No inner quotes available inside `$search="…"`, so a phrase
            // becomes a parenthesized conjunction.
            _ => Some(format!("({})", words.join(" AND "))),
        }
    }

    let mut clauses: Vec<String> = Vec::new();
    for (field, terms) in [
        ("from", &query.from),
        ("to", &query.to),
        ("subject", &query.subject),
    ] {
        for term in terms {
            if let Some(operand) = operand(term) {
                clauses.push(format!("{field}:{operand}"));
            }
        }
    }
    if query.has_attachment == Some(true) {
        clauses.push("hasAttachment:true".into());
    }
    if let Some(is_read) = query.is_read {
        clauses.push(format!("isRead:{is_read}"));
    }
    for term in &query.terms {
        if let Some(operand) = operand(term) {
            clauses.push(operand);
        }
    }
    if clauses.is_empty() {
        return String::new();
    }
    // KQL joins with an implicit AND, but spelling it out keeps a
    // parenthesized value from being read as alternatives.
    format!("\"{}\"", clauses.join(" AND "))
}

pub async fn search(
    client: &Client<'_>,
    query: &SearchQuery,
    folder_id: Option<Option<&str>>,
    limit: usize,
) -> Result<Vec<MessageHeader>> {
    // Graph scopes a search by addressing the folder's own message
    // collection; `inbox` is a well-known name it resolves itself.
    let url = match folder_id {
        Some(folder) => format!(
            "{BASE}/me/mailFolders/{}/messages",
            folder.unwrap_or("inbox")
        ),
        None => format!("{BASE}/me/messages"),
    };
    let search_value = search_expression(query);
    if search_value.is_empty() {
        return Ok(Vec::new());
    }
    let limit_str = limit.to_string();
    let resp = client
        .get(&url)
        .query(&[
            ("$search", search_value.as_str()),
            ("$top", limit_str.as_str()),
            ("$select", MESSAGE_SELECT),
            ("$expand", MESSAGE_EXPAND),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph search failed").await);
    }
    let list: GraphList<GraphMessage> = resp.json().await?;
    Ok(list.value.into_iter().map(Into::into).collect())
}

pub async fn list_from_sender(
    client: &Client<'_>,
    email: &str,
    top: usize,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    let search_value = format!("\"from:{}\"", email.replace('"', " "));
    let top_str = top.to_string();
    let url = format!("{BASE}/me/messages");
    let main_fut = client
        .get(&url)
        .query(&[
            ("$search", search_value.as_str()),
            ("$top", top_str.as_str()),
            ("$select", MESSAGE_SELECT),
            ("$expand", MESSAGE_EXPAND),
        ])
        .send();

    // $search on /me/messages follows the user's Outlook search scope, which
    // excludes the Deleted Items folder by default. Query it explicitly so the
    // sender history also surfaces deleted messages. We use $search (not
    // $filter) because Graph rejects from/emailAddress/address as
    // "InefficientFilter".
    let deleted_url = format!("{BASE}/me/mailFolders/deleteditems/messages");
    let deleted_fut = client
        .get(&deleted_url)
        .query(&[
            ("$search", search_value.as_str()),
            ("$top", top_str.as_str()),
            ("$select", MESSAGE_SELECT),
            ("$expand", MESSAGE_EXPAND),
        ])
        .send();

    let (main_resp, deleted_resp) = tokio::join!(main_fut, deleted_fut);

    let main_resp = main_resp?;
    if !main_resp.status().is_success() {
        return Err(crate::providers::http_error(main_resp, "graph from-sender failed").await);
    }
    let main_list: GraphList<GraphMessage> = main_resp.json().await?;
    let next_link = main_list.next_link;
    let mut out: Vec<MessageHeader> = main_list.value.into_iter().map(Into::into).collect();

    let deleted = fetch_deleted_from_sender(deleted_resp).await;
    if !deleted.is_empty() {
        let seen: std::collections::HashSet<String> = out.iter().map(|m| m.id.clone()).collect();
        for h in deleted {
            if !seen.contains(&h.id) {
                out.push(h);
            }
        }
    }

    out.sort_by_key(|m| std::cmp::Reverse(m.received));
    Ok((out, next_link))
}

async fn fetch_deleted_from_sender(resp: reqwest::Result<reqwest::Response>) -> Vec<MessageHeader> {
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log::warn!("from-sender deleted items request failed: {e:#}");
            return Vec::new();
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        log::warn!("from-sender deleted items failed ({status}): {body}");
        return Vec::new();
    }
    match resp.json::<GraphList<GraphMessage>>().await {
        Ok(list) => list.value.into_iter().map(Into::into).collect(),
        Err(e) => {
            log::warn!("from-sender deleted items parse failed: {e:#}");
            Vec::new()
        }
    }
}

pub async fn fetch_messages_page(
    client: &Client<'_>,
    url: &str,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph paged fetch failed").await);
    }
    let list: GraphList<GraphMessage> = resp.json().await?;
    let mut out: Vec<MessageHeader> = list.value.into_iter().map(Into::into).collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.received));
    Ok((out, list.next_link))
}

pub async fn list_thread(client: &Client<'_>, conversation_id: &str) -> Result<Vec<MessageHeader>> {
    let escaped = conversation_id.replace('\'', "''");
    let filter = format!("conversationId eq '{escaped}'");
    let url = format!("{BASE}/me/messages");
    let resp = client
        .get(&url)
        .query(&[
            ("$filter", filter.as_str()),
            ("$top", "50"),
            ("$select", MESSAGE_SELECT),
            ("$expand", MESSAGE_EXPAND),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph thread failed").await);
    }
    let list: GraphList<GraphMessage> = resp.json().await?;
    let mut out: Vec<MessageHeader> = list.value.into_iter().map(Into::into).collect();
    out.sort_by_key(|m| m.received);
    Ok(out)
}

fn drop_unresolved_cid_images(md: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"!\[[^\]]*\]\(cid:[^)]*\)").unwrap());
    re.replace_all(md, "_[image inline manquante]_")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{search_expression, GraphMessage, MESSAGE_SELECT};
    use crate::model::MessageHeader;
    use crate::search_query::SearchQuery;

    /// Graph only returns the fields `$select` asks for. Dropping
    /// `conversationId` from the listing would silently ungroup every mailbox
    /// without breaking a single type.
    #[test]
    fn listing_select_carries_the_conversation_id() {
        assert!(
            MESSAGE_SELECT
                .split(',')
                .any(|field| field == "conversationId"),
            "{MESSAGE_SELECT}"
        );

        let message: GraphMessage = serde_json::from_str(
            r#"{
                "id":"message-1",
                "receivedDateTime":"2026-03-15T12:00:00Z",
                "isRead":false,
                "conversationId":"conversation-1"
            }"#,
        )
        .expect("synthetic Graph message");
        let header: MessageHeader = message.into();
        assert_eq!(header.conversation_id.as_deref(), Some("conversation-1"));
    }

    /// Graph wants `$search="from:alice"`: the quotes wrap the *whole* value
    /// and the KQL lives inside. Sending `from:"alice"` instead is rejected
    /// with `Syntax error: character ':' is not valid at position 4`, so the
    /// shape of the string is the contract — not just its parts.
    #[test]
    fn search_expression_wraps_the_whole_kql_in_quotes() {
        let rendered = search_expression(&SearchQuery::parse("de:alice@example.test"));
        assert_eq!(rendered, "\"from:alice@example.test\"");

        let rendered = search_expression(&SearchQuery::parse(
            "de:alice objet:contrat avec:pj est:non-lu facture",
        ));
        assert!(
            rendered.starts_with('"') && rendered.ends_with('"'),
            "{rendered}"
        );
        // Any inner quote would close the wrapper and break the query.
        assert_eq!(rendered.matches('"').count(), 2, "{rendered}");
        assert!(rendered.contains("from:alice"), "{rendered}");
        assert!(rendered.contains("subject:contrat"), "{rendered}");
        assert!(rendered.contains("hasAttachment:true"), "{rendered}");
        assert!(rendered.contains("isRead:false"), "{rendered}");
        assert!(rendered.contains("facture"), "{rendered}");
        assert!(rendered.contains(" AND "), "{rendered}");
    }

    /// With no inner quotes available, a phrase has to be grouped with KQL
    /// parentheses instead.
    #[test]
    fn multi_word_values_are_grouped_not_quoted() {
        let rendered = search_expression(&SearchQuery::parse("objet:\"bon de commande\""));
        assert_eq!(rendered, "\"subject:(bon AND de AND commande)\"");
    }

    /// A colon inside a value would terminate the operand and produce the very
    /// syntax error this function exists to avoid.
    #[test]
    fn syntax_characters_are_stripped_from_values() {
        let rendered = search_expression(&SearchQuery::parse("de:\"a:b(c)\""));
        assert_eq!(rendered, "\"from:abc\"");
    }

    /// Graph rejects `$search` combined with `$filter`, so dates cannot be
    /// expressed here. They must be silently absent — the runtime re-applies
    /// them locally — rather than emitted as KQL the endpoint ignores.
    #[test]
    fn date_bounds_are_not_emitted() {
        let rendered = search_expression(&SearchQuery::parse("avant:2026-03-15 facture"));
        assert_eq!(rendered, "\"facture\"");
        assert_eq!(rendered.matches('"').count(), 2, "{rendered}");
    }

    /// A query with nothing textual would produce an empty `$search`, which
    /// Graph rejects; the caller checks for it.
    #[test]
    fn flag_only_query_renders_nothing_textual() {
        assert!(search_expression(&SearchQuery::parse("avant:2026-03-15")).is_empty());
    }
}
