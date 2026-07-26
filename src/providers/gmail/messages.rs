use super::{check_status, BASE};
use crate::model::{
    AccountId, Attachment, BodyFormat, CalendarInvitation, InlineImage, Message, MessageHeader,
};
use crate::providers::html::{collapse_blank_lines, convert_email_html};
use crate::providers::{MailSyncPage, MessagePage};
use crate::search_query::SearchQuery;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Deserialize)]
struct GmailListResponse {
    #[serde(default)]
    messages: Vec<GmailIdRef>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GmailIdRef {
    id: String,
    #[serde(rename = "threadId", default)]
    #[allow(dead_code)]
    thread_id: String,
}

#[derive(Deserialize)]
struct GmailProfile {
    #[serde(rename = "historyId")]
    history_id: String,
}

#[derive(Deserialize, Default)]
struct GmailHistoryPage {
    #[serde(default)]
    history: Vec<GmailHistory>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "historyId")]
    history_id: String,
}

#[derive(Deserialize, Default)]
struct GmailHistory {
    #[serde(default, rename = "messagesAdded")]
    messages_added: Vec<GmailHistoryMessage>,
    #[serde(default, rename = "messagesDeleted")]
    messages_deleted: Vec<GmailHistoryMessage>,
    #[serde(default, rename = "labelsAdded")]
    labels_added: Vec<GmailHistoryMessage>,
    #[serde(default, rename = "labelsRemoved")]
    labels_removed: Vec<GmailHistoryMessage>,
}

#[derive(Deserialize)]
struct GmailHistoryMessage {
    message: GmailIdRef,
    #[serde(default, rename = "labelIds")]
    label_ids: Vec<String>,
}

#[derive(Deserialize)]
struct GmailMessage {
    id: String,
    #[serde(rename = "threadId", default)]
    thread_id: Option<String>,
    #[serde(default)]
    snippet: String,
    #[serde(rename = "labelIds", default)]
    label_ids: Vec<String>,
    #[serde(rename = "internalDate", default)]
    internal_date: Option<String>,
    payload: Option<GmailPayload>,
}

#[derive(Deserialize)]
struct GmailPayload {
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(default)]
    headers: Vec<GmailHeader>,
    #[serde(default)]
    parts: Vec<GmailPayload>,
    body: Option<GmailBody>,
    #[serde(default)]
    filename: String,
}

#[derive(Deserialize)]
struct GmailHeader {
    name: String,
    value: String,
}

#[derive(Deserialize)]
struct GmailBody {
    #[serde(default)]
    data: Option<String>,
    #[serde(rename = "attachmentId", default)]
    attachment_id: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct AttachmentBody {
    #[serde(default)]
    data: Option<String>,
}

fn header(payload: &GmailPayload, name: &str) -> Option<String> {
    payload
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| unfold_header_value(&h.value))
}

/// Gmail can preserve RFC 5322 folding (`CRLF` + whitespace) in metadata
/// values. Those control characters are not displayable and cosmic-text may
/// incorrectly route `CR` through the color-emoji font, producing one raster
/// error per frame. Header folding has no semantic whitespace beyond one
/// separator, so normalize every run at the provider boundary.
fn unfold_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_internal_date(raw: Option<&str>) -> DateTime<Utc> {
    let ms: i64 = raw.and_then(|s| s.parse().ok()).unwrap_or(0);
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

fn message_header_from(m: &GmailMessage) -> MessageHeader {
    let payload = m.payload.as_ref();
    let subject = payload
        .and_then(|p| header(p, "Subject"))
        .unwrap_or_default();
    let from = payload.and_then(|p| header(p, "From")).unwrap_or_default();
    // User-created labels are exactly the ones whose id starts with `Label_`
    // — every system label (INBOX, UNREAD, STARRED, CATEGORY_*, CHAT, …)
    // uses an all-caps name. Same partition `list_tags` applies, kept in
    // sync so a label that surfaces as a Tag also surfaces here as a tag id.
    let tags: Vec<String> = m
        .label_ids
        .iter()
        .filter(|l| l.starts_with("Label_"))
        .cloned()
        .collect();
    MessageHeader {
        id: m.id.clone(),
        account_id: AccountId::default(),
        subject,
        from,
        received: parse_internal_date(m.internal_date.as_deref()),
        preview: m.snippet.clone(),
        is_read: !m.label_ids.iter().any(|l| l == "UNREAD"),
        is_flagged: m.label_ids.iter().any(|l| l == "STARRED"),
        // Gmail's `format=metadata` doesn't expose payload parts, so we
        // can't tell from this response alone. The list path runs a parallel
        // `q=has:attachment` query and patches this flag afterwards.
        has_attachments: false,
        tags,
        // L'API Gmail n'expose ni \Answered ni $Forwarded — pas d'info
        // replied/forwarded status for this provider.
        last_action: None,
        last_action_at: None,
        conversation_id: m.thread_id.clone().filter(|id| !id.is_empty()),
        // Gmail stores one message per mail and models folders as labels,
        // so `threads.get` never returns two copies of the same thing —
        // there is nothing here to deduplicate, and the listing does not
        // pay for an extra `metadataHeaders=Message-Id`.
        internet_message_id: None,
    }
}

struct GmailMetadata {
    header: MessageHeader,
    label_ids: Vec<String>,
}

impl GmailMetadata {
    fn from_message(message: GmailMessage) -> Self {
        let header = message_header_from(&message);
        Self {
            header,
            label_ids: message.label_ids,
        }
    }
}

fn labels_belong_to_folder(label_ids: &[String], label: &str) -> bool {
    label_ids.iter().any(|id| id == label)
        && (!label.starts_with("CATEGORY_") || label_ids.iter().any(|id| id == "INBOX"))
}

/// Category labels persist when a message is archived. Gmail's inbox tabs are
/// therefore the intersection of `INBOX` and `CATEGORY_*`, not the category
/// label by itself.
fn with_folder_labels(request: reqwest::RequestBuilder, label: &str) -> reqwest::RequestBuilder {
    let request = if label.starts_with("CATEGORY_") {
        request.query(&[("labelIds", "INBOX")])
    } else {
        request
    };
    request.query(&[("labelIds", label)])
}

/// Run a `messages.list?q=has:attachment` lookup scoped to the same label
/// (or unbounded for search). Returns the set of message IDs Gmail flags as
/// having attachments, so the caller can mark its `MessageHeader`s.
async fn fetch_has_attachment_ids(
    client: &reqwest::Client,
    access_token: &str,
    label: Option<&str>,
    extra_query: Option<&str>,
    max_results: usize,
) -> HashSet<String> {
    let q = match extra_query {
        Some(q) => format!("({q}) has:attachment"),
        None => "has:attachment".to_string(),
    };
    let max_s = max_results.to_string();
    let request = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("q", q.as_str()), ("maxResults", max_s.as_str())]);
    let request = match label {
        Some(label) => with_folder_labels(request, label),
        None => request,
    };
    let resp = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("gmail has:attachment list failed: {e:#}");
            return HashSet::new();
        }
    };
    if !resp.status().is_success() {
        log::warn!(
            "gmail has:attachment list status {} ({:?})",
            resp.status(),
            resp.text().await.ok()
        );
        return HashSet::new();
    }
    match resp.json::<GmailListResponse>().await {
        Ok(body) => body.messages.into_iter().map(|m| m.id).collect(),
        Err(e) => {
            log::warn!("gmail has:attachment parse failed: {e:#}");
            HashSet::new()
        }
    }
}

