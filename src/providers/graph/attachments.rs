use super::{Client, GraphList, BASE};
use crate::model::{Attachment, InlineImage};
use crate::providers::html::{cid_matches, cid_references_name, normalize_cid};
use anyhow::{bail, Result};
use base64::Engine;
use futures::StreamExt;
use serde::Deserialize;
use std::time::Instant;

/// Only properties declared by the base `microsoft.graph.attachment` type are
/// valid in a collection `$select`. `contentId` and `contentBytes` belong to
/// the derived `fileAttachment` type and make the whole list request fail.
const ATTACHMENT_METADATA_SELECT: &str = "id,name,contentType,isInline,size";
/// Keep the eager CID downloads below Graph's per-mailbox concurrency ceiling.
/// Other mailbox operations can occupy the remaining permits concurrently.
const INLINE_FETCH_CONCURRENCY: usize = 2;

#[derive(Deserialize)]
struct GraphAttachment {
    id: Option<String>,
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    #[serde(rename = "isInline")]
    is_inline: Option<bool>,
    #[serde(rename = "contentId")]
    content_id: Option<String>,
    #[serde(rename = "contentBytes")]
    content_bytes: Option<String>,
    name: Option<String>,
    size: Option<u64>,
}

struct ResolvedAttachment {
    metadata: GraphAttachment,
    id: String,
    mime: String,
    cid: Option<String>,
    inline_bytes: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
struct InlineDisposition {
    /// Payload needed by Blitz even when the sender metadata does not let us
    /// prove that the body embeds it.
    candidate: bool,
    /// Hidden from the regular download list because the body embeds it.
    embedded: bool,
}

/// Fetch attachment metadata for a message and split it into eager inline
/// images and lazy regular files.
///
/// `html_cids` is the set of `cid:` references actually present in the body
/// HTML — it's used to suppress download chips for parts the body actually
/// embeds. Any file carrying a `contentId` remains an eager inline candidate,
/// even when the raw HTML scan misses the reference: Outlook senders are
/// inconsistent about casing, angle brackets and the `isInline` flag. Files
/// which are not actually referenced still remain visible as lazy downloads.
pub(super) async fn fetch_attachments(
    client: &Client<'_>,
    message_id: &str,
    html_cids: &[String],
) -> Result<(Vec<InlineImage>, Vec<Attachment>)> {
    let started = Instant::now();
    // Derived `fileAttachment` fields are resolved only for likely inline
    // images. Regular files therefore stay metadata-only and lazy.
    let url = format!("{BASE}/me/messages/{message_id}/attachments");
    let resp = client
        .get(&url)
        .query(&[("$select", ATTACHMENT_METADATA_SELECT)])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph attachments failed").await);
    }
    let list: GraphList<GraphAttachment> = resp.json().await?;
    let attachment_count = list.value.len();
    let resolved = futures::stream::iter(list.value)
        .map(|metadata| resolve_attachment(client, message_id, metadata, html_cids))
        // Preserve provider order while overlapping the network waits.
        .buffered(INLINE_FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut inline = Vec::new();
    let mut files = Vec::new();
    for resolved in resolved.into_iter().flatten() {
        let a = resolved.metadata;
        let disposition = inline_disposition(
            resolved.cid.as_deref(),
            a.name.as_deref(),
            a.is_inline.unwrap_or(false),
            html_cids,
        );
        // Register as a candidate inline image whenever there's any way the
        // body might reference it. Push by both `contentId` and `name` so
        // the substitution loop catches whichever form the markdown ends
        // up with.
        if disposition.candidate {
            if let Some(bytes) = resolved.inline_bytes {
                register_inline_aliases(
                    &mut inline,
                    resolved.cid.as_deref(),
                    a.name.as_deref(),
                    html_cids,
                    &resolved.mime,
                    bytes,
                );
            }
        }
        // Show as a downloadable file unless the body actually embeds this
        // part. We trust the HTML reference over Graph's `isInline` flag,
        // but still honour `isInline: true` as a hint that the sender
        // meant the part to be embedded (common for signature logos).
        if !disposition.embedded {
            files.push(Attachment {
                id: resolved.id,
                filename: a.name.unwrap_or_else(|| "attachment".to_string()),
                mime: resolved.mime,
                size: a.size.unwrap_or_default(),
                bytes: None,
            });
        }
    }
    log::debug!(
        "Graph attachments resolved in {} ms (metadata={}, inline_aliases={}, files={})",
        started.elapsed().as_millis(),
        attachment_count,
        inline.len(),
        files.len()
    );
    Ok((inline, files))
}

async fn resolve_attachment(
    client: &Client<'_>,
    message_id: &str,
    metadata: GraphAttachment,
    html_cids: &[String],
) -> Option<ResolvedAttachment> {
    let mime = metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let is_file = metadata
        .odata_type
        .as_deref()
        .map(|kind| kind.ends_with("fileAttachment"))
        // Some Graph responses omit the discriminator when `$select` is
        // used. Inline-only evidence is sufficient to identify a file
        // without mistaking item attachments for downloadable bytes.
        .unwrap_or_else(|| {
            metadata.is_inline.unwrap_or(false)
                || (!html_cids.is_empty() && mime.to_ascii_lowercase().starts_with("image/"))
        });
    if !is_file {
        return None;
    }
    let id = metadata.id.clone()?;
    let mut cid = metadata
        .content_id
        .as_deref()
        .map(normalize_cid)
        .filter(|cid| !cid.is_empty());
    let mut inline_bytes = None;
    if should_fetch_inline_details(
        cid.as_deref(),
        metadata.name.as_deref(),
        &mime,
        metadata.is_inline.unwrap_or(false),
        html_cids,
    ) {
        match fetch_attachment_record(client, message_id, &id).await {
            Ok(record) => {
                cid = record
                    .content_id
                    .as_deref()
                    .map(normalize_cid)
                    .filter(|cid| !cid.is_empty())
                    .or(cid);
                match decode_attachment_bytes(&record) {
                    Ok(bytes) => inline_bytes = Some(bytes),
                    Err(error) => {
                        log::warn!("failed to decode an inline Graph attachment: {error:#}");
                    }
                }
            }
            Err(error) => {
                log::warn!("failed to fetch an inline Graph attachment: {error:#}");
            }
        }
    }
    Some(ResolvedAttachment {
        metadata,
        id,
        mime,
        cid,
        inline_bytes,
    })
}

pub async fn fetch_attachment(
    client: &Client<'_>,
    message_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>> {
    let attachment = fetch_attachment_record(client, message_id, attachment_id).await?;
    decode_attachment_bytes(&attachment)
}

async fn fetch_attachment_record(
    client: &Client<'_>,
    message_id: &str,
    attachment_id: &str,
) -> Result<GraphAttachment> {
    let url = format!("{BASE}/me/messages/{message_id}/attachments/{attachment_id}");
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            tr!("attachment-error-provider", {
                provider: "Microsoft Graph",
                status: status,
                detail: body
            })
        );
    }
    Ok(resp.json().await?)
}

