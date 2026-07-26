//! IMAP message + folder operations.
//!
//! Message IDs are encoded as `"<urlencoded folder>:<uid>"` so they survive
//! folder switches without any extra runtime state. Folder IDs are normally
//! the literal mailbox name (`INBOX`, `INBOX/Sent`, …); duplicated special
//! folders use a local virtual id containing all their physical mailboxes.

use super::connect::{with_session, ImapSession};
use crate::model::{
    Account, AccountId, Attachment, BodyFormat, InlineImage, LastAction, MailFolder, Message,
    MessageHeader, Provider,
};
use crate::providers::html::{
    cid_matches, collapse_blank_lines, convert_email_html, extract_cids_from_html,
};
use crate::providers::{ImapAuth, MailSyncPage, MessagePage};
use crate::search_query::SearchQuery;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::DecodePaddingMode;
use base64::engine::{general_purpose::GeneralPurpose, GeneralPurposeConfig};
use base64::{alphabet, Engine as _};
use chrono::{DateTime, Utc};
use mail_parser::{Address, MessageParser, MimeHeaders, PartType};
use std::collections::{HashMap, HashSet};

const MERGED_FOLDER_PREFIX: &str = "aviary:imap-merged:v1:";
const PAGE_CURSOR_PREFIX: &str = "aviary:imap-page:v1:";

/// Header fields every listing FETCH asks for. `REFERENCES`/`IN-REPLY-TO`
/// ride along at negligible cost and are what [`derived_conversation_id`]
/// threads the mailbox with — IMAP has no native conversation id.
const LISTING_FETCH: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS \
     (FROM SUBJECT DATE MESSAGE-ID REFERENCES IN-REPLY-TO)])";

#[derive(serde::Deserialize, serde::Serialize)]
struct PageCursor {
    folder_id: Option<String>,
    top: usize,
    skip: usize,
}

fn encode_page_cursor(cursor: &PageCursor) -> String {
    let json = serde_json::to_vec(cursor).expect("serializing IMAP page cursor cannot fail");
    format!(
        "{PAGE_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

fn decode_page_cursor(cursor: &str) -> Result<PageCursor> {
    let encoded = cursor
        .strip_prefix(PAGE_CURSOR_PREFIX)
        .context("invalid IMAP page cursor prefix")?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .context("invalid IMAP page cursor encoding")?;
    serde_json::from_slice(&json).context("invalid IMAP page cursor payload")
}

/// Builds one IMAP SEARCH key without putting UTF-8 bytes in an RFC 3501
/// quoted string. The synchronous `imap` API accepts a raw command fragment,
/// including the CRLF separating a literal declaration from its octets.
fn search_key(field: &str, value: &str) -> String {
    if value.is_ascii() && !value.contains(['\r', '\n']) {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{field} \"{escaped}\"")
    } else {
        format!("CHARSET UTF-8 {field} {{{}}}\r\n{value}", value.len())
    }
}

/// A merged folder id is local to Aviary and contains the real IMAP mailbox
/// names. Message ids still embed one real source mailbox, so mutations keep
/// targeting the correct place on the server.
fn merged_folder_id(folders: &[String]) -> String {
    let json = serde_json::to_vec(folders).expect("serializing IMAP folder ids cannot fail");
    format!(
        "{MERGED_FOLDER_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

fn decode_merged_folder_id(id: &str) -> Option<Vec<String>> {
    let encoded = id.strip_prefix(MERGED_FOLDER_PREFIX)?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()?;
    let folders: Vec<String> = serde_json::from_slice(&json).ok()?;
    (!folders.is_empty()).then_some(folders)
}

/// Accept Aviary's inbox alias, literal mailbox names and virtual ids that
/// combine physical mailboxes sharing one special-folder role.
fn resolve_folders(id: Option<&str>) -> Vec<String> {
    match id {
        None => vec!["INBOX".to_string()],
        Some(id) if id.eq_ignore_ascii_case("inbox") => vec!["INBOX".to_string()],
        Some(id) => decode_merged_folder_id(id).unwrap_or_else(|| vec![id.to_string()]),
    }
}

fn merge_special_folders(
    existing: &mut MailFolder,
    incoming: MailFolder,
    incoming_preferred: bool,
) {
    let total = existing
        .total_item_count
        .saturating_add(incoming.total_item_count);
    let unread = existing
        .unread_item_count
        .saturating_add(incoming.unread_item_count);
    let mut existing_ids = resolve_folders(Some(&existing.id));
    let mut incoming_ids = resolve_folders(Some(&incoming.id));
    let mut merged = if incoming_preferred {
        incoming.clone()
    } else {
        existing.clone()
    };
    let mut ids = if incoming_preferred {
        incoming_ids.append(&mut existing_ids);
        incoming_ids
    } else {
        existing_ids.append(&mut incoming_ids);
        existing_ids
    };
    let preferred = ids.remove(0);
    ids.sort();
    ids.dedup();
    ids.retain(|id| id != &preferred);
    ids.insert(0, preferred);

    merged.id = merged_folder_id(&ids);
    merged.total_item_count = total;
    merged.unread_item_count = unread;
    *existing = merged;
}

fn make_id(folder: &str, uid: u32) -> String {
    format!("{}:{}", urlencoding::encode(folder), uid)
}

fn parse_id(id: &str) -> Result<(String, u32)> {
    let (f, u) = id
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("identifiant message invalide : {id}"))?;
    let folder = urlencoding::decode(f)
        .map_err(|e| anyhow!(tr!("imap-error-folder-decode", { error: e })))?
        .to_string();
    let uid = u.parse::<u32>().context(tr!("imap-error-uid-invalid"))?;
    Ok((folder, uid))
}

/// Build an `Account` straight from the IMAP credentials. We don't have a
/// `/me` endpoint to call against IMAP — the email is the username the user
/// typed at setup time, and the display name is what they put in the form.
pub async fn get_me(auth: &ImapAuth<'_>) -> Result<Account> {
    // Sanity-check the credentials by opening + closing a session. This is
    // what makes the IMAP connection error fire at setup time
    // instead of on the first real operation.
    with_session(auth, |_session| Ok(())).await?;
    let email = auth.email.to_string();
    Ok(Account {
        id: AccountId(email.clone()),
        email,
        display_name: auth.display_name.to_string(),
        tenant: auth.imap_host.to_string(),
        provider: Provider::Imap,
    })
}

pub async fn list_folders(auth: &ImapAuth<'_>) -> Result<Vec<MailFolder>> {
    with_session(auth, |session| {
        let names = session
            .list(Some(""), Some("*"))
            .context(tr!("technical-operation-failed", { operation: "LIST IMAP" }))?;

        // LIST exposes every mailbox on the server, including old folders
        // that another client has deliberately unsubscribed. Outlook and
        // most desktop clients show the active (LSUB) set instead. Keep the
        // authoritative attributes from LIST, but use LSUB to decide which
        // entries are visible. Empty/unsupported LSUB falls back to LIST so
        // a server with no subscription support cannot hide everything.
        let subscribed = session
            .lsub(Some(""), Some("*"))
            .ok()
            .filter(|entries| !entries.is_empty())
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.name().to_string())
                    .collect::<HashSet<_>>()
            });
        let mut out: Vec<MailFolder> = Vec::with_capacity(names.len());
        let mut special_folders: HashMap<&'static str, (usize, u8)> = HashMap::new();
        for entry in names.iter() {
            let attrs: Vec<String> = entry
                .attributes()
                .iter()
                .map(|a| format!("{a:?}").to_ascii_lowercase())
                .collect();
            // RFC 3501 \Noselect mailboxes are hierarchy nodes only — skip.
            if attrs.iter().any(|a| a.contains("noselect")) {
                continue;
            }
            let name = entry.name().to_string();
            if subscribed.as_ref().is_some_and(|active| {
                !name.eq_ignore_ascii_case("INBOX") && !active.contains(&name)
            }) {
                continue;
            }
            // STATUS works on most servers and avoids SELECTing every folder.
            let (total, unread) = match session.status(&name, "(MESSAGES UNSEEN)") {
                Ok(s) => (s.exists, s.unseen.unwrap_or(0)),
                Err(_) => (0, 0),
            };
            let well_known = well_known_for(&name, entry.delimiter(), &attrs);
            let parent_id = entry.delimiter().and_then(|delimiter| {
                name.rsplit_once(delimiter)
                    .map(|(parent, _)| parent.to_string())
            });
            let folder = MailFolder {
                id: name.clone(),
                display_name: mailbox_display_name(&name, entry.delimiter()),
                parent_id: if well_known.is_none() {
                    parent_id
                } else {
                    None
                },
                well_known_name: well_known.map(|folder| folder.alias.to_string()),
                total_item_count: total,
                unread_item_count: unread,
            };

            // Several clients can leave their own localized Drafts/Sent/Junk/
            // Trash mailboxes behind. Merge them into one virtual row whose
            // counters and message listing cover every physical mailbox. The
            // preferred RFC 6154/conventional mailbox stays first in the id,
            // which also makes it the destination for moves into this row.
            if let Some(known) = well_known {
                if let Some((index, priority)) = special_folders.get_mut(known.alias) {
                    let existing_physical = resolve_folders(Some(&out[*index].id));
                    let incoming_preferred = known.priority > *priority
                        || (known.priority == *priority && folder.id < existing_physical[0]);
                    log::debug!(
                        "merging duplicate IMAP folder {:?} into {:?} for {}",
                        folder.id,
                        existing_physical,
                        known.alias
                    );
                    merge_special_folders(&mut out[*index], folder, incoming_preferred);
                    *priority = (*priority).max(known.priority);
                    continue;
                }
                special_folders.insert(known.alias, (out.len(), known.priority));
            }
            out.push(folder);
        }
        // A special role can merge several physical mailboxes into one local
        // virtual row. Point children of any physical member at that virtual
        // id so they stay nested below the visible parent.
        let mut virtual_parents = HashMap::new();
        for folder in &out {
            if let Some(physical_ids) = decode_merged_folder_id(&folder.id) {
                for physical_id in physical_ids {
                    virtual_parents.insert(physical_id, folder.id.clone());
                }
            }
        }
        for folder in &mut out {
            if let Some(virtual_parent) = folder
                .parent_id
                .as_ref()
                .and_then(|parent| virtual_parents.get(parent))
            {
                folder.parent_id = Some(virtual_parent.clone());
            }
        }
        // Inbox first, then alphabetical.
        out.sort_by(|a, b| {
            let ka = a.well_known_name.as_deref() == Some("inbox");
            let kb = b.well_known_name.as_deref() == Some("inbox");
            kb.cmp(&ka)
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        Ok(out)
    })
    .await
}

/// Map an IMAP mailbox to one of Aviary's well-known folder aliases. We
/// look at LIST attributes (RFC 6154 \Sent, \Trash, …) first — they're the
/// authoritative signal — and fall back to common naming conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WellKnownFolder {
    alias: &'static str,
    priority: u8,
}

