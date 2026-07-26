use super::{BgAccount, Evt, OutgoingMail};
use crate::model::{BodyFormat, LastAction, Message, MessageHeader, SentMessage};
use crate::providers::html::{collapse_blank_lines, convert_email_html};
use std::sync::Arc;

pub(super) async fn perform_send(
    account: Arc<BgAccount>,
    compose_id: u64,
    reply_to: Option<String>,
    reply_all: bool,
    forward_of: Option<String>,
    draft_id: Option<String>,
    mail: OutgoingMail,
) -> anyhow::Result<()> {
    let auth = account.ensure_auth().await?;
    let permit = account.mailbox_permit().await;
    let info = account.info.read().await;
    let from = info.email.clone();
    let display_from = if info.display_name.trim().is_empty() {
        from.clone()
    } else {
        format!("{} <{}>", info.display_name, info.email)
    };
    drop(info);
    let outgoing = mail.as_outgoing(&from);
    let sent_ids = match &reply_to {
        Some(reply_id) => account.session(&auth).send_reply(reply_id, &outgoing).await,
        // Only reply/forward snapshots are kept, so ids are only worth
        // paying for (Graph draft flow) when this send is a forward.
        None => {
            account
                .session(&auth)
                .send_mail(&outgoing, forward_of.is_some())
                .await
        }
    }?;
    drop(permit);
    let recipients = mail
        .to
        .iter()
        .chain(&mail.cc)
        .chain(&mail.bcc)
        .cloned()
        .collect();
    account.global.record_recipient_usage(recipients).await;
    // Successful send — clean up the originating draft so it
    // doesn't linger. A failure here is logged but doesn't change
    // the user-visible outcome (the message was sent).
    if let Some(did) = &draft_id {
        if let Err(e) = account.session(&auth).delete_draft(did).await {
            log::warn!("draft cleanup after send failed: {e:#}");
        }
    }
    // Mark the original as replied to or forwarded, Outlook-style.
    // Best effort: a failure here does not change the send outcome.
    let noted = match (&reply_to, &forward_of) {
        (Some(id), _) => Some((
            id.clone(),
            if reply_all {
                LastAction::RepliedAll
            } else {
                LastAction::Replied
            },
        )),
        (None, Some(id)) => Some((id.clone(), LastAction::Forwarded)),
        (None, None) => None,
    };
    let sent_at = chrono::Utc::now();
    if let Some((id, action)) = &noted {
        match account.session(&auth).note_last_action(id, *action).await {
            Ok(true) => account.emit(Evt::MessageActionNoted {
                account_id: account.id.clone(),
                id: id.clone(),
                action: *action,
                at: sent_at,
            }),
            Ok(false) => {}
            Err(e) => log::warn!("recording reply/forward action failed: {e:#}"),
        }
    }
    let sent_message = if let Some((related_to, action)) = noted {
        let account_id = account.id.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            sent_message_snapshot(&account_id, compose_id, &display_from, sent_at, mail)
        })
        .await;
        match snapshot {
            Ok(message) => Some(SentMessage {
                related_to,
                action,
                message: Box::new(message),
                sent_id: sent_ids.message_id,
                internet_message_id: sent_ids.internet_message_id,
            }),
            Err(error) => {
                log::error!("sent snapshot conversion task failed: {error}");
                None
            }
        }
    } else {
        None
    };
    if let Some(sent) = &sent_message {
        account
            .global
            .cache
            .store_message(account.id.clone(), (*sent.message).clone());
    }
    account.emit(Evt::MailSent {
        account_id: account.id.clone(),
        compose_id,
        sent_message,
    });
    Ok(())
}