pub async fn list_folder_messages(
    client: &reqwest::Client,
    access_token: &str,
    folder_id: Option<&str>,
    top: usize,
    skip: usize,
) -> Result<Vec<MessageHeader>> {
    // Gmail doesn't support `skip` natively; if the caller asked to skip,
    // walk forward through `nextPageToken` until we reach the right offset.
    let label = folder_id.unwrap_or("INBOX");
    let mut page_token: Option<String> = None;
    let mut remaining_skip = skip;
    loop {
        let want = remaining_skip + top;
        let want_s = want.to_string();
        let req = client
            .get(format!("{BASE}/users/me/messages"))
            .bearer_auth(access_token)
            .query(&[("maxResults", want_s.as_str())]);
        let mut req = with_folder_labels(req, label);
        if let Some(tok) = &page_token {
            req = req.query(&[("pageToken", tok.as_str())]);
        }
        let resp = req.send().await?;
        let resp = check_status(resp, "list").await?;
        let body: GmailListResponse = resp.json().await?;
        let total = body.messages.len();
        if total <= remaining_skip {
            // Whole page fits in skip window.
            remaining_skip -= total;
            match body.next_page_token {
                Some(tok) => page_token = Some(tok),
                _ => return Ok(Vec::new()),
            }
            continue;
        }
        let take_ids: Vec<String> = body
            .messages
            .into_iter()
            .skip(remaining_skip)
            .take(top)
            .map(|m| m.id)
            .collect();
        // Run the per-id metadata fetches and the `has:attachment` index
        // lookup in parallel — neither depends on the other and we need
        // both to flag the rows.
        let (headers_result, attach_ids) = tokio::join!(
            fetch_metadata_batch(client, access_token, take_ids),
            fetch_has_attachment_ids(client, access_token, Some(label), None, top.max(50)),
        );
        let mut headers = headers_result?;
        for h in &mut headers {
            if attach_ids.contains(&h.id) {
                h.has_attachments = true;
            }
        }
        return Ok(headers);
    }
}

pub async fn list_folder_messages_page(
    client: &reqwest::Client,
    access_token: &str,
    folder_id: Option<&str>,
    top: usize,
) -> Result<MessagePage> {
    let label = folder_id.unwrap_or("INBOX");
    let top_s = top.to_string();
    let request = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("maxResults", top_s.as_str())]);
    let resp = with_folder_labels(request, label).send().await?;
    let resp = check_status(resp, "list").await?;
    let body: GmailListResponse = resp.json().await?;
    let ids: Vec<String> = body
        .messages
        .into_iter()
        .map(|message| message.id)
        .collect();
    let (headers_result, attach_ids) = tokio::join!(
        fetch_metadata_batch(client, access_token, ids),
        fetch_has_attachment_ids(client, access_token, Some(label), None, top.max(50)),
    );
    let mut messages = headers_result?;
    for message in &mut messages {
        if attach_ids.contains(&message.id) {
            message.has_attachments = true;
        }
    }
    Ok(MessagePage {
        messages,
        next: body.next_page_token.map(|token| format!("{label}|{token}")),
    })
}

/// Uses Gmail's global history. The first call only captures the
/// `historyId`; subsequent refreshes retrieve affected identifiers and reuse
/// the existing metadata batch.
pub async fn sync_folder_messages(
    client: &reqwest::Client,
    access_token: &str,
    folder_id: Option<&str>,
    cursor_or_page: Option<&str>,
) -> Result<MailSyncPage> {
    let Some(cursor_or_page) = cursor_or_page else {
        let resp = client
            .get(format!("{BASE}/users/me/profile"))
            .bearer_auth(access_token)
            .send()
            .await?;
        let resp = check_status(resp, "profile historyId").await?;
        let profile: GmailProfile = resp.json().await?;
        return Ok(MailSyncPage {
            cursor: Some(profile.history_id),
            ..MailSyncPage::default()
        });
    };

    let (start, page_token) = cursor_or_page
        .split_once('|')
        .map_or((cursor_or_page, None), |(start, page)| (start, Some(page)));
    let label = folder_id.unwrap_or("INBOX");
    let category_folder = label.starts_with("CATEGORY_");
    let mut request = client
        .get(format!("{BASE}/users/me/history"))
        .bearer_auth(access_token)
        .query(&[
            ("startHistoryId", start),
            ("labelId", label),
            ("maxResults", "500"),
        ]);
    if let Some(page_token) = page_token {
        request = request.query(&[("pageToken", page_token)]);
    }
    let resp = request.send().await?;
    let resp = check_status(resp, "history").await?;
    let page: GmailHistoryPage = resp.json().await?;
    let mut affected = HashSet::new();
    let mut deleted = HashSet::new();
    let mut removed_from_folder = HashSet::new();
    for history in page.history {
        for entry in history.messages_added {
            affected.insert(entry.message.id);
        }
        for entry in history.labels_added {
            affected.insert(entry.message.id);
        }
        for entry in history.labels_removed {
            if entry
                .label_ids
                .iter()
                .any(|removed| removed == label || (category_folder && removed == "INBOX"))
            {
                removed_from_folder.insert(entry.message.id);
            } else {
                affected.insert(entry.message.id);
            }
        }
        for entry in history.messages_deleted {
            deleted.insert(entry.message.id);
        }
    }
    for id in &deleted {
        affected.remove(id);
    }
    for id in &removed_from_folder {
        affected.remove(id);
    }
    let metadata =
        fetch_folder_metadata_batch(client, access_token, affected.into_iter().collect(), label)
            .await?;
    removed_from_folder.extend(metadata.outside_folder);
    let next = page.next_page_token.map(|token| format!("{start}|{token}"));
    Ok(MailSyncPage {
        upserts: metadata.headers,
        deleted: deleted.into_iter().collect(),
        removed_from_folder: removed_from_folder.into_iter().collect(),
        cursor: next.is_none().then_some(page.history_id),
        next,
    })
}

