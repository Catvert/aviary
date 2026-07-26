use super::{BgAccount, Evt};
use std::sync::Arc;

pub(super) async fn load_sender_history(account: Arc<BgAccount>, email: String, limit: usize) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            account.emit(Evt::SenderHistoryError {
                account_id: account.id.clone(),
                email,
                loading_more: false,
                error: tr!("runtime-error-sender-history", { error: format!("{e:#}") }).to_string(),
            });
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).list_from_sender(&email, limit).await {
        Ok((mut messages, next_link)) => {
            for m in &mut messages {
                m.account_id = account.id.clone();
            }
            account.emit(Evt::SenderHistory {
                account_id: account.id.clone(),
                email,
                messages,
                next_link,
            });
        }
        Err(e) => account.emit(Evt::SenderHistoryError {
            account_id: account.id.clone(),
            email,
            loading_more: false,
            error: tr!("runtime-error-sender-history", { error: format!("{e:#}") }).to_string(),
        }),
    }
}

pub(super) async fn load_more_sender_history(
    account: Arc<BgAccount>,
    email: String,
    next_link: String,
) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            account.emit(Evt::SenderHistoryError {
                account_id: account.id.clone(),
                email,
                loading_more: true,
                error: tr!("runtime-error-sender-history", { error: format!("{e:#}") }).to_string(),
            });
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).fetch_messages_page(&next_link).await {
        Ok((mut messages, next_link)) => {
            for m in &mut messages {
                m.account_id = account.id.clone();
            }
            account.emit(Evt::SenderHistoryMore {
                account_id: account.id.clone(),
                email,
                messages,
                next_link,
            });
        }
        Err(e) => account.emit(Evt::SenderHistoryError {
            account_id: account.id.clone(),
            email,
            loading_more: true,
            error: tr!("runtime-error-sender-history", { error: format!("{e:#}") }).to_string(),
        }),
    }
}