fn sent_message_snapshot(
    account_id: &crate::model::AccountId,
    compose_id: u64,
    from: &str,
    sent_at: chrono::DateTime<chrono::Utc>,
    mail: OutgoingMail,
) -> Message {
    let markdown = mail
        .attachments
        .iter()
        .fold(convert_email_html(&mail.body), |body, image| {
            body.replace(
                &format!("cid:{}", image.cid),
                &format!("bytes://cid-{}", image.cid),
            )
        });
    let markdown = collapse_blank_lines(&markdown);
    let preview = markdown
        .split_whitespace()
        .take(40)
        .collect::<Vec<_>>()
        .join(" ");
    let has_attachments = !mail.files.is_empty();
    Message {
        header: MessageHeader {
            id: format!("aviary-sent-{}-{compose_id}", sent_at.timestamp_micros()),
            account_id: account_id.clone(),
            subject: mail.subject,
            from: from.to_string(),
            received: sent_at,
            preview,
            is_read: true,
            is_flagged: false,
            has_attachments,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            // Local snapshot of an outgoing mail: the provider decides which
            // thread its Sent copy belongs to, and `SentCopyResolved` later
            // swaps in that copy with its real conversation id. The RFC 5322
            // id of the outgoing mail lives on `SentMessage`, which is what
            // resolves that copy.
            conversation_id: None,
            internet_message_id: None,
        },
        body: markdown,
        format: BodyFormat::Markdown,
        inline_images: mail.attachments,
        attachments: mail.files,
        tags: Vec::new(),
        raw_body: Some(mail.body),
        to: mail.to,
        cc: mail.cc,
        bcc: mail.bcc,
        draft_id: None,
        invitation: None,
    }
}

/// Resolve the provider's Sent-items copy behind a local reply/forward
/// snapshot. Best-effort background enrichment: failures are logged, never
/// surfaced — the snapshot simply stays in place and the lookup will be
/// retried the next time the card is expanded. Retries cover Graph's
/// documented delay between the send and the copy landing in Sent Items.
pub(super) async fn fetch_sent_copy(
    account: Arc<BgAccount>,
    related_to: String,
    snapshot_id: String,
    sent_id: Option<String>,
    internet_message_id: Option<String>,
) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            log::warn!("sent copy lookup: auth failed: {e:#}");
            return;
        }
    };
    let mut last_err = None;
    for attempt in 0..5u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        let found = {
            let _permit = account.mailbox_permit().await;
            resolve_sent_copy(
                &account,
                &auth,
                sent_id.as_deref(),
                internet_message_id.as_deref(),
            )
            .await
        };
        match found {
            Ok(Some(mut message)) => {
                message.header.account_id = account.id.clone();
                account
                    .global
                    .cache
                    .store_message(account.id.clone(), message.clone());
                account.emit(Evt::SentCopyResolved {
                    account_id: account.id.clone(),
                    related_to,
                    snapshot_id,
                    message: Box::new(message),
                });
                return;
            }
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => log::warn!("sent copy lookup for {related_to} failed: {e:#}"),
        None => log::info!("sent copy for {related_to} not available yet"),
    }
}

async fn resolve_sent_copy(
    account: &BgAccount,
    auth: &super::AuthOwned,
    sent_id: Option<&str>,
    internet_message_id: Option<&str>,
) -> anyhow::Result<Option<Message>> {
    if let Some(id) = sent_id {
        return account.session(auth).get_message(id).await.map(Some);
    }
    if let Some(imid) = internet_message_id {
        if let Some(id) = account.session(auth).find_sent_message(imid).await? {
            return account.session(auth).get_message(&id).await.map(Some);
        }
    }
    Ok(None)
}

pub(super) async fn save_draft(
    account: Arc<BgAccount>,
    compose_id: u64,
    replace_id: Option<String>,
    mail: OutgoingMail,
    autosave: bool,
) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            account.emit(Evt::DraftSaveError {
                account_id: account.id.clone(),
                compose_id,
                error: e.to_string(),
                autosave,
            });
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    let from = account.info.read().await.email.clone();
    let outgoing = mail.as_outgoing(&from);
    let result = account
        .session(&auth)
        .save_draft(&outgoing, replace_id.as_deref())
        .await;
    match result {
        Ok(draft_id) => account.emit(Evt::DraftSaved {
            account_id: account.id.clone(),
            compose_id,
            draft_id,
            autosave,
        }),
        Err(e) => account.emit(Evt::DraftSaveError {
            account_id: account.id.clone(),
            compose_id,
            error: e.to_string(),
            autosave,
        }),
    }
}