pub async fn fetch_messages_page(
    client: &reqwest::Client,
    access_token: &str,
    next_link: &str,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    if let Some(search_cursor) = next_link.strip_prefix("__SEARCH__|") {
        let (query, token) = search_cursor
            .rsplit_once('|')
            .ok_or_else(|| anyhow!("malformed Gmail search nextLink: {next_link}"))?;
        let request = client
            .get(format!("{BASE}/users/me/messages"))
            .bearer_auth(access_token)
            .query(&[
                ("q", query),
                ("maxResults", "50"),
                ("pageToken", token),
                ("includeSpamTrash", "true"),
            ]);
        let resp = request.send().await?;
        let resp = check_status(resp, "fetch_messages_page(search)").await?;
        let body: GmailListResponse = resp.json().await?;
        let ids: Vec<String> = body.messages.into_iter().map(|m| m.id).collect();
        let next = body
            .next_page_token
            .map(|next_token| format!("__SEARCH__|{query}|{next_token}"));
        let headers = fetch_metadata_batch(client, access_token, ids).await?;
        return Ok((headers, next));
    }

    // We treat `next_link` as an opaque "label|pageToken" pair so callers
    // can handle Microsoft's URL-style continuation tokens uniformly.
    let (label, token) = next_link
        .split_once('|')
        .ok_or_else(|| anyhow!("malformed Gmail nextLink: {next_link}"))?;
    let request = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("maxResults", "50"), ("pageToken", token)]);
    let resp = with_folder_labels(request, label).send().await?;
    let resp = check_status(resp, "fetch_messages_page").await?;
    let body: GmailListResponse = resp.json().await?;
    let ids: Vec<String> = body.messages.into_iter().map(|m| m.id).collect();
    let next = body.next_page_token.map(|t| format!("{label}|{t}"));
    let (headers_result, attach_ids) = tokio::join!(
        fetch_metadata_batch(client, access_token, ids),
        fetch_has_attachment_ids(client, access_token, Some(label), None, 50),
    );
    let mut headers = headers_result?;
    for h in &mut headers {
        if attach_ids.contains(&h.id) {
            h.has_attachments = true;
        }
    }
    Ok((headers, next))
}

/// Public wrapper accepting a borrowed slice; used by the tags listing.
/// Cheap clone of each id is fine — the Vec gets consumed by spawn anyway.
pub(super) async fn fetch_metadata_batch_pub(
    client: &reqwest::Client,
    access_token: &str,
    ids: &[String],
) -> Result<Vec<MessageHeader>> {
    fetch_metadata_batch(client, access_token, ids.to_vec()).await
}

async fn fetch_metadata_batch(
    client: &reqwest::Client,
    access_token: &str,
    ids: Vec<String>,
) -> Result<Vec<MessageHeader>> {
    Ok(fetch_metadata(client, access_token, ids)
        .await?
        .into_iter()
        .map(|metadata| metadata.header)
        .collect())
}

struct FolderMetadataBatch {
    headers: Vec<MessageHeader>,
    outside_folder: Vec<String>,
}

async fn fetch_folder_metadata_batch(
    client: &reqwest::Client,
    access_token: &str,
    ids: Vec<String>,
    label: &str,
) -> Result<FolderMetadataBatch> {
    let metadata = fetch_metadata(client, access_token, ids).await?;
    let mut headers = Vec::with_capacity(metadata.len());
    let mut outside_folder = Vec::new();
    for metadata in metadata {
        if labels_belong_to_folder(&metadata.label_ids, label) {
            headers.push(metadata.header);
        } else {
            outside_folder.push(metadata.header.id);
        }
    }
    Ok(FolderMetadataBatch {
        headers,
        outside_folder,
    })
}

async fn fetch_metadata(
    client: &reqwest::Client,
    access_token: &str,
    ids: Vec<String>,
) -> Result<Vec<GmailMetadata>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Gmail counts every inner request against both quota and per-user
    // concurrency. Small batches retain most of the connection saving without
    // presenting 50 simultaneous `messages.get`s to the mailbox backend.
    const BATCH_CHUNK: usize = 10;
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(BATCH_CHUNK) {
        match batch_get_metadata(client, access_token, chunk).await {
            Ok(mut batch) => {
                out.append(&mut batch.metadata);
                // A successful outer HTTP response may still contain 429/5xx
                // sub-responses. Retry only those ids, sequentially, with the
                // same backoff as a normal metadata request.
                for id in batch.retry_ids {
                    match fetch_one_metadata(client, access_token, &id).await? {
                        Some(header) => out.push(header),
                        None => log::debug!("gmail message {id} disappeared during listing"),
                    }
                }
            }
            Err(e) => {
                // Fall back to sequential per-id fetches on batch endpoint
                // failure. Do not turn a partial page into a successful one:
                // any persistent error bubbles up so the cache stays intact.
                log::warn!("gmail batch metadata failed, falling back: {e:#}");
                for id in chunk {
                    match fetch_one_metadata(client, access_token, id).await? {
                        Some(header) => out.push(header),
                        None => log::debug!("gmail message {id} disappeared during listing"),
                    }
                }
            }
        }
    }
    out.sort_by_key(|metadata| std::cmp::Reverse(metadata.header.received));
    Ok(out)
}

#[derive(Default)]
struct BatchMetadataResult {
    metadata: Vec<GmailMetadata>,
    retry_ids: Vec<String>,
}

/// Send one Gmail batch request containing a deliberately small group of
/// `messages.get?format=metadata` calls. Gmail returns response parts in the
/// request order, which lets the parser associate a failed part with its id.
async fn batch_get_metadata(
    client: &reqwest::Client,
    access_token: &str,
    ids: &[String],
) -> Result<BatchMetadataResult> {
    let boundary = format!(
        "aviary_batch_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let part_delim = format!("--{boundary}\r\n");
    let close_delim = format!("--{boundary}--\r\n");
    let mut body = String::with_capacity(ids.len() * 200);
    for id in ids {
        body.push_str(&part_delim);
        body.push_str("Content-Type: application/http\r\n");
        body.push_str("Content-ID: <");
        body.push_str(id);
        body.push_str(">\r\n\r\n");
        body.push_str("GET /gmail/v1/users/me/messages/");
        body.push_str(id);
        body.push_str("?format=metadata&metadataHeaders=Subject&metadataHeaders=From\r\n\r\n");
    }
    body.push_str(&close_delim);

    let resp = client
        .post("https://gmail.googleapis.com/batch/gmail/v1")
        .bearer_auth(access_token)
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/mixed; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    let resp_ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        return Err(crate::providers::http_error(resp, "gmail batch endpoint failed").await);
    }
    let resp_boundary = resp_ct
        .as_deref()
        .and_then(parse_multipart_boundary)
        .ok_or_else(|| anyhow!("gmail batch response missing boundary"))?;
    let bytes = resp.bytes().await?;
    parse_batch_metadata_response(&bytes, &resp_boundary, ids)
}

fn parse_multipart_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';').map(str::trim) {
        if let Some(b) = part.strip_prefix("boundary=") {
            return Some(b.trim_matches('"').to_string());
        }
    }
    None
}

