//! SMTP send via lettre. Builds a `multipart/related` MIME message when
//! inline images are present (so the HTML body's `cid:` refs resolve at
//! the receiving MUA), or a single-part body otherwise.
//!
//! SMTP only delivers. What a mail client owes its user beyond that is done
//! here, around the delivery: a `Message-ID` of our own (so the message can
//! be found again), the threading headers of a reply (so it stays in its
//! conversation — at the recipient's, and in Aviary, whose IMAP threads are
//! derived from them), and a copy filed in the Sent mailbox, which a plain
//! Dovecot/Postfix setup does not keep by itself.

use crate::auth::NetSecurity;
use crate::providers::{build_rfc822, ImapAuth, OutgoingMessage, SentIds};
use anyhow::{Context, Result};
use lettre::message::Message as LettreMessage;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use super::connect::{with_session, ImapSession};

pub async fn send_mail(auth: &ImapAuth<'_>, msg: &OutgoingMessage<'_>) -> Result<SentIds> {
    send(auth, None, msg).await
}

pub async fn send_reply(
    auth: &ImapAuth<'_>,
    reply_to_id: &str,
    msg: &OutgoingMessage<'_>,
) -> Result<SentIds> {
    send(auth, Some(reply_to_id), msg).await
}

/// A fresh `Message-ID`, without its angle brackets, on the sender's domain.
/// Generated here rather than left to the submission server: not every MSA
/// adds one, and we need to know it to find the message afterwards.
fn new_message_id(from: &str) -> String {
    let mut random = [0u8; 16];
    if getrandom::fill(&mut random).is_err() {
        // Uniqueness is all that is asked of it.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        random = nanos.to_le_bytes();
    }
    let domain = from
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim_end_matches('>').trim())
        .filter(|domain| {
            !domain.is_empty()
                && domain
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        })
        .unwrap_or("aviary.invalid");
    format!("{}@{domain}", hex::encode(random))
}

async fn send(
    auth: &ImapAuth<'_>,
    reply_to_id: Option<&str>,
    msg: &OutgoingMessage<'_>,
) -> Result<SentIds> {
    let message_id = new_message_id(msg.from);
    let mut headers: Vec<(&str, String)> = vec![("Message-ID", format!("<{message_id}>"))];
    if let Some(reply_to_id) = reply_to_id {
        let id = reply_to_id.to_string();
        // Best-effort: a reply that cannot read its original still goes out,
        // unthreaded, rather than not at all.
        match with_session(auth, move |session| {
            super::messages::fetch_reply_headers(session, &id)
        })
        .await
        {
            Ok(thread) => headers.extend(thread),
            Err(e) => log::warn!("reply sent without threading headers: {e:#}"),
        }
    }
    let wire = build_rfc822(msg, &headers, false)?;
    deliver(auth, wire).await?;

    // The message is delivered. **Nothing below may fail this call**: an
    // error here would be read as a failed send and replayed by the outbox —
    // a second delivery to make up for a missing copy.
    match build_rfc822(msg, &headers, true) {
        // `keep_bcc`: the Sent copy is the sender's record of who got it.
        Ok(copy) => {
            let raw = copy.formatted();
            let id = message_id.clone();
            if let Err(e) =
                with_session(auth, move |session| file_sent_copy(session, &raw, &id)).await
            {
                log::warn!("sent message was not copied to the Sent mailbox: {e:#}");
            }
        }
        Err(e) => log::warn!("building the Sent copy failed: {e:#}"),
    }
    Ok(SentIds {
        message_id: None,
        internet_message_id: Some(message_id),
    })
}

/// Files the sent message in the Sent mailbox unless the server already did.
///
/// Gmail, Outlook.com and others copy what goes through their SMTP into Sent
/// by themselves; appending blindly would leave two. The `Message-ID` we just
/// generated says whether a copy is there.
fn file_sent_copy(session: &mut ImapSession, raw: &[u8], message_id: &str) -> Result<()> {
    let folders = super::messages::mailboxes_for_alias(session, "sentitems")?;
    let Some(sent) = folders.first().cloned() else {
        // Never invented: a mailbox the server does not designate as Sent
        // would be one more folder nobody asked for.
        log::info!("no Sent mailbox on this server; the sent message is not copied");
        return Ok(());
    };
    for folder in &folders {
        if super::messages::find_uid_by_message_id(session, folder, message_id)?.is_some() {
            return Ok(());
        }
    }
    session
        .append(&sent, raw)
        .flag(imap::types::Flag::Seen)
        .finish()
        .with_context(
            || tr!("technical-operation-failed", { operation: format!("APPEND {sent}") }),
        )?;
    Ok(())
}

