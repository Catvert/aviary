use crate::model::{Account, AccountId, Provider};
use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
struct UserInfo {
    email: String,
    #[serde(default)]
    name: String,
}

pub async fn get_me(client: &reqwest::Client, access_token: &str) -> Result<Account> {
    let resp = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "gmail userinfo failed").await);
    }
    let info: UserInfo = resp.json().await?;
    Ok(Account {
        id: AccountId(info.email.clone()),
        email: info.email,
        display_name: info.name,
        tenant: "google".to_string(),
        provider: Provider::Google,
    })
}