fn decode_attachment_bytes(attachment: &GraphAttachment) -> Result<Vec<u8>> {
    let encoded = attachment
        .content_bytes
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(tr!("attachment-error-no-content")))?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!(tr!("attachment-error-invalid-content")))
}

fn should_fetch_inline_details(
    content_id: Option<&str>,
    name: Option<&str>,
    mime: &str,
    inline_flag: bool,
    html_cids: &[String],
) -> bool {
    content_id.is_some()
        || inline_flag
        || name.is_some_and(|name| {
            html_cids
                .iter()
                .any(|reference| cid_references_name(reference, name))
        })
        // `isInline` is not reliable for mail produced by every Outlook
        // version. Restrict the permissive fallback to images in a body which
        // actually contains CID references, keeping documents/PDFs lazy.
        || (!html_cids.is_empty() && mime.to_ascii_lowercase().starts_with("image/"))
}

fn inline_disposition(
    content_id: Option<&str>,
    name: Option<&str>,
    inline_flag: bool,
    html_cids: &[String],
) -> InlineDisposition {
    let referenced_by_cid = content_id
        .map(|cid| {
            html_cids
                .iter()
                .any(|reference| cid_matches(reference, cid))
        })
        .unwrap_or(false);
    let referenced_by_name = name
        .map(|name| {
            html_cids
                .iter()
                .any(|reference| cid_references_name(reference, name))
        })
        .unwrap_or(false);
    InlineDisposition {
        candidate: content_id.is_some() || referenced_by_name || inline_flag,
        embedded: referenced_by_cid || referenced_by_name || inline_flag,
    }
}

fn register_inline_aliases(
    inline: &mut Vec<InlineImage>,
    content_id: Option<&str>,
    name: Option<&str>,
    html_cids: &[String],
    mime: &str,
    bytes: Vec<u8>,
) {
    let mut aliases = Vec::new();
    if let Some(content_id) = content_id {
        aliases.push(normalize_cid(content_id));
    }
    if let Some(name) = name {
        aliases.push(normalize_cid(name));
        aliases.extend(
            html_cids
                .iter()
                .filter(|reference| cid_references_name(reference, name))
                .map(|reference| normalize_cid(reference)),
        );
    }
    aliases.retain(|alias| !alias.is_empty());
    aliases.sort_by_key(|alias| alias.to_ascii_lowercase());
    aliases.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    for alias in aliases {
        inline.push(InlineImage {
            cid: alias,
            mime: mime.to_string(),
            bytes: bytes.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::html::extract_cids_from_html;

    #[test]
    fn extracts_cids_case_insensitively_and_normalizes_brackets() {
        let html = r#"<img src="CID:%3Cimage-a%40example.test%3E">"#;
        assert_eq!(extract_cids_from_html(html), vec!["image-a@example.test"]);
    }

    #[test]
    fn content_ids_match_despite_sender_casing() {
        assert!(cid_matches(
            "Image-A@Example.Test",
            "<image-a@example.test>"
        ));
    }

    #[test]
    fn unreferenced_content_id_remains_an_inline_candidate() {
        let disposition = inline_disposition(Some("image-a@example.test"), None, false, &[]);
        assert_eq!(
            disposition,
            InlineDisposition {
                candidate: true,
                embedded: false,
            }
        );
    }

    #[test]
    fn metadata_select_uses_only_base_attachment_properties() {
        let selected: Vec<_> = ATTACHMENT_METADATA_SELECT.split(',').collect();
        assert!(!selected.contains(&"contentId"));
        assert!(!selected.contains(&"contentBytes"));
        assert!(selected.contains(&"isInline"));
    }

    #[test]
    fn outlook_filename_matches_its_generated_content_id() {
        assert!(cid_references_name(
            "image-a.png@synthetic.example.test",
            "image-a.png"
        ));
    }
}