/// Save the message as a draft via IMAP `APPEND`. Picks the user's Drafts
/// mailbox (RFC 6154 `\Drafts` attribute, then a name-based fallback) and
/// uploads the formatted RFC822 with `\Draft \Seen` flags. When `replace_id`
/// is `Some` (encoded as `<folder>:<uid>` like every IMAP message id), the
/// old draft is `\Deleted` + `UID EXPUNGE`'d after the new one lands so the user
/// doesn't accumulate duplicates while iterating.
///
/// The new draft's id is read back by searching Drafts for the `Message-ID`
/// stamped on it: APPENDUID (RFC 4315) would say it directly, but the imap
/// crate does not expose it. Returning `None` — as this used to, always —
/// meant the composer never learned its draft's id, so every autosave
/// appended one more copy and the send had nothing to delete: a Drafts folder
/// filling up with every version of every message, Bcc included.
/// SMTP isn't involved here.
pub async fn save_draft(
    auth: &ImapAuth<'_>,
    msg: &OutgoingMessage<'_>,
    replace_id: Option<&str>,
) -> Result<Option<String>> {
    // An IMAP draft must retain its Bcc header so reopening it can restore
    // the blind recipients. SMTP sends use the default Lettre behavior,
    // which removes that header after deriving the private envelope.
    let message_id = new_message_id(msg.from);
    let mail = build_rfc822(msg, &[("Message-ID", format!("<{message_id}>"))], true)?;
    let raw = mail.formatted();
    let replace = replace_id.map(|s| s.to_string());
    with_session(auth, move |session| {
        let drafts = resolve_drafts_mailbox(session);
        session
            .append(&drafts, &raw)
            .flag(imap::types::Flag::Draft)
            .flag(imap::types::Flag::Seen)
            .finish()
            .with_context(
                || tr!("technical-operation-failed", { operation: format!("APPEND {drafts}") }),
            )?;

        // Best-effort: drop the previous draft so the user doesn't keep a
        // stale copy alongside the new one. Errors here mustn't mask the
        // successful append above.
        if let Some(old) = &replace {
            if let Some((folder, uid)) = parse_old_id(old) {
                if let Err(e) = drop_old_draft(session, &folder, uid) {
                    log::warn!("previous draft was not deleted: {e:#}");
                }
            }
        }

        // Best-effort as well: without the id the next save appends again,
        // which is the old behaviour, not a failure of this one.
        Ok(
            match super::messages::find_uid_by_message_id(session, &drafts, &message_id) {
                Ok(uid) => uid.map(|uid| super::messages::make_id(&drafts, uid)),
                Err(e) => {
                    log::warn!("new draft id could not be read back: {e:#}");
                    None
                }
            },
        )
    })
    .await
}

fn parse_old_id(id: &str) -> Option<(String, u32)> {
    let (f, u) = id.rsplit_once(':')?;
    let folder = urlencoding::decode(f).ok()?.to_string();
    let uid = u.parse::<u32>().ok()?;
    Some((folder, uid))
}

fn drop_old_draft(session: &mut ImapSession, folder: &str, uid: u32) -> Result<()> {
    session.select(folder).with_context(
        || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
    )?;
    remove_uid(session, uid)
}

/// Hard-removes one draft, scoped to its UID: a plain `EXPUNGE` would also
/// purge whatever other clients flagged `\Deleted` in the Drafts mailbox.
fn remove_uid(session: &mut ImapSession, uid: u32) -> Result<()> {
    let caps = super::messages::ServerCaps::query(session);
    super::messages::purge_uid(session, uid, caps)
}