/// Walk a `multipart/mixed` response body and decode each part's embedded HTTP
/// response. Retryable sub-errors are returned to the caller instead of being
/// silently discarded. Each part has the shape:
/// `<MIME headers>\r\n\r\n<status line>\r\n<inner headers>\r\n\r\n<body>`.
fn parse_batch_metadata_response(
    body: &[u8],
    boundary: &str,
    ids: &[String],
) -> Result<BatchMetadataResult> {
    let mut out = BatchMetadataResult::default();
    let initial = format!("--{boundary}");
    let inter = format!("\r\n--{boundary}");
    let Some(first) = find_subslice(body, initial.as_bytes()) else {
        bail!("gmail batch response has no opening boundary");
    };
    let mut cursor = first + initial.len();
    let mut part_index = 0;
    loop {
        // After a boundary marker we expect either "\r\n" (next part) or
        // "--\r\n" (closing boundary, end of stream).
        if body.get(cursor..cursor + 2) == Some(b"--") {
            break;
        }
        // Skip past the line terminator that follows the boundary.
        let part_start = cursor
            + body[cursor..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(0);
        let part_end = match find_subslice(&body[part_start..], inter.as_bytes()) {
            Some(i) => part_start + i,
            None => bail!("gmail batch response has an unterminated part"),
        };
        let id = ids
            .get(part_index)
            .ok_or_else(|| anyhow!("gmail batch returned more parts than requested"))?;
        let (status, inner_body) = parse_inner_http(&body[part_start..part_end])
            .ok_or_else(|| anyhow!("gmail batch part {part_index} is malformed"))?;
        let trimmed = trim_trailing_crlf(inner_body);
        if (200..300).contains(&status) {
            let message: GmailMessage = serde_json::from_slice(trimmed)
                .map_err(|error| anyhow!("gmail batch metadata {id} is invalid: {error:#}"))?;
            out.metadata.push(GmailMetadata::from_message(message));
        } else if status == 404 {
            // The message may have been deleted between messages.list and
            // messages.get. This is a normal race and should not fail a page.
            log::debug!("gmail message {id} disappeared during batch listing");
        } else if is_retryable_gmail_error(status, trimmed) {
            log::warn!("gmail batch sub-status {status} for {id}; retrying");
            out.retry_ids.push(id.clone());
        } else {
            let error_body = String::from_utf8_lossy(trimmed);
            return Err(sub_request_error(
                status,
                format!("gmail batch metadata {id} failed ({status}): {error_body}"),
            ));
        }
        part_index += 1;
        cursor = part_end + inter.len();
    }
    if part_index != ids.len() {
        bail!(
            "gmail batch returned {part_index} parts for {} requests",
            ids.len()
        );
    }
    Ok(out)
}

fn parse_inner_http(part: &[u8]) -> Option<(u16, &[u8])> {
    // part = mime_headers \r\n\r\n <inner http response>
    let after_mime = find_subslice(part, b"\r\n\r\n")? + 4;
    let inner = &part[after_mime..];
    // status line: "HTTP/1.1 <code> <reason>\r\n"
    let status_end = find_subslice(inner, b"\r\n")?;
    let status_line = std::str::from_utf8(&inner[..status_end]).ok()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let after_status = &inner[status_end + 2..];
    let after_headers = find_subslice(after_status, b"\r\n\r\n")? + 4;
    Some((status, &after_status[after_headers..]))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trim_trailing_crlf(s: &[u8]) -> &[u8] {
    let mut end = s.len();
    while end > 0 && (s[end - 1] == b'\n' || s[end - 1] == b'\r') {
        end -= 1;
    }
    &s[..end]
}

/// Wraps a sub-request failure so the retry policy sees its status.
///
/// Gmail's batch endpoint answers `200` and reports per-message outcomes inside
/// the multipart body, so the status never reaches us through a `Response`.
fn sub_request_error(status: u16, message: String) -> anyhow::Error {
    match reqwest::StatusCode::from_u16(status) {
        Ok(status) => crate::providers::error::ProviderError::new(status, message).into(),
        Err(_) => anyhow::anyhow!(message),
    }
}

fn is_retryable_gmail_error(status: u16, body: &[u8]) -> bool {
    status == 429
        || (500..600).contains(&status)
        || (status == 403
            && [
                b"rateLimitExceeded".as_slice(),
                b"userRateLimitExceeded".as_slice(),
            ]
            .iter()
            .any(|reason| find_subslice(body, reason).is_some()))
}

fn metadata_retry_delay(attempt: u32) -> std::time::Duration {
    let base_seconds = 1u64 << attempt.min(5);
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()) % 1_001)
        .unwrap_or(0);
    std::time::Duration::from_millis(base_seconds * 1_000 + jitter_ms)
}

async fn fetch_one_metadata(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
) -> Result<Option<GmailMetadata>> {
    // Gmail recommends truncated exponential backoff starting at one second
    // plus jitter. Retry both modern 429s and legacy 403 rate-limit reasons.
    let mut attempt: u32 = 0;
    loop {
        let resp = client
            .get(format!("{BASE}/users/me/messages/{id}"))
            .bearer_auth(access_token)
            .query(&[
                ("format", "metadata"),
                ("metadataHeaders", "Subject"),
                ("metadataHeaders", "From"),
            ])
            .send()
            .await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            let m: GmailMessage = resp.json().await?;
            return Ok(Some(GmailMetadata::from_message(m)));
        }
        let body = resp.bytes().await.unwrap_or_default();
        if status == 404 {
            return Ok(None);
        }
        if is_retryable_gmail_error(status, &body) && attempt < 4 {
            let delay = metadata_retry_delay(attempt);
            attempt += 1;
            log::warn!(
                "gmail metadata {id} status {status}; retry {attempt}/4 in {:.1}s",
                delay.as_secs_f32()
            );
            tokio::time::sleep(delay).await;
            continue;
        }
        let error_body = String::from_utf8_lossy(&body);
        return Err(sub_request_error(
            status,
            format!("gmail metadata {id} failed ({status}): {error_body}"),
        ));
    }
}