impl WellKnownFolder {
    const fn authoritative(alias: &'static str) -> Self {
        Self { alias, priority: 3 }
    }

    const fn conventional(alias: &'static str) -> Self {
        Self { alias, priority: 2 }
    }

    const fn localized(alias: &'static str) -> Self {
        Self { alias, priority: 1 }
    }
}

fn well_known_for(
    name: &str,
    delimiter: Option<&str>,
    attrs: &[String],
) -> Option<WellKnownFolder> {
    if name.eq_ignore_ascii_case("inbox") {
        return Some(WellKnownFolder::authoritative("inbox"));
    }
    if attrs.iter().any(|a| a.contains("sent")) {
        return Some(WellKnownFolder::authoritative("sentitems"));
    }
    if attrs.iter().any(|a| a.contains("drafts")) {
        return Some(WellKnownFolder::authoritative("drafts"));
    }
    if attrs.iter().any(|a| a.contains("trash")) {
        return Some(WellKnownFolder::authoritative("deleteditems"));
    }
    if attrs.iter().any(|a| a.contains("junk")) {
        return Some(WellKnownFolder::authoritative("junkemail"));
    }
    if attrs.iter().any(|a| a.contains("archive")) {
        return Some(WellKnownFolder::authoritative("archive"));
    }

    // RFC 6154 is optional. Match the decoded final path component so this
    // also works for servers whose personal namespace is `INBOX.` rather
    // than `/`, and for the localized names created by Outlook.
    let decoded = decode_mailbox_name(name);
    let leaf = delimiter
        .and_then(|separator| decoded.rsplit(separator).next())
        .unwrap_or(decoded.as_str())
        .trim()
        .to_lowercase();
    match leaf.as_str() {
        "sent" | "sent items" => Some(WellKnownFolder::conventional("sentitems")),
        "éléments envoyés" | "messages envoyés" | "envoyés" => {
            Some(WellKnownFolder::localized("sentitems"))
        }
        "draft" | "drafts" => Some(WellKnownFolder::conventional("drafts")),
        "brouillon" | "brouillons" => Some(WellKnownFolder::localized("drafts")),
        "trash" | "deleted" | "deleted items" => {
            Some(WellKnownFolder::conventional("deleteditems"))
        }
        "éléments supprimés" | "corbeille" => Some(WellKnownFolder::localized("deleteditems")),
        "junk" | "junk email" | "spam" => Some(WellKnownFolder::conventional("junkemail")),
        "courrier indésirable" | "indésirables" => Some(WellKnownFolder::localized("junkemail")),
        "archive" => Some(WellKnownFolder::conventional("archive")),
        "archives" => Some(WellKnownFolder::localized("archive")),
        "outbox" => Some(WellKnownFolder::conventional("outbox")),
        "boîte d'envoi" | "boîte d’envoi" => Some(WellKnownFolder::localized("outbox")),
        _ => None,
    }
}

