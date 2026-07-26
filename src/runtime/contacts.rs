use super::{BgAccount, Evt};
use std::sync::Arc;

pub(super) async fn load_contacts(account: Arc<BgAccount>) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            log::warn!("contacts: {e:#}");
            return;
        }
    };
    match account.session(&auth).list_people(200).await {
        Ok(contacts) => account.emit(Evt::Contacts {
            account_id: account.id.clone(),
            contacts,
        }),
        Err(e) => log::warn!("contacts failed: {e:#}"),
    }
}