pub async fn get_message(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
) -> Result<Message> {
    // Opening a message is the most latency-sensitive provider call, and every
    // stage below lands in the single `fetch_ms` the caller reports. Break it
    // down here so a slow open can be attributed without guessing: the two
    // round trips are necessarily sequential (attachment ids only exist in the
    // payload response), so knowing which one dominates decides what to fix.
    let started = std::time::Instant::now();
    let resp = client
        .get(format!("{BASE}/users/me/messages/{id}"))
        .bearer_auth(access_token)
        .query(&[("format", "full")])
        .send()
        .await?;
    let resp = check_status(resp, "get").await?;
    let m: GmailMessage = resp.json().await?;
    let payload_elapsed = started.elapsed();
    let mut header = message_header_from(&m);
    // Surface every label as a tag candidate; the UI cross-references this
    // against `Tag::list_tags` (user labels only) so system labels like
    // `UNREAD`/`STARRED`/`CATEGORY_*` filter themselves out.
    let tags = m.label_ids.clone();
    let is_draft = m.label_ids.iter().any(|l| l == "DRAFT");
    let (to, cc, bcc) = m
        .payload
        .as_ref()
        .map(|p| {
            (
                parse_address_header(p, "To"),
                parse_address_header(p, "Cc"),
                parse_address_header(p, "Bcc"),
            )
        })
        .unwrap_or_default();

    let mut html_body: Option<String> = None;
    let mut text_body: Option<String> = None;
    let mut inline_images: Vec<(String, String, String)> = Vec::new(); // (cid, mime, attachment_id)
    let mut file_refs: Vec<FileRef> = Vec::new();
    let mut calendar_refs: Vec<CalendarPartRef> = Vec::new();
    if let Some(p) = &m.payload {
        walk_payload(
            p,
            &mut html_body,
            &mut text_body,
            &mut inline_images,
            &mut file_refs,
            &mut calendar_refs,
        );
    }
    header.has_attachments = !file_refs.is_empty();

    let inline_requested = inline_images.len();
    let mut inline_elapsed = std::time::Duration::ZERO;
    let mut convert_elapsed = std::time::Duration::ZERO;
    let (content, format, inline, raw_body) = if let Some(html) = html_body {
        let inline_started = std::time::Instant::now();
        let resolved = fetch_inline_images(client, access_token, id, inline_images).await;
        inline_elapsed = inline_started.elapsed();
        let raw_html = html;
        let html = raw_html.clone();
        let convert_started = std::time::Instant::now();
        let md = tokio::task::spawn_blocking(move || convert_email_html(&html))
            .await
            .map_err(|error| anyhow!(tr!("message-error-html-conversion", { error: error })))?;
        let md = resolved.iter().fold(md, |acc, img| {
            acc.replace(
                &format!("cid:{}", img.cid),
                &format!("bytes://cid-{}", img.cid),
            )
        });
        let md = drop_unresolved_cid_images(&md);
        // Measured after the string passes too: on a long newsletter they cost
        // more than the conversion itself.
        let content = collapse_blank_lines(&md);
        convert_elapsed = convert_started.elapsed();
        (content, BodyFormat::Markdown, resolved, Some(raw_html))
    } else if let Some(text) = text_body {
        (text, BodyFormat::Text, Vec::new(), None)
    } else {
        (String::new(), BodyFormat::Text, Vec::new(), None)
    };

    let attachments = file_refs
        .into_iter()
        .map(|reference| Attachment {
            id: reference.attachment_id,
            filename: reference.filename,
            mime: reference.mime,
            size: reference.size,
            bytes: None,
        })
        .collect();

    // Both of these can add a third sequential round trip, but only for drafts
    // and for messages carrying a calendar part.
    let draft_started = std::time::Instant::now();
    let draft_id = if is_draft {
        match lookup_draft_id(client, access_token, id).await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("gmail draft lookup failed: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let draft_elapsed = draft_started.elapsed();

    let invitation_started = std::time::Instant::now();
    let invitation = resolve_calendar_invitation(client, access_token, id, calendar_refs).await;
    let invitation_elapsed = invitation_started.elapsed();

    log::debug!(
        "gmail get_message in {} ms \
         (payload_ms={}, inline_ms={}, inline_images={}, convert_ms={}, \
         draft_ms={}, invitation_ms={})",
        started.elapsed().as_millis(),
        payload_elapsed.as_millis(),
        inline_elapsed.as_millis(),
        inline_requested,
        convert_elapsed.as_millis(),
        draft_elapsed.as_millis(),
        invitation_elapsed.as_millis()
    );

    Ok(Message {
        header,
        body: content,
        format,
        inline_images: inline,
        attachments,
        tags,
        raw_body,
        to,
        cc,
        bcc,
        draft_id,
        invitation,
    })
}

/// Split an RFC 5322 address-list header like `"Contact A <a@x>, Contact B <b@y>"` into
/// individual address entries. Naive split on `,` is wrong because quoted
/// names can contain commas — we walk the string respecting quote/angle
/// nesting. Each output entry is the unmodified input slice for that address.
fn parse_address_header(payload: &GmailPayload, name: &str) -> Vec<String> {
    let raw = match header(payload, name) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut angle_depth = 0i32;
    for ch in raw.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            '<' if !in_quotes => {
                angle_depth += 1;
                cur.push(ch);
            }
            '>' if !in_quotes => {
                angle_depth -= 1;
                cur.push(ch);
            }
            ',' if !in_quotes && angle_depth == 0 => {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

/// Resolve a Gmail message id to its corresponding `draft.id`. Gmail keeps
/// these as separate concepts: `messages.list?labelIds=DRAFT` returns message
/// ids, but `drafts.update`/`drafts.send`/`drafts.delete` need the draft id.
/// We page through `users.drafts.list` until we find the draft whose embedded
/// `message.id` matches. Returns `None` when the draft isn't found (race with
/// a remote send/delete).
async fn lookup_draft_id(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct DraftsList {
        #[serde(default)]
        drafts: Vec<DraftEntry>,
        #[serde(rename = "nextPageToken", default)]
        next_page_token: Option<String>,
    }
    #[derive(Deserialize)]
    struct DraftEntry {
        id: String,
        #[serde(default)]
        message: Option<DraftMsgRef>,
    }
    #[derive(Deserialize)]
    struct DraftMsgRef {
        #[serde(default)]
        id: String,
    }
    let mut page_token: Option<String> = None;
    loop {
        let mut req = client
            .get(format!("{BASE}/users/me/drafts"))
            .bearer_auth(access_token)
            .query(&[("maxResults", "100")]);
        if let Some(tok) = &page_token {
            req = req.query(&[("pageToken", tok.as_str())]);
        }
        let resp = req.send().await?;
        let resp = check_status(resp, "drafts.list").await?;
        let body: DraftsList = resp.json().await?;
        for d in body.drafts {
            if d.message.as_ref().map(|m| m.id.as_str()) == Some(message_id) {
                return Ok(Some(d.id));
            }
        }
        match body.next_page_token {
            Some(tok) => page_token = Some(tok),
            None => return Ok(None),
        }
    }
}

/// Metadata for one non-inline attachment part discovered while walking the
/// Gmail payload. We collect these refs first and fan out the byte fetches
/// in `fetch_file_attachments` so they run in parallel.
struct FileRef {
    filename: String,
    mime: String,
    size: u64,
    attachment_id: String,
}

struct CalendarPartRef {
    data: Option<String>,
    attachment_id: Option<String>,
}

fn walk_payload(
    p: &GmailPayload,
    html_out: &mut Option<String>,
    text_out: &mut Option<String>,
    inline_out: &mut Vec<(String, String, String)>,
    files_out: &mut Vec<FileRef>,
    calendar_out: &mut Vec<CalendarPartRef>,
) {
    let mime = p.mime_type.to_ascii_lowercase();
    let cid = p
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Id"))
        .map(|h| {
            h.value
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        });
    let disposition = p
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Disposition"))
        .map(|h| h.value.to_ascii_lowercase())
        .unwrap_or_default();
    let is_inline_disp = disposition.starts_with("inline");

    if mime == "text/calendar" || mime.starts_with("text/calendar;") {
        if let Some(body) = &p.body {
            calendar_out.push(CalendarPartRef {
                data: body.data.clone(),
                attachment_id: body.attachment_id.clone(),
            });
        }
    } else if mime.starts_with("image/") && (cid.is_some() || is_inline_disp) {
        if let Some(body) = &p.body {
            if let Some(att_id) = &body.attachment_id {
                let key = cid.unwrap_or_else(|| {
                    if !p.filename.is_empty() {
                        p.filename.clone()
                    } else {
                        att_id.clone()
                    }
                });
                inline_out.push((key, mime.clone(), att_id.clone()));
            }
        }
    } else if mime == "text/html" && html_out.is_none() {
        if let Some(body) = &p.body {
            if let Some(data) = &body.data {
                if let Some(decoded) = decode_b64url(data) {
                    *html_out = Some(decoded);
                }
            }
        }
    } else if mime == "text/plain" && text_out.is_none() {
        if let Some(body) = &p.body {
            if let Some(data) = &body.data {
                if let Some(decoded) = decode_b64url(data) {
                    *text_out = Some(decoded);
                }
            }
        }
    } else if !p.filename.is_empty() && !mime.starts_with("multipart/") {
        // Anything with a filename that isn't a recognised inline image and
        // isn't a multipart container is a regular attachment.
        if let Some(body) = &p.body {
            if let Some(att_id) = &body.attachment_id {
                files_out.push(FileRef {
                    filename: p.filename.clone(),
                    mime: mime.clone(),
                    size: body.size,
                    attachment_id: att_id.clone(),
                });
            }
        }
    }

    for child in &p.parts {
        walk_payload(
            child,
            html_out,
            text_out,
            inline_out,
            files_out,
            calendar_out,
        );
    }
}

async fn resolve_calendar_invitation(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
    refs: Vec<CalendarPartRef>,
) -> Option<CalendarInvitation> {
    for reference in refs {
        let bytes = if let Some(data) = reference.data {
            decode_b64url_bytes(&data)
        } else if let Some(attachment_id) = reference.attachment_id {
            fetch_attachment(client, access_token, message_id, &attachment_id)
                .await
                .ok()
        } else {
            None
        };
        let Some(bytes) = bytes else {
            continue;
        };
        let Ok(calendar) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Some(uid) = invitation_uid(calendar) else {
            continue;
        };
        match super::calendar::invitation_by_ical_uid(client, access_token, &uid).await {
            Ok(Some(invitation)) => return Some(invitation),
            Ok(None) => {}
            Err(error) => {
                log::warn!("failed to resolve Gmail calendar invitation: {error:#}");
            }
        }
    }
    None
}

/// Extracts the VEVENT UID from an iTIP meeting request. RFC 5545 folded
/// content lines are unfolded before matching; cancellations and replies are
/// deliberately ignored because they do not offer attendee response actions.
fn invitation_uid(calendar: &str) -> Option<String> {
    let normalized = calendar.replace("\r\n", "\n");
    let mut lines: Vec<String> = Vec::new();
    for line in normalized.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(previous) = lines.last_mut() {
                previous.push_str(line.trim_start());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    let request = lines.iter().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("METHOD") && value.trim().eq_ignore_ascii_case("REQUEST")
        })
    });
    if !request {
        return None;
    }
    lines.into_iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.split(';')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("UID"))
            .then(|| value.trim().to_string())
            .filter(|uid| !uid.is_empty())
    })
}