/// Decode the modified UTF-7 representation used by IMAP mailbox names
/// (RFC 3501 section 5.1.3). Invalid shifted chunks are preserved verbatim:
/// a malformed server response should remain addressable and visible.
fn decode_mailbox_name(name: &str) -> String {
    let engine = GeneralPurpose::new(
        &alphabet::IMAP_MUTF7,
        GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    let mut decoded = String::with_capacity(name.len());
    let mut cursor = 0;

    while let Some(relative_amp) = name[cursor..].find('&') {
        let amp = cursor + relative_amp;
        decoded.push_str(&name[cursor..amp]);
        let Some(relative_end) = name[amp + 1..].find('-') else {
            decoded.push_str(&name[amp..]);
            return decoded;
        };
        let end = amp + 1 + relative_end;
        if end == amp + 1 {
            decoded.push('&');
        } else {
            let shifted = &name[amp + 1..end];
            let replacement = engine.decode(shifted).ok().and_then(|bytes| {
                (bytes.len() % 2 == 0).then(|| {
                    bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>()
                })
            });
            match replacement.and_then(|units| String::from_utf16(&units).ok()) {
                Some(text) => decoded.push_str(&text),
                None => decoded.push_str(&name[amp..=end]),
            }
        }
        cursor = end + 1;
    }
    decoded.push_str(&name[cursor..]);
    decoded
}

/// Encode a user-entered mailbox name for classic IMAP servers. The raw
/// encoded value remains the provider id; only `display_name` is decoded.
fn encode_mailbox_name(name: &str) -> String {
    fn flush_shifted(units: &mut Vec<u16>, output: &mut String) {
        if units.is_empty() {
            return;
        }
        let bytes = units
            .drain(..)
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let engine = GeneralPurpose::new(
            &alphabet::IMAP_MUTF7,
            GeneralPurposeConfig::new()
                .with_encode_padding(false)
                .with_decode_padding_mode(DecodePaddingMode::RequireNone),
        );
        output.push('&');
        output.push_str(&engine.encode(bytes));
        output.push('-');
    }

    let mut encoded = String::with_capacity(name.len());
    let mut shifted = Vec::new();
    for ch in name.chars() {
        if (' '..='~').contains(&ch) && ch != '&' {
            flush_shifted(&mut shifted, &mut encoded);
            encoded.push(ch);
        } else if ch == '&' {
            flush_shifted(&mut shifted, &mut encoded);
            encoded.push_str("&-");
        } else {
            shifted.extend(ch.encode_utf16(&mut [0; 2]).iter().copied());
        }
    }
    flush_shifted(&mut shifted, &mut encoded);
    encoded
}

fn mailbox_display_name(name: &str, delimiter: Option<&str>) -> String {
    let decoded = decode_mailbox_name(name);
    let Some(delimiter) = delimiter else {
        return decoded;
    };
    decoded
        .rsplit_once(delimiter)
        .map_or(decoded.clone(), |(_, leaf)| leaf.to_string())
}

/// Page of messages, newest-first. Pagination is by sequence number window
/// (`<n-skip-top+1>:<n-skip>`); since IMAP delivers messages with
/// monotonically increasing sequence numbers, this gives the user "the
/// latest 50, then the next 50…" exactly like the OAuth providers.
pub async fn list_folder_messages(
    auth: &ImapAuth<'_>,
    folder_id: Option<&str>,
    top: usize,
    skip: usize,
) -> Result<Vec<MessageHeader>> {
    let folders = resolve_folders(folder_id);
    with_session(auth, move |session| {
        if folders.len() == 1 {
            return list_physical_folder_messages(session, &folders[0], top, skip);
        }

        // To obtain the global page [skip, skip + top), at most that many
        // candidates are needed from each source mailbox. Merge them by date
        // afterwards; each header keeps a source-aware message id.
        let candidates_per_folder = top.saturating_add(skip);
        let mut out = Vec::new();
        for folder in &folders {
            out.extend(list_physical_folder_messages(
                session,
                folder,
                candidates_per_folder,
                0,
            )?);
        }
        out.sort_by_key(|h| std::cmp::Reverse(h.received));
        Ok(out.into_iter().skip(skip).take(top).collect())
    })
    .await
}

pub async fn list_folder_messages_page(
    auth: &ImapAuth<'_>,
    folder_id: Option<&str>,
    top: usize,
) -> Result<MessagePage> {
    let messages = list_folder_messages(auth, folder_id, top, 0).await?;
    let next = (messages.len() >= top && top > 0).then(|| {
        encode_page_cursor(&PageCursor {
            folder_id: folder_id.map(str::to_string),
            top,
            skip: messages.len(),
        })
    });
    Ok(MessagePage { messages, next })
}

fn list_physical_folder_messages(
    session: &mut ImapSession,
    folder: &str,
    top: usize,
    skip: usize,
) -> Result<Vec<MessageHeader>> {
    if top == 0 {
        return Ok(Vec::new());
    }
    let mailbox = session.select(folder).with_context(
        || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
    )?;
    let total = mailbox.exists;
    let skip = u32::try_from(skip).unwrap_or(u32::MAX);
    let top = u32::try_from(top).unwrap_or(u32::MAX);
    if total == 0 || skip >= total {
        return Ok(Vec::new());
    }
    let end = total - skip;
    let start = end.saturating_sub(top.saturating_sub(1)).max(1);
    let range = format!("{start}:{end}");
    let fetches = session
        .fetch(range, LISTING_FETCH)
        .context(tr!("technical-operation-failed", { operation: "FETCH (page)" }))?;
    let mut out: Vec<MessageHeader> = fetches
        .iter()
        .filter_map(|fetch| message_header_from_fetch(folder, fetch))
        .collect();
    // FETCH returns ascending sequence numbers; the UI expects newest first.
    out.sort_by_key(|header| std::cmp::Reverse(header.received));
    Ok(out)
}

/// Minimal incremental synchronization based on UIDVALIDITY/UIDNEXT.
/// Flag changes on recent messages are also reconciled by the normal page
/// loaded before this call.
pub async fn sync_folder_messages(
    auth: &ImapAuth<'_>,
    folder_id: Option<&str>,
    cursor: Option<&str>,
) -> Result<MailSyncPage> {
    let mut folders = resolve_folders(folder_id);
    // The normal page loader fully reconciles a merged virtual folder on
    // every refresh. A compound UIDVALIDITY/UIDNEXT cursor would add no data
    // here and would become invalid whenever the server's aliases change.
    if folders.len() != 1 {
        return Ok(MailSyncPage::default());
    }
    let folder = folders.remove(0);
    let cursor = cursor.map(str::to_string);
    with_session(auth, move |session| {
        let mailbox = session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        let validity = mailbox.uid_validity.unwrap_or(0);
        let next = mailbox.uid_next.unwrap_or(1);
        let new_cursor = format!("{validity}:{next}");
        let Some(cursor) = cursor else {
            return Ok(MailSyncPage {
                cursor: Some(new_cursor),
                ..MailSyncPage::default()
            });
        };
        let (old_validity, old_next) = cursor
            .split_once(':')
            .and_then(|(validity, next)| {
                Some((validity.parse::<u32>().ok()?, next.parse::<u32>().ok()?))
            })
            .ok_or_else(|| anyhow!("curseur IMAP invalide (410)"))?;
        if old_validity != validity {
            bail!(tr!("imap-error-uidvalidity-changed"));
        }
        let mut upserts = Vec::new();
        if old_next < next {
            let end = next.saturating_sub(1);
            let set = format!("{old_next}:{end}");
            let fetches = session.uid_fetch(&set, LISTING_FETCH).context(
                tr!("technical-operation-failed", { operation: "incremental UID FETCH" }),
            )?;
            upserts = fetches
                .iter()
                .filter_map(|fetch| message_header_from_fetch(&folder, fetch))
                .collect();
            upserts.sort_by_key(|header| std::cmp::Reverse(header.received));
        }
        Ok(MailSyncPage {
            upserts,
            cursor: Some(new_cursor),
            ..MailSyncPage::default()
        })
    })
    .await
}

pub async fn get_message(auth: &ImapAuth<'_>, id: &str) -> Result<Message> {
    let (folder, uid) = parse_id(id)?;
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        let fetches = session
            .uid_fetch(uid.to_string(), "(UID FLAGS INTERNALDATE RFC822)")
            .context(tr!("technical-operation-failed", { operation: "UID FETCH RFC822" }))?;
        let fetch = fetches
            .iter()
            .next()
            .ok_or_else(|| anyhow!("UID {uid} introuvable dans {folder}"))?;
        let raw = fetch
            .body()
            .ok_or_else(|| anyhow!("FETCH UID {uid} sans corps"))?;
        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| anyhow!("RFC822 illisible"))?;
        // Build the header straight from the parsed RFC822. This is the only
        // path where we have the full message in hand — `f.header()` is None
        // here because the FETCH attribute list asked for RFC822, not
        // BODY[HEADER], and IMAP fetches don't return the embedded headers
        // section unless explicitly requested.
        let mut header = message_header_from_parsed(&folder, uid, fetch, &parsed);
        let tags = header.tags.clone();
        let (body, format, inline_images, raw_body) = render_body(&parsed);
        let html_cids = raw_body
            .as_deref()
            .map(extract_cids_from_html)
            .unwrap_or_default();
        let attachments = collect_attachments(&parsed, &html_cids);
        header.has_attachments = !attachments.is_empty();
        let to = render_address_list(parsed.to());
        let cc = render_address_list(parsed.cc());
        let bcc = render_address_list(parsed.bcc());
        let is_draft = fetch
            .flags()
            .iter()
            .any(|fl| matches!(fl, imap::types::Flag::Draft));
        let draft_id = is_draft.then(|| header.id.clone());
        Ok(Message {
            header,
            body,
            format,
            inline_images,
            attachments,
            tags,
            raw_body,
            to,
            cc,
            bcc,
            draft_id,
            invitation: None,
        })
    })
    .await
}

/// Build a `MessageHeader` from a fully parsed RFC822 message + the IMAP
/// FETCH metadata (UID, flags, INTERNALDATE). Used by `get_message` where
/// the full body is already in memory.
fn message_header_from_parsed(
    folder: &str,
    uid: u32,
    fetch: &imap::types::Fetch,
    parsed: &mail_parser::Message<'_>,
) -> MessageHeader {
    let subject = parsed.subject().unwrap_or("").to_string();
    let from = render_address(parsed.from());
    let received = parsed
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .or_else(|| fetch.internal_date().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);
    let is_read = fetch
        .flags()
        .iter()
        .any(|fl| matches!(fl, imap::types::Flag::Seen));
    let is_flagged = fetch
        .flags()
        .iter()
        .any(|fl| matches!(fl, imap::types::Flag::Flagged));
    // mail_parser populates `Message.attachments` (indices) during parse;
    // a non-empty iterator means there's at least one part the library has
    // classified as a non-body attachment.
    let has_attachments = parsed.attachments().next().is_some();
    MessageHeader {
        id: make_id(folder, uid),
        account_id: AccountId::default(),
        subject,
        from,
        received,
        preview: parsed
            .body_text(0)
            .map(|t| t.chars().take(200).collect::<String>())
            .unwrap_or_default(),
        is_read,
        is_flagged,
        has_attachments,
        tags: custom_flags(fetch),
        last_action: last_action_from_flags(fetch.flags()),
        // IMAP flags are not timestamped.
        last_action_at: None,
        conversation_id: derived_conversation_id(parsed),
        internet_message_id: parsed.message_id().map(str::to_string),
    }
}

