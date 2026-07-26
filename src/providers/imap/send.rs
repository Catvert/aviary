//! SMTP send via lettre. Builds a `multipart/related` MIME message when
//! inline images are present (so the HTML body's `cid:` refs resolve at
//! the receiving MUA), or a single-part body otherwise.
//!
//! Replies don't get an `In-Reply-To` / `References` header in v1: our
//! internal `reply_to_id` is `<folder>:<uid>`, which doesn't give us the
//! upstream `Message-ID` without an extra IMAP round-trip. The reply is
//! still delivered correctly — only the recipient's threading view loses
//! the visual alignment. We can wire that in later by FETCHing the
//! original headers in the runtime layer before calling send.

use crate::auth::NetSecurity;
use crate::providers::{build_rfc822, ImapAuth, OutgoingMessage};
use anyhow::{Context, Result};
use lettre::message::Message as LettreMessage;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use super::connect::{with_session, ImapSession};

pub async fn send_mail(auth: &ImapAuth<'_>, msg: &OutgoingMessage<'_>) -> Result<()> {
    let mail = build_rfc822(msg, &[], false)?;
    deliver(auth, mail).await
}

pub async fn send_reply(
    auth: &ImapAuth<'_>,
    _reply_to_id: &str,
    msg: &OutgoingMessage<'_>,
) -> Result<()> {
    let mail = build_rfc822(msg, &[], false)?;
    deliver(auth, mail).await
}

/// Save the message as a draft via IMAP `APPEND`. Picks the user's Drafts
/// mailbox (RFC 6154 `\Drafts` attribute, then a name-based fallback) and
/// uploads the formatted RFC822 with `\Draft \Seen` flags. When `replace_id`
/// is `Some` (encoded as `<folder>:<uid>` like every IMAP message id), the
/// old draft is `\Deleted`+EXPUNGE'd after the new one lands so the user
/// doesn't accumulate duplicates while iterating.
///
/// Always returns `None` for the new id: APPENDUID (RFC 4315 UIDPLUS) is
/// the only reliable way to learn the new UID, and the imap crate routes
/// it through `imap_proto::UidSetMember` which isn't re-exported. The
/// follow-up save still writes to the *previous* `replace_id`, so each
/// edit keeps replacing the same draft via SEARCH on subsequent fetches —
/// at worst the user sees a brief duplicate until the next folder refresh.
/// SMTP isn't involved here.
pub async fn save_draft(
    auth: &ImapAuth<'_>,
    msg: &OutgoingMessage<'_>,
    replace_id: Option<&str>,
) -> Result<Option<String>> {
    // An IMAP draft must retain its Bcc header so reopening it can restore
    // the blind recipients. SMTP sends use the default Lettre behavior,
    // which removes that header after deriving the private envelope.
    let mail = build_rfc822(msg, &[], true)?;
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

        Ok(None)
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
    session
        .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
        .context(tr!("technical-operation-failed", { operation: "UID STORE \\Deleted" }))?;
    session
        .expunge()
        .context(tr!("technical-operation-failed", { operation: "EXPUNGE" }))?;
    Ok(())
}

/// Delete an IMAP draft by its message id (`<encoded folder>:<uid>`). Hard
/// remove (`\Deleted` + EXPUNGE) rather than moving to Trash — once a draft
/// has been sent we don't want a second copy lingering in Deleted Items.
pub async fn delete_draft(auth: &ImapAuth<'_>, draft_id: &str) -> Result<()> {
    let Some((folder, uid)) = parse_old_id(draft_id) else {
        return Err(anyhow::anyhow!("invalid draft identifier: {draft_id}"));
    };
    with_session(auth, move |session| {
        session.select(&folder).with_context(
            || tr!("technical-operation-failed", { operation: format!("SELECT {folder}") }),
        )?;
        session
            .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
            .context(tr!("technical-operation-failed", { operation: "UID STORE \\Deleted" }))?;
        session
            .expunge()
            .context(tr!("technical-operation-failed", { operation: "EXPUNGE" }))?;
        Ok(())
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
