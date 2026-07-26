//! IMAP/SMTP credential storage.
//!
//! Server settings (host, port, TLS mode, username) live in the regular
//! per-account JSON file alongside `Tokens`. The password is stored in the
//! OS credential store via the `keyring` crate — Keychain on macOS,
//! Credential Manager on Windows, freedesktop Secret Service on Linux.
//! The keyring entry is keyed by a service name + the AccountId, so each
//! account has its own slot and we never persist a plain password to disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::AccountId;

const KEYRING_SERVICE: &str = "aviary-imap";

/// How the IMAP / SMTP socket is wrapped in TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NetSecurity {
    /// Cleartext (not recommended; only for local debug).
    Plain,
    /// Connect plain, then `STARTTLS` to upgrade. IMAP 143, SMTP 587.
    StartTls,
    /// Implicit TLS from the first byte. IMAP 993, SMTP 465.
    #[default]
    Tls,
}

/// Server settings for an IMAP+SMTP account. Persisted in the per-account
/// JSON next to `Tokens` (via `Tokens.imap_config`). The password is *not*
/// part of this struct — it lives in the keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImapConfig {
    pub email: String,
    pub display_name: String,

    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: NetSecurity,
    pub imap_username: String,

    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: NetSecurity,
    pub smtp_username: String,
}

/// Persist the password for `account_id` in the OS credential store.
/// Overwrites any previous secret in the same slot.
pub fn save_password(account_id: &AccountId, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account_id.0)
        .context(tr!("auth-error-keyring-open"))?;
    entry
        .set_password(password)
        .context(tr!("auth-error-keyring-write"))?;
    Ok(())
}

/// Read the password for `account_id`. Returns `Err` if the entry is
/// missing or the keyring isn't available.
pub fn load_password(account_id: &AccountId) -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &account_id.0)
        .context(tr!("auth-error-keyring-open"))?;
    entry
        .get_password()
        .context("reading password from keyring")
}

/// Delete the password slot. Idempotent: a missing entry is not an error.
pub fn delete_password(account_id: &AccountId) -> Result<()> {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, &account_id.0) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("deleting password from keyring"),
    }
}