pub async fn delete_message(auth: &ImapAuth<'_>, id: &str) -> Result<()> {
    let (folder, uid) = parse_id(id)?;
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        // Try MOVE first (RFC 6851). Falls back to copy+\Deleted+EXPUNGE for
        // servers that don't advertise it. We keep this simple: try MOVE to
        // "Trash" and if that fails, mark deleted and EXPUNGE (which is
        // destructive but compatible).
        let trash = guess_trash_name(session).unwrap_or_else(|| "Trash".to_string());
        let mv = session.uid_mv(uid.to_string(), &trash);
        if mv.is_err() {
            session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .context(tr!("technical-operation-failed", { operation: "UID STORE \\Deleted" }))?;
            session
                .expunge()
                .context(tr!("technical-operation-failed", { operation: "EXPUNGE" }))?;
        }
        Ok(())
    })
    .await
}

fn guess_trash_name(session: &mut ImapSession) -> Option<String> {
    let names = session.list(Some(""), Some("*")).ok()?;
    for entry in names.iter() {
        let attrs: Vec<String> = entry
            .attributes()
            .iter()
            .map(|a| format!("{a:?}").to_ascii_lowercase())
            .collect();
        if attrs.iter().any(|a| a.contains("trash")) {
            return Some(entry.name().to_string());
        }
        let lower = entry.name().to_ascii_lowercase();
        if lower == "trash" || lower.contains("deleted") {
            return Some(entry.name().to_string());
        }
    }
    None
}

pub async fn set_flag(auth: &ImapAuth<'_>, id: &str, flagged: bool) -> Result<()> {
    let (folder, uid) = parse_id(id)?;
    let cmd = if flagged {
        "+FLAGS (\\Flagged)"
    } else {
        "-FLAGS (\\Flagged)"
    };
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session
            .uid_store(uid.to_string(), cmd)
            .context(tr!("technical-operation-failed", { operation: "UID STORE flag" }))?;
        Ok(())
    })
    .await
}

pub async fn mark_read(auth: &ImapAuth<'_>, id: &str, read: bool) -> Result<()> {
    let (folder, uid) = parse_id(id)?;
    let cmd = if read {
        "+FLAGS (\\Seen)"
    } else {
        "-FLAGS (\\Seen)"
    };
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session
            .uid_store(uid.to_string(), cmd)
            .context(tr!("technical-operation-failed", { operation: "UID STORE seen" }))?;
        Ok(())
    })
    .await
}

/// Renders a parsed query into RFC 3501 `SEARCH` keys.
///
/// IMAP has no attachment predicate, so `avec:pj` cannot be expressed and is
/// left to the caller's local filter. Dates use the protocol's `d-MMM-yyyy`
/// form, and `BEFORE`/`SINCE` are day-granular and half-open, matching
/// [`SearchQuery`]'s own semantics.
pub(super) fn search_keys(query: &SearchQuery) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for term in &query.from {
        keys.push(search_key("FROM", term));
    }
    for term in &query.to {
        keys.push(search_key("TO", term));
    }
    for term in &query.subject {
        keys.push(search_key("SUBJECT", term));
    }
    for term in &query.terms {
        keys.push(search_key("TEXT", term));
    }
    match query.is_read {
        Some(true) => keys.push("SEEN".into()),
        Some(false) => keys.push("UNSEEN".into()),
        None => {}
    }
    match query.is_flagged {
        Some(true) => keys.push("FLAGGED".into()),
        Some(false) => keys.push("UNFLAGGED".into()),
        None => {}
    }
    if let Some(before) = query.before {
        keys.push(format!("BEFORE {}", before.format("%d-%b-%Y")));
    }
    if let Some(after) = query.after {
        keys.push(format!("SINCE {}", after.format("%d-%b-%Y")));
    }
    if keys.is_empty() {
        // `SEARCH` needs at least one key; ALL keeps a flag-only query
        // meaningful instead of erroring.
        keys.push("ALL".into());
    }
    keys
}

/// Mailboxes an account-wide search walks before giving up.
///
/// IMAP has no cross-folder search: covering an account means `SELECT` +
/// `SEARCH` per mailbox. They all run inside a single session, so the cost is
/// one round-trip each rather than one connection each, but a server with
/// hundreds of mailboxes would still stall the query — hence the cap. The
/// local index has no such limit, and it answers first.
const MAX_SEARCHED_FOLDERS: usize = 12;

pub async fn search(
    auth: &ImapAuth<'_>,
    query: &SearchQuery,
    folder_id: Option<Option<&str>>,
    limit: usize,
) -> Result<Vec<MessageHeader>> {
    // IMAP joins keys with an implicit AND.
    let keys = search_keys(query).join(" ");
    let scoped: Option<Vec<String>> = folder_id.map(|folder| resolve_folders(folder));
    with_session(auth, move |session| {
        let folders = match scoped {
            Some(folders) => folders,
            // Account-wide: walk the mailbox list, inbox first so the most
            // likely hits arrive even if the cap cuts the tail off.
            None => {
                let mut names: Vec<String> = session
                    .list(Some(""), Some("*"))
                    .context(tr!("technical-operation-failed", { operation: "LIST IMAP" }))?
                    .iter()
                    .filter(|entry| {
                        !entry
                            .attributes()
                            .iter()
                            .any(|attr| format!("{attr:?}").to_lowercase().contains("noselect"))
                    })
                    .map(|entry| entry.name().to_string())
                    .collect();
                names.sort_by_key(|name| !name.eq_ignore_ascii_case("INBOX"));
                names.truncate(MAX_SEARCHED_FOLDERS);
                names
            }
        };

        let mut out: Vec<MessageHeader> = Vec::new();
        for folder in folders {
            if out.len() >= limit {
                break;
            }
            // One unreadable mailbox must not fail the whole search.
            if session.select(&folder).is_err() {
                continue;
            }
            let uids = match session.uid_search(&keys) {
                Ok(uids) => uids,
                Err(e) => {
                    log::warn!("IMAP SEARCH failed in {folder}: {e}");
                    continue;
                }
            };
            if uids.is_empty() {
                continue;
            }
            let mut sorted: Vec<u32> = uids.into_iter().collect();
            sorted.sort_by(|a, b| b.cmp(a));
            sorted.truncate(limit);
            let set = sorted
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fetches = session
                .uid_fetch(&set, LISTING_FETCH)
                .context(tr!("technical-operation-failed", { operation: "UID FETCH (search)" }))?;
            out.extend(
                fetches
                    .iter()
                    .filter_map(|f| message_header_from_fetch(&folder, f)),
            );
        }
        out.sort_by_key(|h| std::cmp::Reverse(h.received));
        out.truncate(limit);
        Ok(out)
    })
    .await
}

pub async fn list_from_sender(
    auth: &ImapAuth<'_>,
    email: &str,
    top: usize,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    let folder = "INBOX".to_string();
    let needle = email.to_string();
    let messages = with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        let uids = session
            .uid_search(search_key("FROM", &needle))
            .context(tr!("technical-operation-failed", { operation: "UID SEARCH FROM" }))?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let mut sorted: Vec<u32> = uids.into_iter().collect();
        sorted.sort_by(|a, b| b.cmp(a));
        sorted.truncate(top);
        let set = sorted
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetches = session
            .uid_fetch(&set, LISTING_FETCH)
            .context(tr!("technical-operation-failed", { operation: "UID FETCH (sender)" }))?;
        let mut out: Vec<MessageHeader> = fetches
            .iter()
            .filter_map(|f| message_header_from_fetch(&folder, f))
            .collect();
        out.sort_by_key(|h| std::cmp::Reverse(h.received));
        Ok(out)
    })
    .await?;
    Ok((messages, None))
}

/// RFC 3501 `SEARCH` disjunction matching every member of a thread rooted at
/// `conversation_id`.
///
/// `OR` is binary and written in prefix form, so three alternatives read
/// `OR OR k1 k2 k3` — that is `OR (OR k1 k2) k3`. Getting this shape wrong
/// does not fail loudly: the server would answer a differently-parenthesized
/// query, hence the test pinning it.
fn thread_search_keys(conversation_id: &str) -> String {
    format!(
        "OR OR {} {} {}",
        search_key("HEADER MESSAGE-ID", conversation_id),
        search_key("HEADER REFERENCES", conversation_id),
        search_key("HEADER IN-REPLY-TO", conversation_id),
    )
}