pub async fn fetch_attachment(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>> {
    let url = format!("{BASE}/users/me/messages/{message_id}/attachments/{attachment_id}");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    let resp = check_status(resp, "attachment").await?;
    let body: AttachmentBody = resp.json().await?;
    let data = body
        .data
        .ok_or_else(|| anyhow::anyhow!(tr!("attachment-error-no-content")))?;
    decode_b64url_bytes(&data)
        .ok_or_else(|| anyhow::anyhow!(tr!("attachment-error-invalid-content")))
}

async fn fetch_inline_images(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
    refs: Vec<(String, String, String)>,
) -> Vec<InlineImage> {
    let handles: Vec<_> = refs
        .into_iter()
        .map(|(cid, mime, att_id)| {
            let client = client.clone();
            let token = access_token.to_string();
            let mid = message_id.to_string();
            tokio::spawn(async move {
                let url = format!("{BASE}/users/me/messages/{mid}/attachments/{att_id}");
                let resp = client.get(&url).bearer_auth(&token).send().await.ok()?;
                if !resp.status().is_success() {
                    log::warn!("gmail attachment {att_id} status {}", resp.status());
                    return None;
                }
                let body: AttachmentBody = resp.json().await.ok()?;
                let data = body.data?;
                let bytes = decode_b64url_bytes(&data)?;
                Some(InlineImage { cid, mime, bytes })
            })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(Some(img)) = h.await {
            out.push(img);
        }
    }
    out
}

pub async fn delete_message(client: &reqwest::Client, access_token: &str, id: &str) -> Result<()> {
    // Gmail's DELETE is permanent; use trash for parity with Outlook's "delete".
    let resp = trash_request(client, access_token, id).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "gmail trash failed").await);
    }
    Ok(())
}

fn trash_request(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
) -> reqwest::RequestBuilder {
    let url = format!("{BASE}/users/me/messages/{id}/trash");
    client
        .post(url)
        .bearer_auth(access_token)
        // This Gmail action has no payload, but its HTTP/1.1 endpoint rejects
        // POST requests that omit Content-Length instead of treating them as empty.
        .header(reqwest::header::CONTENT_LENGTH, "0")
}

pub async fn set_flag(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    flagged: bool,
) -> Result<()> {
    let payload = if flagged {
        serde_json::json!({ "addLabelIds": ["STARRED"] })
    } else {
        serde_json::json!({ "removeLabelIds": ["STARRED"] })
    };
    modify_labels(client, access_token, id, &payload).await
}

pub async fn mark_read(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    read: bool,
) -> Result<()> {
    let payload = if read {
        serde_json::json!({ "removeLabelIds": ["UNREAD"] })
    } else {
        serde_json::json!({ "addLabelIds": ["UNREAD"] })
    };
    modify_labels(client, access_token, id, &payload).await
}

/// Gmail "move": labels not folders. Add the target label, drop the source
/// (which is `INBOX` when moving from the inbox view, or the user-label the
/// row was rendered under). The id never changes — caller returns `None` to
/// signal that.
pub async fn move_message(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    source_folder_id: Option<&str>,
    target_folder_id: &str,
) -> Result<Option<String>> {
    let payload = move_label_payload(source_folder_id, target_folder_id);
    modify_labels(client, access_token, id, &payload).await?;
    Ok(None)
}

/// Label mutation a move translates into. Split out of the request so the
/// three-way distinction it encodes — archive, reclassification into a Gmail
/// tab, ordinary move — is unit-testable.
fn move_label_payload(source_folder_id: Option<&str>, target_folder_id: &str) -> serde_json::Value {
    // Map well-known target aliases to their Gmail label ids. Other ids (real
    // user labels) pass through unchanged.
    let target = match target_folder_id {
        "inbox" => "INBOX",
        "sentitems" => "SENT",
        "drafts" => "DRAFT",
        "deleteditems" => "TRASH",
        "junkemail" => "SPAM",
        other => other,
    };
    // Source is what we want to remove. When the user selected an "all mail"
    // view (source `None`), we still drop INBOX since "moving" without a
    // source most commonly means "out of the inbox". Caller can pass an
    // explicit source to override.
    let source = source_folder_id
        .map(|s| match s {
            "inbox" => "INBOX",
            "sentitems" => "SENT",
            "drafts" => "DRAFT",
            "deleteditems" => "TRASH",
            "junkemail" => "SPAM",
            other => other,
        })
        .unwrap_or("INBOX");
    let archive = target.eq_ignore_ascii_case(crate::providers::ARCHIVE_FOLDER_ALIAS);
    let target_is_category = target.starts_with("CATEGORY_");
    let source_is_category = source.starts_with("CATEGORY_");
    if archive {
        // Gmail has no Archive label: archiving is exactly removing INBOX.
        // Category labels describe classification and remain untouched.
        let remove = if source_is_category { "INBOX" } else { source };
        serde_json::json!({ "removeLabelIds": [remove] })
    } else if target_is_category {
        // Moving to a Gmail tab is reclassification, not archiving: keep (or
        // restore) INBOX and only remove the previous category/source label.
        let remove: Vec<&str> = if source == "INBOX" || source == target {
            Vec::new()
        } else {
            vec![source]
        };
        serde_json::json!({
            "addLabelIds": ["INBOX", target],
            "removeLabelIds": remove,
        })
    } else {
        // A category view is still an inbox view. Moving out of it must remove
        // INBOX (archive semantics), not destroy Gmail's classification.
        let remove = if source_is_category { "INBOX" } else { source };
        serde_json::json!({
            "addLabelIds": [target],
            "removeLabelIds": [remove],
        })
    }
}

