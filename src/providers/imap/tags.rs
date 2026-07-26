//! IMAP implementation of the `Tag` concept — RFC 3501 custom flags
//! (a.k.a. "keywords" in the IMAP spec, or `$Tag` flags by convention).
//!
//! IMAP doesn't have a central tag registry; keywords are per-message
//! flags that the server merely stores. So:
//! - `list_tags` discovers known keywords in the INBOX's `FLAGS` and
//!   `PERMANENTFLAGS` replies.
//! - `create_tag` is a no-op (no central registry); the tag exists once
//!   `add_tag_to_message` has applied it to at least one message.
//! - `delete_tag` is also a no-op for the same reason. The UI removes the
//!   column from the user's saved list and we forget about it.
//! - `add_tag_to_message` / `remove_tag_from_message` use UID STORE.
//! - `list_messages_tagged` searches the INBOX (we don't traverse every
//!   folder — that would be O(folders × Search) and slow).

use super::connect::with_session;
use crate::providers::{ImapAuth, TagRename};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;

use crate::model::{MessageHeader, Tag};

fn parse_id(id: &str) -> Result<(String, u32)> {
    let (f, u) = id
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid message identifier: {id}"))?;
    let folder = urlencoding::decode(f)
        .map_err(|e| anyhow::anyhow!(tr!("imap-error-folder-decode", { error: e })))?
        .to_string();
    let uid = u.parse::<u32>().context(tr!("imap-error-uid-invalid"))?;
    Ok((folder, uid))
}

/// Probe the INBOX's FLAGS and PERMANENTFLAGS for declared keywords.
pub async fn list_tags(auth: &ImapAuth<'_>) -> Result<Vec<Tag>> {
    with_session(auth, |session| {
        let inbox = session
            .select("INBOX")
            .context(tr!("technical-operation-failed", { operation: "SELECT INBOX" }))?;
        let names: BTreeSet<String> = inbox
            .flags
            .iter()
            .chain(inbox.permanent_flags.iter())
            .filter_map(|f| {
                if let imap::types::Flag::Custom(name) = f {
                    (!is_system_keyword(name)).then(|| name.to_string())
                } else {
                    None
                }
            })
            .collect();
        let mut out: Vec<Tag> = names
            .into_iter()
            .map(|name| Tag {
                id: name.clone(),
                display_name: pretty_keyword(&name),
                color: None,
            })
            .collect();
        out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        Ok(out)
    })
    .await
}

/// `$Project1` → "Project1"; "Foo" stays as-is. Convention is to prefix
/// custom keywords with `$` so they don't clash with system flag names.
fn pretty_keyword(raw: &str) -> String {
    raw.strip_prefix('$').unwrap_or(raw).to_string()
}

/// IMAP keywords reserved for system use (RFC 5788 and the IANA registry),
/// never user tags. `$Forwarded` populates `MessageHeader::last_action`; the
/// others are simply hidden.
pub(super) fn is_system_keyword(name: &str) -> bool {
    const SYSTEM: &[&str] = &["$Forwarded", "$MDNSent", "$Junk", "$NotJunk", "$Phishing"];
    SYSTEM.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Best-effort: IMAP keywords are not declared centrally, so creating a
/// tag is a no-op. The tag becomes visible to `list_tags` once a message
/// has been flagged with it. We return a synthetic `Tag` so the UI can
/// add a column right away.
pub async fn create_tag(_auth: &ImapAuth<'_>, name: &str) -> Result<Tag> {
    validate_keyword(name)?;
    let id = if name.starts_with('$') {
        name.to_string()
    } else {
        format!("${name}")
    };
    Ok(Tag {
        id,
        display_name: name.to_string(),
        color: None,
    })
}

fn validate_keyword(keyword: &str) -> Result<()> {
    let name = keyword.strip_prefix('$').unwrap_or(keyword);
    let invalid = name.is_empty()
        || name.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'(' | b')' | b'{' | b']' | b'%' | b'*' | b'"' | b'\\')
        });
    if invalid {
        bail!(tr!("imap-error-invalid-tag-name"));
    }
    Ok(())
}

pub async fn rename_tag(_auth: &ImapAuth<'_>, _id: &str, _new_name: &str) -> Result<TagRename> {
    anyhow::bail!(tr!("imap-error-tag-rename-unsupported"))
}

pub async fn delete_tag(_auth: &ImapAuth<'_>, _id: &str) -> Result<()> {
    // No central registry to delete from; the column will simply vanish
    // from the user's saved list on the UI side.
    Ok(())
}

pub async fn add_tag_to_message(auth: &ImapAuth<'_>, message_id: &str, tag_id: &str) -> Result<()> {
    validate_keyword(tag_id)?;
    let (folder, uid) = parse_id(message_id)?;
    let cmd = format!("+FLAGS ({tag_id})");
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session.uid_store(uid.to_string(), &cmd).with_context(
            || tr!("technical-operation-failed", { operation: format!("UID STORE {cmd}") }),
        )?;
        Ok(())
    })
    .await
}

pub async fn remove_tag_from_message(
    auth: &ImapAuth<'_>,
    message_id: &str,
    tag_id: &str,
) -> Result<()> {
    validate_keyword(tag_id)?;
    let (folder, uid) = parse_id(message_id)?;
    let cmd = format!("-FLAGS ({tag_id})");
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session.uid_store(uid.to_string(), &cmd).with_context(
            || tr!("technical-operation-failed", { operation: format!("UID STORE {cmd}") }),
        )?;
        Ok(())
    })
    .await
}

/// Return up to `top` messages flagged with `tag_id`. Searches INBOX only —
/// scanning every folder would be expensive and most workflows keep tags
/// on inbox-level mail anyway. Servers that don't support keywords return
/// an empty list rather than an error.
pub async fn list_messages_tagged(
    auth: &ImapAuth<'_>,
    tag_id: &str,
    top: usize,
) -> Result<Vec<MessageHeader>> {
    validate_keyword(tag_id)?;
    if top == 0 {
        return Ok(Vec::new());
    }
    let tag_id = tag_id.to_string();
    with_session(auth, move |session| {
        session
            .select("INBOX")
            .context(tr!("technical-operation-failed", { operation: "SELECT INBOX" }))?;
        let uids = session
            .uid_search(format!("KEYWORD {tag_id}"))
            .with_context(|| {
                tr!("technical-operation-failed", {
                    operation: format!("UID SEARCH KEYWORD {tag_id}")
                })
            })?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let mut uids: Vec<u32> = uids.into_iter().collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(top);
        let set = uids
            .into_iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetches = session
            .uid_fetch(
                set,
                "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE MESSAGE-ID)])",
            )
            .context(tr!("technical-operation-failed", { operation: "UID FETCH (tag)" }))?;
        let mut messages: Vec<MessageHeader> = fetches
            .iter()
            .filter_map(|fetch| super::messages::message_header_from_fetch("INBOX", fetch))
            .collect();
        messages.sort_by_key(|message| std::cmp::Reverse(message.received));
        Ok(messages)
    })
    .await
}