/// Mailboxes a thread lookup visits. A conversation lives in the inbox and
/// in Sent; anything further would cost a SELECT + SEARCH per mailbox on
/// every message the user opens.
const THREAD_LOOKUP_ALIASES: &[&str] = &["inbox", "sentitems"];

/// Rebuilds a thread from the root `Message-ID` that
/// [`derived_conversation_id`] hands out.
///
/// A member either *is* the root or points at it, so one disjunction covers
/// the thread: `MESSAGE-ID` matches the root itself, `REFERENCES` matches
/// every reply that carries the chain, and `IN-REPLY-TO` catches the mailers
/// that send no `References`. `HEADER` matching is a substring test in RFC
/// 3501, which is exactly what reading inside those lists requires.
///
/// The search is deliberately confined to the inbox and Sent (see
/// [`THREAD_LOOKUP_ALIASES`]): this runs on every message open, and a thread
/// whose replies were filed elsewhere simply comes back shorter rather than
/// making every open pay for a full-mailbox walk.
pub async fn list_thread(auth: &ImapAuth<'_>, conversation_id: &str) -> Result<Vec<MessageHeader>> {
    if conversation_id.is_empty() {
        return Ok(Vec::new());
    }
    let keys = thread_search_keys(conversation_id);
    let conversation_id = conversation_id.to_string();
    with_session(auth, move |session| {
        let mut folders: Vec<String> = Vec::new();
        for entry in session
            .list(Some(""), Some("*"))
            .context(tr!("technical-operation-failed", { operation: "LIST IMAP" }))?
            .iter()
        {
            let attrs: Vec<String> = entry
                .attributes()
                .iter()
                .map(|attr| format!("{attr:?}").to_ascii_lowercase())
                .collect();
            if attrs.iter().any(|attr| attr.contains("noselect")) {
                continue;
            }
            let name = entry.name().to_string();
            if well_known_for(&name, entry.delimiter(), &attrs)
                .is_some_and(|known| THREAD_LOOKUP_ALIASES.contains(&known.alias))
            {
                folders.push(name);
            }
        }
        // Duplicated localized Sent mailboxes are common; visiting each once
        // is enough, and the inbox comes first so the thread reads in the
        // order the user expects even if a later SELECT fails.
        folders.sort_by_key(|name| !name.eq_ignore_ascii_case("INBOX"));
        folders.dedup();

        let mut out: Vec<MessageHeader> = Vec::new();
        for folder in folders {
            // One unreadable mailbox must not empty the whole thread.
            if session.select(&folder).is_err() {
                continue;
            }
            let uids = match session.uid_search(&keys) {
                Ok(uids) => uids,
                Err(e) => {
                    log::warn!("IMAP thread search failed in {folder}: {e}");
                    continue;
                }
            };
            if uids.is_empty() {
                continue;
            }
            let set = uids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let fetches = session
                .uid_fetch(&set, LISTING_FETCH)
                .context(tr!("technical-operation-failed", { operation: "UID FETCH (thread)" }))?;
            out.extend(
                fetches
                    .iter()
                    .filter_map(|fetch| message_header_from_fetch(&folder, fetch)),
            );
        }
        // `HEADER` is a substring test, so a message-id that happens to
        // contain another one would slip in. Re-deriving the thread from the
        // fetched headers is the same rule the listing applies, and drops it.
        out.retain(|header| header.conversation_id.as_deref() == Some(conversation_id.as_str()));
        out.sort_by_key(|header| std::cmp::Reverse(header.received));
        Ok(out)
    })
    .await
}

pub async fn fetch_messages_page(
    auth: &ImapAuth<'_>,
    url: &str,
) -> Result<(Vec<MessageHeader>, Option<String>)> {
    let cursor = decode_page_cursor(url)?;
    let messages =
        list_folder_messages(auth, cursor.folder_id.as_deref(), cursor.top, cursor.skip).await?;
    let next = (messages.len() >= cursor.top && cursor.top > 0).then(|| {
        encode_page_cursor(&PageCursor {
            folder_id: cursor.folder_id,
            top: cursor.top,
            skip: cursor.skip.saturating_add(messages.len()),
        })
    });
    Ok((messages, next))
}

// -----------------------------------------------------------------------------
// FETCH → MessageHeader / Message bodies

pub(super) fn message_header_from_fetch(
    folder: &str,
    f: &imap::types::Fetch,
) -> Option<MessageHeader> {
    let uid = f.uid?;
    let header_bytes = f.header()?;
    let parsed = MessageParser::default().parse_headers(header_bytes)?;
    let subject = parsed.subject().unwrap_or("").to_string();
    let from = render_address(parsed.from());
    let received = parsed
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .or_else(|| f.internal_date().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);
    let is_read = f
        .flags()
        .iter()
        .any(|fl| matches!(fl, imap::types::Flag::Seen));
    let is_flagged = f
        .flags()
        .iter()
        .any(|fl| matches!(fl, imap::types::Flag::Flagged));
    Some(MessageHeader {
        id: make_id(folder, uid),
        account_id: AccountId::default(),
        subject,
        from,
        received,
        // Without a body fetch we can't compute a preview. The viewer will
        // show the full body once the user opens the message.
        preview: String::new(),
        is_read,
        is_flagged,
        // Listing path doesn't fetch BODYSTRUCTURE, so we can't tell at this
        // stage. The icon will appear once the message is opened and we
        // backfill `has_attachments` on the displayed row.
        has_attachments: false,
        tags: custom_flags(f),
        last_action: last_action_from_flags(f.flags()),
        // IMAP flags are not timestamped.
        last_action_at: None,
        conversation_id: derived_conversation_id(&parsed),
        internet_message_id: parsed.message_id().map(str::to_string),
    })
}

/// First entry of a `Message-ID` list header, brackets already stripped by
/// mail-parser. A single-id header parses as `Text`, several as `TextList`.
fn first_message_id(value: &mail_parser::HeaderValue<'_>) -> Option<String> {
    match value {
        mail_parser::HeaderValue::Text(text) => Some(text.as_ref()),
        mail_parser::HeaderValue::TextList(list) => list.first().map(|id| id.as_ref()),
        _ => None,
    }
    .map(str::trim)
    .filter(|id| !id.is_empty())
    .map(str::to_string)
}

/// Thread identifier for a protocol that has none.
///
/// The id is the **root** `Message-ID` of the thread, which RFC 5322 puts
/// first in `References`. That choice matters more than it looks: it is
/// computable from one message's own headers, so every page of a listing
/// derives the same id for the same thread without any cross-page state —
/// which a union-find over `In-Reply-To` chains could not promise.
///
/// Two known imprecisions, both deliberate:
/// - a reply carrying only `In-Reply-To` (no `References`) is keyed on its
///   parent rather than the root, so a deep branch of such a thread splits
///   off. Mailers that omit `References` are rare.
/// - there is **no fallback on the normalized subject**. It would merge two
///   unrelated exchanges that happen to share a subject ("Re: contrat"), and
///   silently merging conversations is worse than leaving them apart.
fn derived_conversation_id(parsed: &mail_parser::Message<'_>) -> Option<String> {
    first_message_id(parsed.references())
        .or_else(|| first_message_id(parsed.in_reply_to()))
        .or_else(|| parsed.message_id().map(str::to_string))
        .filter(|id| !id.is_empty())
}

fn custom_flags(fetch: &imap::types::Fetch) -> Vec<String> {
    fetch
        .flags()
        .iter()
        .filter_map(|flag| match flag {
            imap::types::Flag::Custom(name) if !super::tags::is_system_keyword(name) => {
                Some(name.to_string())
            }
            _ => None,
        })
        .collect()
}

