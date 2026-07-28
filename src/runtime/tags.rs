//! Runtime tasks related to tags and kanban columns.

use super::retry::retry_read;
use super::{BgAccount, Evt};
use std::sync::Arc;

pub(super) async fn load(account: Arc<BgAccount>) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::Error(
                tr!("runtime-error-tags-auth", {
                    error: format!("{error:#}")
                })
                .to_string(),
            ));
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match retry_read(|| async { account.session(&auth).list_tags().await }).await {
        Ok(tags) => account.emit(Evt::Tags {
            account_id: account.id.clone(),
            tags,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-tags", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn create(account: Arc<BgAccount>, name: String, color: Option<u32>) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).create_tag(&name, color).await {
        Ok(tag) => account.emit(Evt::TagCreated {
            account_id: account.id.clone(),
            tag,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-create-tag", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn set_color(account: Arc<BgAccount>, id: String, color: u32) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).set_tag_color(&id, color).await {
        Ok(()) => account.emit(Evt::TagColorSet {
            account_id: account.id.clone(),
            id,
            color,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-tag-color", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn rename(account: Arc<BgAccount>, id: String, new_name: String) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).rename_tag(&id, &new_name).await {
        Ok(renamed) => {
            let old_message_tag = renamed
                .message_tag_rename
                .as_ref()
                .map(|(old, _)| old.clone());
            if let Some((old, new)) = renamed.message_tag_rename {
                if let Err(error) = account
                    .global
                    .cache
                    .rename_tag(account.id.clone(), old, new)
                    .await
                {
                    log::warn!("cache tag rename failed: {error:#}");
                    account.global.cache.purge_account(account.id.clone());
                }
            }
            account.emit(Evt::TagRenamed {
                account_id: account.id.clone(),
                id,
                new_id: renamed.new_id,
                old_message_tag,
                new_name,
            });
        }
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-rename-tag", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn delete(account: Arc<BgAccount>, id: String) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).delete_tag(&id).await {
        Ok(()) => account.emit(Evt::TagDeleted {
            account_id: account.id.clone(),
            id,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-delete-tag", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn add_to_message(account: Arc<BgAccount>, message_id: String, tag_id: String) {
    set_on_message(account, message_id, tag_id, true).await;
}

pub(super) async fn remove_from_message(
    account: Arc<BgAccount>,
    message_id: String,
    tag_id: String,
) {
    set_on_message(account, message_id, tag_id, false).await;
}

async fn set_on_message(account: Arc<BgAccount>, message_id: String, tag_id: String, added: bool) {
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(error) => {
            account.emit(Evt::TagApplyError {
                account_id: account.id.clone(),
                message_id,
                tag_id,
                added,
                error: tr!("runtime-error-update-tag", { error: format!("{error:#}") }).to_string(),
            });
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    let result = if added {
        account
            .session(&auth)
            .add_tag_to_message(&message_id, &tag_id)
            .await
    } else {
        account
            .session(&auth)
            .remove_tag_from_message(&message_id, &tag_id)
            .await
    };
    match result {
        Ok(()) => {
            account.global.cache.set_tag(
                account.id.clone(),
                message_id.clone(),
                tag_id.clone(),
                added,
            );
            account.emit(Evt::TagApplied {
                account_id: account.id.clone(),
                message_id,
                tag_id,
                added,
            });
        }
        Err(error) => account.emit(Evt::TagApplyError {
            account_id: account.id.clone(),
            message_id,
            tag_id,
            added,
            error: tr!("runtime-error-update-tag", { error: format!("{error:#}") }).to_string(),
        }),
    }
}

pub(super) async fn load_listing(account: Arc<BgAccount>, tag_id: String, limit: usize) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    let listed = retry_read(|| async {
        account
            .session(&auth)
            .list_messages_tagged(&tag_id, limit)
            .await
    })
    .await;
    match listed {
        Ok(mut messages) => {
            for message in &mut messages {
                message.account_id = account.id.clone();
            }
            account.emit(Evt::TagListing {
                account_id: account.id.clone(),
                tag_id,
                messages,
            });
        }
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-tag-listing", {
                tag: tag_id,
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}