pub(super) async fn modify_labels_public(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    modify_labels(client, access_token, id, payload).await
}

async fn modify_labels(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let url = format!("{BASE}/users/me/messages/{id}/modify");
    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .json(payload)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "gmail modify failed").await);
    }
    Ok(())
}

/// Renders a parsed query into Gmail's `q` syntax, which happens to be the
/// closest of the three backends to Aviary's own operators.
///
/// Values are quoted so a space or a colon inside one cannot be read as more
/// operators. Dates use Gmail's `YYYY/MM/DD`, and `after:` is inclusive on
/// Gmail's side as it is here.
pub(super) fn search_expression(query: &SearchQuery) -> String {
    fn quote(value: &str) -> String {
        format!("\"{}\"", value.replace('"', " "))
    }

    let mut parts: Vec<String> = Vec::new();
    for term in &query.from {
        parts.push(format!("from:{}", quote(term)));
    }
    for term in &query.to {
        parts.push(format!("to:{}", quote(term)));
    }
    for term in &query.subject {
        parts.push(format!("subject:{}", quote(term)));
    }
    if query.has_attachment == Some(true) {
        parts.push("has:attachment".into());
    }
    match query.is_read {
        Some(true) => parts.push("is:read".into()),
        Some(false) => parts.push("is:unread".into()),
        None => {}
    }
    match query.is_flagged {
        Some(true) => parts.push("is:starred".into()),
        Some(false) => parts.push("-is:starred".into()),
        None => {}
    }
    if let Some(before) = query.before {
        parts.push(format!("before:{}", before.format("%Y/%m/%d")));
    }
    if let Some(after) = query.after {
        parts.push(format!("after:{}", after.format("%Y/%m/%d")));
    }
    for term in &query.terms {
        parts.push(quote(term));
    }
    parts.join(" ")
}

pub async fn search(
    client: &reqwest::Client,
    access_token: &str,
    query: &SearchQuery,
    folder_id: Option<Option<&str>>,
    limit: usize,
) -> Result<Vec<MessageHeader>> {
    let expression = search_expression(query);
    let limit_s = limit.to_string();
    let request = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("q", expression.as_str()), ("maxResults", limit_s.as_str())]);
    // Scoping is a label restriction. `with_folder_labels` also re-adds INBOX
    // for a category, a Gmail tab being the intersection of the two.
    let request = match folder_id {
        Some(folder) => with_folder_labels(request, folder.unwrap_or("INBOX")),
        None => request,
    };
    let resp = request.send().await?;
    let resp = check_status(resp, "search").await?;
    let body: GmailListResponse = resp.json().await?;
    let ids: Vec<String> = body.messages.into_iter().map(|m| m.id).collect();
    fetch_metadata_batch(client, access_token, ids).await
}

pub async fn list_from_sender(
    client: &reqwest::Client,
    access_token: &str,
    email: &str,
    top: usize,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    let q = format!("from:{email}");
    let top_s = top.to_string();
    let resp = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[
            ("q", q.as_str()),
            ("maxResults", top_s.as_str()),
            ("includeSpamTrash", "true"),
        ])
        .send()
        .await?;
    let resp = check_status(resp, "list_from_sender").await?;
    let body: GmailListResponse = resp.json().await?;
    let ids: Vec<String> = body.messages.into_iter().map(|m| m.id).collect();
    let headers = fetch_metadata_batch(client, access_token, ids).await?;
    // Encode the `q=...` filter into the page-token blob so paginated calls
    // keep the same filter active. We use a marker ` __SEARCH__` so
    // `fetch_messages_page` can route correctly.
    let next = body
        .next_page_token
        .map(|t| format!("__SEARCH__|{}|{}", q, t));
    Ok((headers, next))
}

pub async fn list_thread(
    client: &reqwest::Client,
    access_token: &str,
    thread_id: &str,
) -> Result<Vec<MessageHeader>> {
    let url = format!("{BASE}/users/me/threads/{thread_id}");
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .query(&[
            ("format", "metadata"),
            ("metadataHeaders", "Subject"),
            ("metadataHeaders", "From"),
        ])
        .send()
        .await?;
    let resp = check_status(resp, "thread").await?;
    #[derive(Deserialize)]
    struct ThreadResp {
        #[serde(default)]
        messages: Vec<GmailMessage>,
    }
    let body: ThreadResp = resp.json().await?;
    let mut out: Vec<MessageHeader> = body.messages.iter().map(message_header_from).collect();
    out.sort_by_key(|m| m.received);
    Ok(out)
}

fn decode_b64url(s: &str) -> Option<String> {
    decode_b64url_bytes(s).and_then(|b| String::from_utf8(b).ok())
}

