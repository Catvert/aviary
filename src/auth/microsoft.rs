use super::Tokens;
use crate::model::Provider;
use anyhow::{bail, Result};
use chrono::{Duration, Utc};
use serde::Deserialize;

const SCOPE: &str =
    "offline_access Mail.ReadWrite Mail.Send Calendars.ReadWrite People.Read User.Read";

/// Default Aviary Azure AD app registration (multi-tenant + MSA, public client flows enabled).
/// Users may override via Settings → Comptes → Configuration Azure when their tenant blocks
/// unverified third-party apps, or when self-hosting the registration.
pub const DEFAULT_CLIENT_ID: &str = "37b9d14c-2880-4360-b1d7-948f3eac48bf";
pub const DEFAULT_TENANT: &str = "common";

fn authority(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: i64,
    pub interval: i64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

pub async fn start_device_code(
    client: &reqwest::Client,
    client_id: &str,
    tenant: &str,
) -> Result<DeviceCodeStart> {
    let url = format!("{}/oauth2/v2.0/devicecode", authority(tenant));
    let resp = client
        .post(&url)
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("device code request failed ({status}): {body}");
    }
    Ok(resp.json().await?)
}

pub async fn poll_device_code(
    client: &reqwest::Client,
    client_id: &str,
    tenant: &str,
    device_code: &str,
) -> Result<Option<Tokens>> {
    let url = format!("{}/oauth2/v2.0/token", authority(tenant));
    let resp = match client
        .post(&url)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", client_id),
            ("device_code", device_code),
        ])
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(error) => {
            log::warn!("transient device-code polling error: {error:#}");
            return Ok(None);
        }
    };

    if resp.status().is_success() {
        let tr: TokenResponse = match resp.json().await {
            Ok(tokens) => tokens,
            Err(error) => {
                log::warn!("transient device-code token response error: {error:#}");
                return Ok(None);
            }
        };
        return Ok(Some(Tokens {
            provider: Provider::Microsoft,
            access_token: tr.access_token,
            refresh_token: tr.refresh_token,
            expires_at: Utc::now() + Duration::seconds(tr.expires_in - 60),
            client_id: client_id.to_string(),
            client_secret: String::new(),
            tenant: tenant.to_string(),
            imap_config: None,
        }));
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let err: ErrorResponse = match serde_json::from_str(&body) {
        Ok(error) => error,
        Err(error) => {
            log::warn!("transient device-code polling response ({status}): {error:#}");
            return Ok(None);
        }
    };
    match err.error.as_str() {
        "authorization_pending" | "slow_down" => Ok(None),
        // Explicit OAuth terminal states. Everything else (including a
        // proxy-generated JSON error) is treated as transient until the
        // device-code deadline expires.
        "expired_token" | "access_denied" | "authorization_declined" => bail!(
            "auth error: {} ({})",
            err.error,
            err.error_description.unwrap_or_default()
        ),
        _ => {
            log::warn!("transient device-code OAuth error: {}", err.error);
            Ok(None)
        }
    }
}

pub async fn refresh(
    client: &reqwest::Client,
    client_id: &str,
    tenant: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let url = format!("{}/oauth2/v2.0/token", authority(tenant));
    let resp = client
        .post(&url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("refresh failed ({status}): {body}");
    }
    let tr: TokenResponse = resp.json().await?;
    Ok(Tokens {
        provider: Provider::Microsoft,
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        expires_at: Utc::now() + Duration::seconds(tr.expires_in - 60),
        client_id: client_id.to_string(),
        client_secret: String::new(),
        tenant: tenant.to_string(),
        imap_config: None,
    })
}