/// Delete an IMAP draft by its message id (`<encoded folder>:<uid>`). Hard
/// remove (`\Deleted` + `UID EXPUNGE` when the server has UIDPLUS, the
/// flag alone otherwise) rather than moving to Trash — once a draft
/// has been sent we don't want a second copy lingering in Deleted Items.
pub async fn delete_draft(auth: &ImapAuth<'_>, draft_id: &str) -> Result<()> {
    let Some((folder, uid)) = parse_old_id(draft_id) else {
        return Err(anyhow::anyhow!("invalid draft identifier: {draft_id}"));
    };
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        remove_uid(session, uid)
    })
    .await
}

/// Find a Drafts mailbox by `\Drafts` attribute first, then by a name match.
/// We default to `"Drafts"` even when nothing turns up — the IMAP server may
/// auto-create it on APPEND, and if not the caller surfaces the error.
fn resolve_drafts_mailbox(session: &mut ImapSession) -> String {
    if let Ok(names) = session.list(Some(""), Some("*")) {
        for entry in names.iter() {
            let attrs: Vec<String> = entry
                .attributes()
                .iter()
                .map(|a| format!("{a:?}").to_ascii_lowercase())
                .collect();
            if attrs.iter().any(|a| a.contains("drafts")) {
                return entry.name().to_string();
            }
        }
        for entry in names.iter() {
            let lower = entry.name().to_ascii_lowercase();
            if lower == "drafts" || lower.ends_with("/drafts") {
                return entry.name().to_string();
            }
        }
    }
    "Drafts".to_string()
}

async fn deliver(auth: &ImapAuth<'_>, mail: LettreMessage) -> Result<()> {
    let creds = Credentials::new(auth.smtp_username.to_string(), auth.password.to_string());
    let transport: AsyncSmtpTransport<Tokio1Executor> = match auth.smtp_security {
        NetSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(auth.smtp_host)
            .with_context(|| {
                tr!("imap-error-smtp-relay", {
                    host: auth.smtp_host,
                    security: "TLS"
                })
            })?
            .port(auth.smtp_port)
            .credentials(creds)
            .build(),
        NetSecurity::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(auth.smtp_host)
                .with_context(|| {
                    tr!("imap-error-smtp-relay", {
                        host: auth.smtp_host,
                        security: "STARTTLS"
                    })
                })?
                .port(auth.smtp_port)
                .credentials(creds)
                .build()
        }
        NetSecurity::Plain => {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(auth.smtp_host)
                .port(auth.smtp_port)
                .tls(Tls::None)
                .credentials(creds)
                .build()
        }
    };

    transport
        .send(mail)
        .await
        .map(|_| ())
        .context(tr!("imap-error-smtp-send"))
}

#[cfg(test)]
mod tests {
    use super::new_message_id;
    use crate::providers::{build_rfc822, OutgoingMessage};

    #[test]
    fn message_ids_are_unique_and_on_the_senders_domain() {
        let first = new_message_id("Contact A <contact-a@example.test>");
        let second = new_message_id("contact-a@example.test");
        assert!(first.ends_with("@example.test"), "{first}");
        assert!(second.ends_with("@example.test"), "{second}");
        assert_ne!(first, second);
        // A `From` we cannot read a domain out of still gives a valid id.
        assert!(new_message_id("not an address").ends_with("@aviary.invalid"));
    }

    #[test]
    fn the_outgoing_message_carries_exactly_our_message_id() {
        let to = ["contact-b@example.test".to_string()];
        let message = OutgoingMessage {
            from: "contact-a@example.test",
            to: &to,
            cc: &[],
            bcc: &[],
            subject: "Sujet de test",
            body: "Corps de test",
            body_is_html: false,
            attachments: &[],
            files: &[],
        };
        let raw = String::from_utf8(
            build_rfc822(
                &message,
                &[("Message-ID", "<generated@example.test>".to_string())],
                false,
            )
            .unwrap()
            .formatted(),
        )
        .unwrap();
        assert_eq!(
            raw.to_ascii_lowercase().matches("message-id:").count(),
            1,
            "{raw}"
        );
        assert!(
            raw.contains("Message-ID: <generated@example.test>"),
            "{raw}"
        );
    }
}
