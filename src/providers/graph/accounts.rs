use super::{Client, BASE};
use crate::model::{Account, AccountId, Provider};
use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
struct GraphMe {
    id: String,
    #[serde(rename = "userPrincipalName")]
    user_principal_name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    mail: Option<String>,
}

pub async fn get_me(client: &Client<'_>, tenant: &str) -> Result<Account> {
    let url = format!("{BASE}/me");
    let resp = client
        .get(&url)
        .query(&[("$select", "id,userPrincipalName,displayName,mail")])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph /me failed").await);
    }
    let me: GraphMe = resp.json().await?;
    let upn = me
        .user_principal_name
        .clone()
        .unwrap_or_else(|| me.id.clone());
    let email = me.mail.unwrap_or_else(|| upn.clone());
    Ok(Account {
        id: AccountId(upn),
        email,
        display_name: me.display_name.unwrap_or_default(),
        tenant: tenant.to_string(),
        provider: Provider::Microsoft,
    })
}
