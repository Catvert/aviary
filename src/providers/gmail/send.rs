use super::{check_status, BASE};
use crate::providers::{build_rfc822, OutgoingMessage, SentIds};
use anyhow::Result;
use base64::Engine;
use serde::Deserialize;

/// Build a base64url-encoded RFC822 message body for Gmail's `messages.send`.
///
/// MIME construction is shared with IMAP/SMTP; Gmail only adds base64url.
fn build_raw(msg: &OutgoingMessage<'_>, extra_headers: &[(&str, String)]) -> Result<String> {
    let message = build_rfc822(msg, extra_headers, true)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(message.formatted()))
}

/// `messages.send` returns the created message, whose `id` lives in the
/// sender's mailbox under the SENT label — the real sent copy, directly
/// fetchable later with `messages.get`.
#[derive(Deserialize)]
struct SendResponse {
    #[serde(default)]
    id: String,
}

impl From<SendResponse> for SentIds {
    fn from(resp: SendResponse) -> Self {
        SentIds {
            message_id: Some(resp.id).filter(|id| !id.is_empty()),
            internet_message_id: None,
        }
    }
}

pub async fn send_mail(
    client: &reqwest::Client,
    access_token: &str,
    msg: &OutgoingMessage<'_>,
) -> Result<SentIds> {
    let raw = build_raw(msg, &[])?;
    let payload = serde_json::json!({ "raw": raw });
    let resp = client
        .post(format!("{BASE}/users/me/messages/send"))
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await?;
    let resp = check_status(resp, "send_mail").await?;
    let sent: SendResponse = resp.json().await?;
    Ok(sent.into())
}

/// Save `msg` as a Gmail draft. When `replace_id` is `Some`, we `PUT
/// /users/me/drafts/{id}` so the existing draft is updated in place — the
/// draft id is preserved (the underlying message id may change, which is
/// fine because every subsequent operation goes through the draft id).
/// When `None`, a fresh draft is created via POST. Returns the draft id
/// (Gmail's `draft.id`, distinct from the underlying `message.id`) so the
/// caller can target it for further updates, deletion or send.
pub async fn save_draft(
    client: &reqwest::Client,
    access_token: &str,
    msg: &OutgoingMessage<'_>,
    replace_id: Option<&str>,
) -> Result<String> {
    let raw = build_raw(msg, &[])?;
    let payload = serde_json::json!({ "message": { "raw": raw } });
    let resp = if let Some(id) = replace_id {
        client
            .put(format!("{BASE}/users/me/drafts/{id}"))
            .bearer_auth(access_token)
            .json(&payload)
            .send()
            .await?
    } else {
        client
            .post(format!("{BASE}/users/me/drafts"))
            .bearer_auth(access_token)
            .json(&payload)
            .send()
            .await?
    };
    let resp = check_status(resp, "save_draft").await?;
    #[derive(Deserialize)]
    struct DraftResp {
        id: String,
    }
    let body: DraftResp = resp.json().await?;
    Ok(body.id)
}

/// Delete a draft by its `draft.id`. Gmail also wipes the underlying
/// message, so we don't need a separate `messages.trash` call afterward.
pub async fn delete_draft(
    client: &reqwest::Client,
    access_token: &str,
    draft_id: &str,
) -> Result<()> {
    let resp = client
        .delete(format!("{BASE}/users/me/drafts/{draft_id}"))
        .bearer_auth(access_token)
        .send()
        .await?;
    let _ = check_status(resp, "delete_draft").await?;
    Ok(())
}

pub async fn send_reply(
    client: &reqwest::Client,
    access_token: &str,
    reply_to_id: &str,
    msg: &OutgoingMessage<'_>,
) -> Result<SentIds> {
    // We need the original message's `Message-ID` + `References` to thread
    // correctly, plus the `threadId` to keep Gmail's UI grouping intact.
    let orig = client
        .get(format!("{BASE}/users/me/messages/{reply_to_id}"))
        .bearer_auth(access_token)
        .query(&[
            ("format", "metadata"),
            ("metadataHeaders", "Message-ID"),
            ("metadataHeaders", "References"),
            ("metadataHeaders", "Subject"),
        ])
        .send()
        .await?;
    let orig = check_status(orig, "reply_lookup").await?;
    #[derive(Deserialize)]
    struct OrigMsg {
        #[serde(default, rename = "threadId")]
        thread_id: String,
        payload: Option<OrigPayload>,
    }
    #[derive(Deserialize)]
    struct OrigPayload {
        #[serde(default)]
        headers: Vec<HeaderKV>,
    }
    #[derive(Deserialize)]
    struct HeaderKV {
        name: String,
        value: String,
    }
    let orig: OrigMsg = orig.json().await?;
    let h = orig.payload.map(|p| p.headers).unwrap_or_default();
    let pick = |name: &str| -> Option<String> {
        h.iter()
            .find(|x| x.name.eq_ignore_ascii_case(name))
            .map(|x| x.value.clone())
    };
    let mut extras: Vec<(&str, String)> = Vec::new();
    if let Some(mid) = pick("Message-ID") {
        let refs = pick("References")
            .map(|r| format!("{r} {mid}"))
            .unwrap_or_else(|| mid.clone());
        extras.push(("In-Reply-To", mid));
        extras.push(("References", refs));
    }
    let raw = build_raw(msg, &extras)?;
    let mut payload = serde_json::json!({ "raw": raw });
    if !orig.thread_id.is_empty() {
        payload["threadId"] = serde_json::Value::String(orig.thread_id);
    }
    let resp = client
        .post(format!("{BASE}/users/me/messages/send"))
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await?;
    let resp = check_status(resp, "send_reply").await?;
    let sent: SendResponse = resp.json().await?;
    Ok(sent.into())
}
