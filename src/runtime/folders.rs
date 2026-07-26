//! Runtime tasks related to the folder tree.

use super::{BgAccount, Evt};
use std::sync::Arc;

pub(super) async fn load(account: Arc<BgAccount>) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::Error(
                tr!("runtime-error-folders-auth", {
                    error: format!("{error:#}")
                })
                .to_string(),
            ));
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).list_folders().await {
        Ok(folders) => account.emit(Evt::Folders {
            account_id: account.id.clone(),
            folders,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-folders", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}

pub(super) async fn create(account: Arc<BgAccount>, name: String, parent_id: Option<String>) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account
        .session(&auth)
        .create_folder(&name, parent_id.as_deref())
        .await
    {
        Ok(folder) => account.emit(Evt::FolderCreated {
            account_id: account.id.clone(),
            folder,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-create-folder", {
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
    match account.session(&auth).rename_folder(&id, &new_name).await {
        Ok(new_id) => account.emit(Evt::FolderRenamed {
            account_id: account.id.clone(),
            id,
            new_id,
            new_name,
        }),
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-rename-folder", {
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
    match account.session(&auth).delete_folder(&id).await {
        Ok(()) => {
            account
                .global
                .cache
                .purge_folder(account.id.clone(), Some(id.clone()));
            account.emit(Evt::FolderDeleted {
                account_id: account.id.clone(),
                id,
            });
        }
        Err(error) => account.emit(Evt::Error(
            tr!("runtime-error-delete-folder", {
                error: format!("{error:#}")
            })
            .to_string(),
        )),
    }
}