fn decode_b64url_bytes(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s))
        .ok()
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
    use super::{
        invitation_uid, is_retryable_gmail_error, labels_belong_to_folder, move_label_payload,
        parse_batch_metadata_response, trash_request, unfold_header_value, with_folder_labels,
    };
    use crate::providers::{ARCHIVE_FOLDER_ALIAS, INBOX_FOLDER_ALIAS, JUNK_FOLDER_ALIAS};
    use crate::search_query::SearchQuery;

    /// Archiving is the one move with no target label: Gmail has no Archive
    /// folder, so the operation is exactly "drop INBOX". The UI relies on this
    /// by reporting no source folder (`ui::app::archive_message_with_undo`),
    /// which must archive rather than strip whichever label is on screen.
    #[test]
    fn archiving_drops_inbox_whatever_the_source_folder_is() {
        for source in [None, Some("inbox"), Some("CATEGORY_PROMOTIONS")] {
            let payload = move_label_payload(source, ARCHIVE_FOLDER_ALIAS);
            assert_eq!(
                payload,
                serde_json::json!({ "removeLabelIds": ["INBOX"] }),
                "source {source:?} should archive by removing INBOX"
            );
        }
    }

    /// Archiving while a user label is selected keeps that label: the message
    /// leaves the inbox but stays classified.
    #[test]
    fn archiving_from_a_user_label_removes_that_label_only_when_asked() {
        let payload = move_label_payload(Some("Label_42"), ARCHIVE_FOLDER_ALIAS);
        assert_eq!(
            payload,
            serde_json::json!({ "removeLabelIds": ["Label_42"] })
        );
    }

    /// Marking as junk must move the message, not merely label it: `SPAM`
    /// alongside `INBOX` would leave it in the inbox. Like archiving, the UI
    /// reports no source folder, so the removal falls back to `INBOX`.
    #[test]
    fn marking_junk_adds_spam_and_leaves_the_inbox() {
        let payload = move_label_payload(None, JUNK_FOLDER_ALIAS);
        assert_eq!(
            payload,
            serde_json::json!({
                "addLabelIds": ["SPAM"],
                "removeLabelIds": ["INBOX"],
            })
        );
    }

    /// The reverse action passes the junk folder as its source *precisely*
    /// because Gmail needs the label dropped: restoring to `INBOX` without it
    /// would leave the message flagged as spam in Gmail's own UI.
    #[test]
    fn not_junk_restores_the_inbox_and_drops_spam() {
        for source in [JUNK_FOLDER_ALIAS, "SPAM"] {
            let payload = move_label_payload(Some(source), INBOX_FOLDER_ALIAS);
            assert_eq!(
                payload,
                serde_json::json!({
                    "addLabelIds": ["INBOX"],
                    "removeLabelIds": ["SPAM"],
                }),
                "source {source:?} should restore the inbox and drop SPAM"
            );
        }
    }

    /// Gmail's dialect is the closest to Aviary's own operators, so this is
    /// mostly a renaming — but the values must stay quoted or a space inside
    /// one would read as further operators.
    #[test]
    fn search_expression_renders_gmail_operators() {
        let query = SearchQuery::parse(
            "de:alice objet:\"bon de commande\" avec:pj est:non-lu avant:2026-03-15 contrat",
        );
        let rendered = super::search_expression(&query);
        assert!(rendered.contains("from:\"alice\""), "{rendered}");
        assert!(
            rendered.contains("subject:\"bon de commande\""),
            "{rendered}"
        );
        assert!(rendered.contains("has:attachment"), "{rendered}");
        assert!(rendered.contains("is:unread"), "{rendered}");
        assert!(rendered.contains("before:2026/03/15"), "{rendered}");
        assert!(rendered.contains("\"contrat\""), "{rendered}");
    }

    /// Plain words must not acquire operators on the way out.
    #[test]
    fn search_expression_leaves_plain_text_alone() {
        assert_eq!(
            super::search_expression(&SearchQuery::parse("contrat")),
            "\"contrat\""
        );
    }

    /// An ordinary move still adds the destination label, so archiving stays
    /// distinguishable from "move to a folder".
    #[test]
    fn moving_to_a_user_label_adds_it_and_drops_the_source() {
        let payload = move_label_payload(Some("inbox"), "Label_7");
        assert_eq!(
            payload,
            serde_json::json!({
                "addLabelIds": ["Label_7"],
                "removeLabelIds": ["INBOX"],
            })
        );
    }

    const BOUNDARY: &str = "batch_response";

    fn batch(parts: &[(&str, u16, &str)]) -> Vec<u8> {
        let mut body = String::new();
        for (id, status, json) in parts {
            body.push_str(&format!(
                "--{BOUNDARY}\r\nContent-Type: application/http\r\nContent-ID: response-{id}\r\n\r\nHTTP/1.1 {status} Status\r\nContent-Type: application/json\r\n\r\n{json}\r\n"
            ));
        }
        body.push_str(&format!("--{BOUNDARY}--\r\n"));
        body.into_bytes()
    }

    #[test]
    fn batch_parser_returns_retryable_ids_without_truncating_successes() {
        let success = r#"{
            "id":"ok",
            "threadId":"thread",
            "snippet":"preview",
            "labelIds":["INBOX"],
            "internalDate":"0",
            "payload":{
                "mimeType":"text/plain",
                "headers":[
                    {"name":"Subject","value":"Hello"},
                    {"name":"From","value":"sender@example.com"}
                ]
            }
        }"#;
        let limited = r#"{"error":{"code":429,"message":"Too many requests"}}"#;
        let body = batch(&[("ok", 200, success), ("limited", 429, limited)]);
        let ids = vec!["ok".to_string(), "limited".to_string()];

        let parsed = parse_batch_metadata_response(&body, BOUNDARY, &ids).unwrap();

        assert_eq!(parsed.metadata.len(), 1);
        assert_eq!(parsed.metadata[0].header.id, "ok");
        // The listing is where the message list learns about threads: without
        // `threadId` on the header, grouping has nothing to key on.
        assert_eq!(
            parsed.metadata[0].header.conversation_id.as_deref(),
            Some("thread")
        );
        assert_eq!(parsed.retry_ids, vec!["limited"]);
    }

    #[test]
    fn metadata_header_folding_is_removed_before_display() {
        assert_eq!(
            unfold_header_value("Subject line\r\n\tcontinued"),
            "Subject line continued"
        );
        assert_eq!(
            unfold_header_value("Contact A <contact.a@example.test>"),
            "Contact A <contact.a@example.test>"
        );
    }

    #[test]
    fn incremental_metadata_must_still_belong_to_the_requested_folder() {
        assert!(labels_belong_to_folder(&["INBOX".into()], "INBOX"));
        assert!(!labels_belong_to_folder(&["SENT".into()], "INBOX"));
        assert!(labels_belong_to_folder(
            &["INBOX".into(), "CATEGORY_PERSONAL".into()],
            "CATEGORY_PERSONAL"
        ));
        assert!(!labels_belong_to_folder(
            &["CATEGORY_PERSONAL".into()],
            "CATEGORY_PERSONAL"
        ));
    }

    #[test]
    fn batch_parser_rejects_a_missing_sub_response() {
        let body = batch(&[("gone", 404, r#"{"error":{"code":404}}"#)]);
        let ids = vec!["gone".to_string(), "missing".to_string()];

        let error = match parse_batch_metadata_response(&body, BOUNDARY, &ids) {
            Ok(_) => panic!("a truncated batch must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("1 parts for 2 requests"));
    }

    #[test]
    fn legacy_403_rate_limit_is_retryable_but_auth_error_is_not() {
        assert!(is_retryable_gmail_error(
            403,
            br#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        ));
        assert!(!is_retryable_gmail_error(
            403,
            br#"{"error":{"errors":[{"reason":"authError"}]}}"#
        ));
    }

    #[test]
    fn category_folder_queries_intersect_with_inbox() {
        let request = with_folder_labels(
            reqwest::Client::new().get("https://example.test/messages"),
            "CATEGORY_PERSONAL",
        )
        .build()
        .unwrap();
        let labels: Vec<_> = request
            .url()
            .query_pairs()
            .filter(|(key, _)| key == "labelIds")
            .map(|(_, value)| value.into_owned())
            .collect();

        assert_eq!(labels, vec!["INBOX", "CATEGORY_PERSONAL"]);
    }

    #[test]
    fn trash_request_declares_its_empty_body() {
        let request = trash_request(&reqwest::Client::new(), "token", "message-id")
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(reqwest::header::CONTENT_LENGTH),
            Some(&reqwest::header::HeaderValue::from_static("0"))
        );
    }

    #[test]
    fn calendar_request_uid_is_unfolded_and_non_requests_are_ignored() {
        let request = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                       UID:synthetic-event-\r\n 001@example.test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            invitation_uid(request).as_deref(),
            Some("synthetic-event-001@example.test")
        );

        let cancellation = request.replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert_eq!(invitation_uid(&cancellation), None);
    }
}
