use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::{AccountId, Provider};

pub mod google;
pub mod imap;
pub mod microsoft;

pub use google::{DEFAULT_GOOGLE_CLIENT_ID, DEFAULT_GOOGLE_CLIENT_SECRET};
pub use imap::{ImapConfig, NetSecurity};
pub use microsoft::{DEFAULT_CLIENT_ID, DEFAULT_TENANT};

/// Tokens persisted on disk per account. Provider-specific strings are empty
/// when they do not apply; `imap_config` is only populated for IMAP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub provider: Provider,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
    /// OAuth client_id used when these tokens were issued. Persisted alongside
    /// the tokens so refresh continues to work even if the user later changes
    /// the override in settings. Empty for IMAP.
    pub client_id: String,
    /// Google-only: the OAuth client_secret. Required for Google's "Desktop app"
    /// token endpoint even with PKCE; Google itself treats it as non-secret
    /// since it's bundled with installed apps. Empty for other providers.
    pub client_secret: String,
    /// Microsoft-only: the tenant authority the tokens were issued against.
    pub tenant: String,
    /// IMAP-only: server settings for the account. Password lives in the
    /// keyring, not here. None for OAuth providers.
    pub imap_config: Option<ImapConfig>,
}

fn config_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("be", "acetics", "aviary")
        .context("could not determine project directory")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn accounts_dir() -> Result<PathBuf> {
    let dir = config_dir()?.join("accounts");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn pending_token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("pending_tokens.json"))
}

fn account_token_path(id: &AccountId) -> Result<PathBuf> {
    Ok(accounts_dir()?.join(format!("{}.json", sanitize_id(&id.0))))
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Tokens that haven't been bound to an account yet because `/me` has not
/// completed. Persisting them lets a fresh authentication survive a crash.
pub fn save_pending_tokens(tokens: &Tokens) -> Result<()> {
    let path = pending_token_path()?;
    let s = serde_json::to_string_pretty(tokens)?;
    write_private_atomic(&path, s.as_bytes())
}

pub fn load_pending_tokens() -> Option<Tokens> {
    let path = pending_token_path().ok()?;
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn clear_pending_tokens() -> Result<()> {
    let path = pending_token_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn save_tokens(id: &AccountId, tokens: &Tokens) -> Result<()> {
    let path = account_token_path(id)?;
    let s = serde_json::to_string_pretty(tokens)?;
    write_private_atomic(&path, s.as_bytes())
}

pub fn clear_tokens(id: &AccountId) -> Result<()> {
    let path = account_token_path(id)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Synchronously deletes every per-account token file plus an unfinished
/// authentication. Used by the settings "danger zone" before restarting.
pub fn clear_all_tokens() -> Result<()> {
    if let Ok(dir) = accounts_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    clear_pending_tokens()?;
    Ok(())
}

/// Discover all accounts that already have a token file on disk.
/// Returns the parsed `(AccountId, Tokens)` pairs sorted by id.
pub fn list_persisted_accounts() -> Vec<(AccountId, Tokens)> {
    let Ok(dir) = accounts_dir() else {
        return Vec::new();
    };
    let mut out: Vec<(AccountId, Tokens)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(tokens) = serde_json::from_str::<Tokens>(&s) {
            out.push((AccountId(stem.to_string()), tokens));
        }
    }
    out.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    out
}

/// Writes credentials to `path` without ever exposing them.
///
/// The bytes go to a sibling temporary file created directly in `0600` — never
/// through the umask default, which would leave a refresh token world-readable
/// for the instant between `write` and a follow-up `chmod`. The final `rename`
/// then swaps the file in atomically, so a crash mid-write cannot leave a
/// truncated token file behind (the previous one stays intact).
fn write_private_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let directory = path
        .parent()
        .context("credential path has no parent directory")?;
    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials"),
        temporary_suffix()
    ));

    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let write = (|| -> std::io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("writing {}", temporary.display()));
    }

    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

/// Distinguishes the temporary files of two Aviary instances writing the same
/// account concurrently. Falls back to the pid when the OS entropy source is
/// unavailable — `create_new` still rejects a genuine collision.
fn temporary_suffix() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return std::process::id().to_string();
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aviary-auth-test-{name}-{}", temporary_suffix()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// A refresh token must never exist on disk under the umask default, not
    /// even for the instant between `write` and a follow-up `chmod`.
    #[test]
    #[cfg(unix)]
    fn credentials_are_never_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch_dir("perms");
        let path = dir.join("account.json");
        write_private_atomic(&path, b"{\"refresh_token\":\"secret\"}").expect("write");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "unexpected mode {:o}", mode & 0o777);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewriting_replaces_the_previous_content_and_leaves_no_temporary() {
        let dir = scratch_dir("replace");
        let path = dir.join("account.json");
        write_private_atomic(&path, b"first").expect("first write");
        write_private_atomic(&path, b"second").expect("second write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("listing")
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name != "account.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