/// Derives the latest reply/forward action from IMAP flags: `\Answered`
/// (RFC 3501) and the `$Forwarded` keyword (RFC 5788, set by
/// Thunderbird/Dovecot/etc.). If both are present, displays the
/// transfert — IMAP ne date pas ses flags, impossible de savoir lequel est
/// most recent one.
fn last_action_from_flags(flags: &[imap::types::Flag<'_>]) -> Option<LastAction> {
    let forwarded = flags.iter().any(|fl| {
        matches!(fl, imap::types::Flag::Custom(name) if name.eq_ignore_ascii_case("$Forwarded"))
    });
    if forwarded {
        return Some(LastAction::Forwarded);
    }
    flags
        .iter()
        .any(|fl| matches!(fl, imap::types::Flag::Answered))
        .then_some(LastAction::Replied)
}

/// Sets the flag matching `action` on the message (`\Answered` or
/// `$Forwarded`). Best-effort : certains serveurs refusent les keywords
/// (pas de `\*` dans PERMANENTFLAGS) — l'appelant se contente de logguer.
pub async fn note_last_action(auth: &ImapAuth<'_>, id: &str, action: LastAction) -> Result<()> {
    let (folder, uid) = parse_id(id)?;
    let cmd = match action {
        LastAction::Replied | LastAction::RepliedAll => "+FLAGS (\\Answered)",
        LastAction::Forwarded => "+FLAGS ($Forwarded)",
    };
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session
            .uid_store(uid.to_string(), cmd)
            .context(tr!("technical-operation-failed", { operation: "UID STORE last action" }))?;
        Ok(())
    })
    .await
}

fn render_address(addr: Option<&Address<'_>>) -> String {
    let Some(addr) = addr else {
        return String::new();
    };
    let Some(first) = addr.first() else {
        return String::new();
    };
    let email = first.address.as_deref().unwrap_or("");
    let name = first.name.as_deref().unwrap_or("");
    match (name.is_empty(), email.is_empty()) {
        (false, false) => format!("{name} <{email}>"),
        (true, false) => email.to_string(),
        (false, true) => name.to_string(),
        (true, true) => String::new(),
    }
}

/// Render every address in an RFC 5322 list header (To, Cc) as its own
/// `"Name <addr>"` string. Drafts re-open with these in the compose so
/// recipient lists round-trip cleanly.
fn render_address_list(addr: Option<&Address<'_>>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    addr.iter()
        .filter_map(|a| {
            let email = a.address.as_deref().unwrap_or("");
            let name = a.name.as_deref().unwrap_or("");
            match (name.is_empty(), email.is_empty()) {
                (false, false) => Some(format!("{name} <{email}>")),
                (true, false) => Some(email.to_string()),
                (false, true) => Some(name.to_string()),
                (true, true) => None,
            }
        })
        .collect()
}

/// Pick a "best" body part out of a multipart/parsed message and convert
/// HTML → Markdown so the viewer renders consistently with Graph/Gmail.
/// Inline images come back keyed by their `Content-ID`, with the `cid:`
/// references rewritten to Aviary's in-memory image URI scheme.
/// Walk the parsed message via mail-parser's own `attachments()` iterator so
/// ids retain the library's stable ordering. The iterator deliberately
/// includes binary `inline` parts; those referenced by the HTML body are
/// filtered out below because they live in `Message::inline_images`.
fn collect_attachments(msg: &mail_parser::Message<'_>, html_cids: &[String]) -> Vec<Attachment> {
    let mut out = Vec::new();
    for (idx, part) in msg.attachments().enumerate() {
        // mail-parser deliberately includes binary `inline` MIME parts in its
        // attachment iterator. If the HTML body references this part's
        // Content-ID, it belongs exclusively in `Message::inline_images` and
        // must not also appear as a downloadable file.
        if part.content_id().is_some_and(|content_id| {
            html_cids
                .iter()
                .any(|reference| cid_matches(reference, content_id))
        }) {
            continue;
        }
        let size = match &part.body {
            mail_parser::PartType::Binary(bytes) | mail_parser::PartType::InlineBinary(bytes) => {
                bytes.len() as u64
            }
            mail_parser::PartType::Text(text) | mail_parser::PartType::Html(text) => {
                text.len() as u64
            }
            _ => continue,
        };
        let mime = part
            .content_type()
            .map(|c| {
                let main = c.ctype();
                let sub = c.subtype().unwrap_or("");
                if sub.is_empty() {
                    main.to_string()
                } else {
                    format!("{main}/{sub}")
                }
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let filename = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("piece-jointe-{idx}"));
        out.push(Attachment {
            id: format!("part:{idx}"),
            filename,
            mime,
            size,
            bytes: None,
        });
    }
    out
}

pub async fn fetch_attachment(
    auth: &ImapAuth<'_>,
    message_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>> {
    let (folder, uid) = parse_id(message_id)?;
    let index = attachment_id
        .strip_prefix("part:")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| anyhow!(tr!("attachment-error-invalid-id")))?;
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        // IMAP servers expose individual MIME section identifiers through
        // BODYSTRUCTURE, but the mail-parser attachment ordering is what the
        // current message model persists. Use PEEK here so a download never
        // changes read state; a future BODYSTRUCTURE model can narrow this to
        // one section without changing the runtime protocol.
        let fetches = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .context(tr!("technical-operation-failed", {
                operation: "UID FETCH BODY.PEEK[]"
            }))?;
        let raw = fetches
            .iter()
            .next()
            .and_then(imap::types::Fetch::body)
            .ok_or_else(|| anyhow!(tr!("attachment-error-not-found")))?;
        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| anyhow!(tr!("attachment-error-not-found")))?;
        let part = parsed
            .attachments()
            .nth(index)
            .ok_or_else(|| anyhow!(tr!("attachment-error-not-found")))?;
        match &part.body {
            mail_parser::PartType::Binary(bytes) | mail_parser::PartType::InlineBinary(bytes) => {
                Ok(bytes.to_vec())
            }
            mail_parser::PartType::Text(text) | mail_parser::PartType::Html(text) => {
                Ok(text.as_bytes().to_vec())
            }
            _ => Err(anyhow!(tr!("attachment-error-not-found"))),
        }
    })
    .await
}

fn render_body(
    msg: &mail_parser::Message<'_>,
) -> (String, BodyFormat, Vec<InlineImage>, Option<String>) {
    // Collect inline images first so we can rewrite cid: refs in the HTML body.
    let mut inline_images: Vec<InlineImage> = Vec::new();
    for part in msg.parts.iter() {
        if let PartType::Binary(bytes) | PartType::InlineBinary(bytes) = &part.body {
            let cid = match part.content_id() {
                Some(id) => id.trim_matches(|c| c == '<' || c == '>').to_string(),
                None => continue,
            };
            let mime = part
                .content_type()
                .map(|c| {
                    let main = c.ctype();
                    let sub = c.subtype().unwrap_or("");
                    if sub.is_empty() {
                        main.to_string()
                    } else {
                        format!("{main}/{sub}")
                    }
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            inline_images.push(InlineImage {
                cid,
                mime,
                bytes: bytes.to_vec(),
            });
        }
    }

    // Prefer the HTML body — converted to Markdown — and fall back to plain
    // text when no HTML part is present.
    if let Some(html) = msg.body_html(0) {
        let raw_html = html.to_string();
        let md = convert_email_html(&html);
        let md = inline_images.iter().fold(md, |acc, img| {
            acc.replace(
                &format!("cid:{}", img.cid),
                &format!("bytes://cid-{}", img.cid),
            )
        });
        return (
            collapse_blank_lines(&md),
            BodyFormat::Markdown,
            inline_images,
            Some(raw_html),
        );
    }
    if let Some(text) = msg.body_text(0) {
        return (text.to_string(), BodyFormat::Text, inline_images, None);
    }
    (String::new(), BodyFormat::Text, inline_images, None)
}

pub async fn create_folder(
    auth: &ImapAuth<'_>,
    name: String,
    parent_id: Option<&str>,
) -> Result<crate::model::MailFolder> {
    let encoded_leaf = encode_mailbox_name(&name);
    let parent_id = parent_id.map(|id| {
        resolve_folders(Some(id))
            .into_iter()
            .next()
            .unwrap_or_else(|| id.to_string())
    });
    let parent_for_create = parent_id.clone();
    let leaf_for_create = encoded_leaf.clone();
    let id = with_session(auth, move |session| {
        let delimiter = session
            .list(Some(""), Some(""))
            .ok()
            .and_then(|entries| {
                entries
                    .iter()
                    .next()
                    .and_then(|entry| entry.delimiter().map(str::to_string))
            })
            .unwrap_or_else(|| "/".to_string());
        let created = parent_for_create.as_deref().map_or_else(
            || leaf_for_create.clone(),
            |parent| format!("{parent}{delimiter}{leaf_for_create}"),
        );
        session.create(&created).with_context(
            || tr!("technical-operation-failed", { operation: format!("CREATE {created}") }),
        )?;
        // Keep newly created folders visible when this account uses IMAP
        // subscriptions. Some servers preserve CREATE and SUBSCRIBE as two
        // deliberately separate operations.
        if let Err(error) = session.subscribe(&created) {
            log::warn!("created IMAP folder could not be subscribed: {error}");
        }
        Ok(created)
    })
    .await?;
    Ok(crate::model::MailFolder {
        id,
        display_name: name,
        parent_id,
        well_known_name: None,
        total_item_count: 0,
        unread_item_count: 0,
    })
}

pub async fn rename_folder(auth: &ImapAuth<'_>, id: String, new_name: String) -> Result<String> {
    let from = resolve_folders(Some(&id)).into_iter().next().unwrap_or(id);
    let encoded_leaf = encode_mailbox_name(&new_name);
    with_session(auth, move |session| {
        let delimiter = session
            .list(Some(""), Some(""))
            .ok()
            .and_then(|entries| {
                entries
                    .iter()
                    .next()
                    .and_then(|entry| entry.delimiter().map(str::to_string))
            })
            .unwrap_or_else(|| "/".to_string());
        let to = from.rsplit_once(&delimiter).map_or_else(
            || encoded_leaf.clone(),
            |(parent, _)| format!("{parent}{delimiter}{encoded_leaf}"),
        );
        session.rename(&from, &to).with_context(
            || tr!("technical-operation-failed", { operation: format!("RENAME {from} → {to}") }),
        )?;
        // RFC 3501 keeps subscriptions separate from RENAME. Move the
        // subscription as well or the renamed folder disappears from LSUB.
        if let Err(error) = session.subscribe(&to) {
            log::warn!("renamed IMAP folder could not be subscribed: {error}");
        }
        if let Err(error) = session.unsubscribe(&from) {
            log::warn!("old IMAP folder subscription could not be removed: {error}");
        }
        Ok(to)
    })
    .await
}

pub async fn delete_folder(auth: &ImapAuth<'_>, id: String) -> Result<()> {
    let target = id;
    with_session(auth, move |session| {
        session.delete(&target).with_context(
            || tr!("technical-operation-failed", { operation: format!("DELETE {target}") }),
        )?;
        if let Err(error) = session.unsubscribe(&target) {
            log::warn!("deleted IMAP folder subscription could not be removed: {error}");
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod folder_name_tests {
    use super::{
        decode_mailbox_name, decode_merged_folder_id, encode_mailbox_name, mailbox_display_name,
        merge_special_folders, resolve_folders, search_key, well_known_for,
    };
    use crate::model::MailFolder;

    #[test]
    fn non_ascii_search_uses_utf8_literal_octet_length() {
        assert_eq!(
            search_key("TEXT", "réunion"),
            "CHARSET UTF-8 TEXT {8}\r\nréunion"
        );
        assert_eq!(search_key("TEXT", "C#"), "TEXT \"C#\"");
    }

    #[test]
    fn decodes_modified_utf7_mailbox_names() {
        assert_eq!(
            decode_mailbox_name("INBOX.&AMk-l&AOk-ments envoy&AOk-s"),
            "INBOX.Éléments envoyés"
        );
        assert_eq!(decode_mailbox_name("A &- B"), "A & B");
        assert_eq!(
            decode_mailbox_name("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
    }

    #[test]
    fn modified_utf7_round_trips_unicode_and_ampersands() {
        for name in [
            "Éléments envoyés",
            "Courrier indésirable",
            "Projets & clients/日本語",
        ] {
            assert_eq!(decode_mailbox_name(&encode_mailbox_name(name)), name);
        }
    }

    #[test]
    fn display_name_decodes_and_hides_the_personal_namespace() {
        assert_eq!(
            mailbox_display_name("INBOX.&AMk-l&AOk-ments supprim&AOk-s", Some(".")),
            "Éléments supprimés"
        );
        assert_eq!(mailbox_display_name("Projects/Client", Some("/")), "Client");
    }

    #[test]
    fn recognizes_special_folders_with_dot_delimiter_and_localized_names() {
        let no_attrs = Vec::new();
        assert_eq!(
            well_known_for("INBOX.INBOX.Junk", Some("."), &no_attrs).map(|folder| folder.alias),
            Some("junkemail")
        );
        assert_eq!(
            well_known_for("INBOX.Trash", Some("."), &no_attrs).map(|folder| folder.alias),
            Some("deleteditems")
        );
        assert_eq!(
            well_known_for("INBOX.&AMk-l&AOk-ments envoy&AOk-s", Some("."), &no_attrs,)
                .map(|folder| folder.alias),
            Some("sentitems")
        );
    }

    #[test]
    fn conventional_names_win_over_localized_duplicates_without_special_use_flags() {
        let no_attrs = Vec::new();
        let drafts = well_known_for("Drafts", Some("."), &no_attrs).unwrap();
        let localized_drafts = well_known_for("INBOX.Brouillons", Some("."), &no_attrs).unwrap();
        assert_eq!(drafts.alias, localized_drafts.alias);
        assert!(drafts.priority > localized_drafts.priority);

        let trash = well_known_for("INBOX.INBOX.Trash", Some("."), &no_attrs).unwrap();
        let localized_trash =
            well_known_for("INBOX.&AMk-l&AOk-ments supprim&AOk-s", Some("."), &no_attrs).unwrap();
        assert_eq!(trash.alias, localized_trash.alias);
        assert!(trash.priority > localized_trash.priority);
    }

    #[test]
    fn duplicate_special_folders_merge_ids_and_counters() {
        let mut localized = MailFolder {
            id: "INBOX.&AMk-l&AOk-ments supprim&AOk-s".to_string(),
            display_name: "Éléments supprimés".to_string(),
            parent_id: None,
            well_known_name: Some("deleteditems".to_string()),
            total_item_count: 25_000,
            unread_item_count: 24_767,
        };
        let conventional = MailFolder {
            id: "INBOX.INBOX.Trash".to_string(),
            display_name: "INBOX.Trash".to_string(),
            parent_id: None,
            well_known_name: Some("deleteditems".to_string()),
            total_item_count: 4,
            unread_item_count: 3,
        };

        merge_special_folders(&mut localized, conventional, true);

        assert_eq!(localized.total_item_count, 25_004);
        assert_eq!(localized.unread_item_count, 24_770);
        assert_eq!(
            decode_merged_folder_id(&localized.id).unwrap(),
            vec![
                "INBOX.INBOX.Trash".to_string(),
                "INBOX.&AMk-l&AOk-ments supprim&AOk-s".to_string(),
            ]
        );
        assert_eq!(resolve_folders(Some(&localized.id)).len(), 2);
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::{collect_attachments, extract_cids_from_html, render_body, MessageParser};

    #[test]
    fn cid_image_is_not_duplicated_in_regular_attachments() {
        let raw = br#"From: Contact A <contact-a@example.test>
To: Contact B <contact-b@example.test>
Subject: Synthetic message
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="outer"

--outer
Content-Type: multipart/related; boundary="related"

--related
Content-Type: text/html; charset=utf-8

<html><body><p>Body</p><img src="cid:inline-mark@example.test"></body></html>
--related
Content-Type: image/png
Content-ID: <inline-mark@example.test>
Content-Disposition: inline; filename="inline-mark.png"
Content-Transfer-Encoding: base64

iVBORw0KGgo=
--related--
--outer
Content-Type: application/pdf
Content-Disposition: attachment; filename="report.pdf"
Content-Transfer-Encoding: base64

JVBERi0=
--outer--
"#;
        let parsed = MessageParser::default()
            .parse(raw.as_slice())
            .expect("synthetic MIME message");
        let (_, _, inline_images, raw_body) = render_body(&parsed);
        let html_cids = extract_cids_from_html(raw_body.as_deref().expect("HTML body"));
        let attachments = collect_attachments(&parsed, &html_cids);

        assert_eq!(inline_images.len(), 1);
        assert_eq!(inline_images[0].cid, "inline-mark@example.test");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "report.pdf");
        // The id retains its position in mail-parser's unfiltered attachment
        // iterator, so lazy fetching still selects the regular file.
        assert_eq!(attachments[0].id, "part:1");
    }
}

/// Provider-agnostic move aliases the UI may pass instead of a mailbox id,
/// paired with the error shown when the server offers no such mailbox.
///
/// IMAP has no well-known folders of its own — the names are inferred from
/// `LIST` attributes and localized display names (see `well_known_for`) — so an
/// alias can only be resolved against the account's own folder list. `inbox` is
/// absent on purpose: `resolve_folders` already maps it to `INBOX`, which every
/// server has.
fn well_known_move_alias(target: &str) -> Option<(&'static str, gpui::SharedString)> {
    if target.eq_ignore_ascii_case(crate::providers::ARCHIVE_FOLDER_ALIAS) {
        Some(("archive", tr!("notify-archive-unavailable")))
    } else if target.eq_ignore_ascii_case(crate::providers::JUNK_FOLDER_ALIAS) {
        Some(("junkemail", tr!("notify-junk-unavailable")))
    } else {
        None
    }
}

/// Move a message to another mailbox. Tries `UID MOVE` (RFC 6851) and falls
/// back to copy + `\Deleted` + EXPUNGE on servers that don't advertise it.
/// IMAP changes the UID across mailboxes, so on success we return the freshly
/// minted message id (`<encoded_target>:<new_uid>`) — but only when MOVE
/// surfaces it; the COPY+EXPUNGE fallback gives no easy way to learn the new
/// UID, so we return `None` and let the caller refresh.
pub async fn move_message(
    auth: &ImapAuth<'_>,
    id: &str,
    target_folder_id: &str,
) -> Result<Option<String>> {
    let (folder, uid) = parse_id(id)?;
    let resolved_alias;
    let target_folder_id = match well_known_move_alias(target_folder_id) {
        Some((well_known, unavailable)) => {
            resolved_alias = list_folders(auth)
                .await?
                .into_iter()
                .find(|folder| folder.well_known_name.as_deref() == Some(well_known))
                .map(|folder| folder.id)
                .context(unavailable)?;
            resolved_alias.as_str()
        }
        None => target_folder_id,
    };
    // Moving into a virtual merged row targets its preferred physical
    // mailbox (the first id, selected while building the folder list).
    let target = resolve_folders(Some(target_folder_id))
        .into_iter()
        .next()
        .unwrap_or_else(|| "INBOX".to_string());
    let target_for_session = target.clone();
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        let mv = session.uid_mv(uid.to_string(), &target_for_session);
        if mv.is_err() {
            session
                .uid_copy(uid.to_string(), &target_for_session)
                .with_context(|| {
                    tr!("technical-operation-failed", {
                        operation: format!("UID COPY → {target_for_session}")
                    })
                })?;
            session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .context(tr!("technical-operation-failed", { operation: "UID STORE \\Deleted" }))?;
            session
                .expunge()
                .context(tr!("technical-operation-failed", { operation: "EXPUNGE" }))?;
        }
        Ok(())
    })
    .await?;
    // We didn't track the new UID; the caller will reload the destination
    // folder if it's the active one.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{derived_conversation_id, search_keys, thread_search_keys, LISTING_FETCH};
    use crate::search_query::SearchQuery;
    use mail_parser::MessageParser;

    fn conversation_of(headers: &str) -> Option<String> {
        let parsed = MessageParser::default()
            .parse_headers(headers.as_bytes())
            .expect("synthetic headers parse");
        derived_conversation_id(&parsed)
    }

    /// Every message of a thread must derive the *same* id from its own
    /// headers alone — that is what lets page 2 of a listing group with page 1
    /// without any shared state.
    #[test]
    fn thread_members_derive_the_root_message_id() {
        let root = conversation_of(
            "From: Contact A <contact-a@example.test>\r\n\
             Message-ID: <root@example.test>\r\n\
             Subject: Contrat\r\n\r\n",
        );
        assert_eq!(root.as_deref(), Some("root@example.test"));

        // A direct reply: References holds the root, oldest first.
        let reply = conversation_of(
            "From: Contact B <contact-b@example.test>\r\n\
             Message-ID: <reply-1@example.test>\r\n\
             In-Reply-To: <root@example.test>\r\n\
             References: <root@example.test>\r\n\
             Subject: Re: Contrat\r\n\r\n",
        );
        assert_eq!(reply, root);

        // Deeper in the thread the chain grows, but its head does not move.
        let deep = conversation_of(
            "From: Contact A <contact-a@example.test>\r\n\
             Message-ID: <reply-2@example.test>\r\n\
             In-Reply-To: <reply-1@example.test>\r\n\
             References: <root@example.test> <reply-1@example.test>\r\n\
             Subject: Re: Contrat\r\n\r\n",
        );
        assert_eq!(deep, root);
    }

    /// Without `References`, `In-Reply-To` is the only chaining information
    /// available. A reply to the root still lands in the right thread.
    #[test]
    fn in_reply_to_alone_still_chains_onto_the_root() {
        let reply = conversation_of(
            "From: Contact B <contact-b@example.test>\r\n\
             Message-ID: <reply-1@example.test>\r\n\
             In-Reply-To: <root@example.test>\r\n\r\n",
        );
        assert_eq!(reply.as_deref(), Some("root@example.test"));
    }

    /// The subject is deliberately never used: two unrelated exchanges sharing
    /// one must stay apart.
    #[test]
    fn identical_subjects_do_not_merge_unrelated_messages() {
        let first =
            conversation_of("Message-ID: <first@example.test>\r\nSubject: Re: Contrat\r\n\r\n");
        let second =
            conversation_of("Message-ID: <second@example.test>\r\nSubject: Re: Contrat\r\n\r\n");
        assert_ne!(first, second);
        assert!(first.is_some() && second.is_some());
    }

    /// A message without any identifier cannot be threaded, and must not fall
    /// back to a shared placeholder that would pile unrelated mail together.
    #[test]
    fn missing_identifiers_yield_no_thread() {
        assert_eq!(conversation_of("Subject: Sans identifiant\r\n\r\n"), None);
    }

    /// A thread member either *is* the root or points at it, and RFC 3501
    /// writes `OR` in binary prefix form — so three alternatives nest as
    /// `OR (OR a b) c`. A wrong shape is answered by the server rather than
    /// rejected, which is precisely why it is pinned here.
    #[test]
    fn thread_search_covers_the_root_and_both_chaining_headers() {
        let keys = thread_search_keys("root@example.test");
        assert_eq!(
            keys,
            "OR OR HEADER MESSAGE-ID \"root@example.test\" \
             HEADER REFERENCES \"root@example.test\" \
             HEADER IN-REPLY-TO \"root@example.test\""
        );
    }

    /// Deriving a thread is only possible if the listing actually asks for the
    /// chaining headers.
    #[test]
    fn listing_fetch_requests_the_threading_headers() {
        assert!(LISTING_FETCH.contains("REFERENCES"), "{LISTING_FETCH}");
        assert!(LISTING_FETCH.contains("IN-REPLY-TO"), "{LISTING_FETCH}");
        assert!(LISTING_FETCH.contains("MESSAGE-ID"), "{LISTING_FETCH}");
    }

    /// IMAP joins keys with an implicit AND, and each value has to go through
    /// `search_key` so a non-ASCII term becomes a literal instead of breaking
    /// the command.
    #[test]
    fn search_keys_render_rfc3501_predicates() {
        let keys = search_keys(&SearchQuery::parse(
            "de:alice objet:contrat est:non-lu depuis:2026-03-01 avant:2026-03-15 facture",
        ));
        assert!(keys.contains(&"FROM \"alice\"".to_string()), "{keys:?}");
        assert!(
            keys.contains(&"SUBJECT \"contrat\"".to_string()),
            "{keys:?}"
        );
        assert!(keys.contains(&"TEXT \"facture\"".to_string()), "{keys:?}");
        assert!(keys.contains(&"UNSEEN".to_string()), "{keys:?}");
        assert!(keys.contains(&"SINCE 01-Mar-2026".to_string()), "{keys:?}");
        assert!(keys.contains(&"BEFORE 15-Mar-2026".to_string()), "{keys:?}");
    }

    /// IMAP has no attachment predicate: the operator must simply not appear,
    /// leaving it to the caller's local filter rather than producing a key the
    /// server would reject.
    #[test]
    fn attachment_operator_is_dropped_rather_than_faked() {
        let keys = search_keys(&SearchQuery::parse("avec:pj"));
        assert_eq!(keys, vec!["ALL".to_string()]);
    }

    /// A non-ASCII term must go out as a literal, not inside a quoted string.
    #[test]
    fn non_ascii_terms_use_a_literal() {
        let keys = search_keys(&SearchQuery::parse("réunion"));
        assert!(keys[0].starts_with("CHARSET UTF-8 TEXT {"), "{keys:?}");
    }
}
