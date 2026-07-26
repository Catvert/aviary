//! IMAP connection helpers.
//!
//! The IMAP backend is synchronous (`imap` 3.x). Each provider call wraps
//! its work in `tokio::task::spawn_blocking` so we don't stall the
//! current-thread runtime. Authenticated sessions are retained per account
//! and serialized by a mutex: successive reads avoid the TCP + TLS + LOGIN
//! cost (about 500 ms), while automatically reopening
//! a session that has become invalid.

use crate::auth::NetSecurity;
use crate::model::AccountId;
use crate::providers::ImapAuth;
use anyhow::{Context, Result};
use imap::{ClientBuilder, Connection, ConnectionMode, Session, TlsKind};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub type ImapSession = Session<Connection>;
type SessionSlot = Arc<Mutex<Option<ImapSession>>>;

static SESSION_POOL: OnceLock<Mutex<HashMap<String, SessionSlot>>> = OnceLock::new();

fn session_key(auth: &OwnedAuth) -> String {
    let password_hash = hex::encode(Sha256::digest(auth.password.as_bytes()));
    format!(
        "{}:{}:{:?}:{}:{password_hash}",
        auth.imap_host, auth.imap_port, auth.imap_security, auth.imap_username
    )
}

fn session_slot(auth: &OwnedAuth) -> SessionSlot {
    let pool = SESSION_POOL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pool = pool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pool.entry(session_key(auth))
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Open an authenticated IMAP session. **Blocking** — must be called from
/// inside `spawn_blocking` (or any non-async context).
pub fn open_session(auth: &ImapAuth<'_>) -> Result<ImapSession> {
    let mode = match auth.imap_security {
        NetSecurity::Tls => ConnectionMode::Tls,
        NetSecurity::StartTls => ConnectionMode::StartTls,
        NetSecurity::Plain => ConnectionMode::Plaintext,
    };
    let client = ClientBuilder::new(auth.imap_host, auth.imap_port)
        .mode(mode)
        .tls_kind(TlsKind::Rust)
        .connect()
        .with_context(|| {
            tr!("imap-error-connect", {
                host: auth.imap_host,
                port: auth.imap_port
            })
        })?;
    let session = client
        .login(auth.imap_username, auth.password)
        .map_err(|(e, _client)| anyhow::anyhow!(tr!("imap-error-login", { error: e })))?;
    Ok(session)
}

/// Retry a failed connection once if the keyring password changed after the
/// runtime cached it. This keeps keyring I/O off the Tokio thread and makes a
/// server-side authentication failure invalidate the stale snapshot lazily.
fn open_session_with_keyring_refresh(auth: &mut OwnedAuth) -> Result<ImapSession> {
    let first_error = match open_session(&auth.as_view()) {
        Ok(session) => return Ok(session),
        Err(error) => error,
    };
    let Ok(password) = crate::auth::imap::load_password(&AccountId(auth.email.clone())) else {
        return Err(first_error);
    };
    if password == auth.password {
        return Err(first_error);
    }
    auth.password = password;
    open_session(&auth.as_view())
}

/// Run a blocking closure with the account's persistent session. Calls for a
/// given account are serialized; a failed `NOOP` drops the socket so the next
/// operation reconnects.
pub async fn with_session<F, R>(auth: &ImapAuth<'_>, f: F) -> Result<R>
where
    F: FnOnce(&mut ImapSession) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    // Capture everything `f` needs into owned values; `auth` is borrowed and
    // can't cross the blocking boundary directly.
    let owned = OwnedAuth::from(auth);
    let slot = session_slot(&owned);
    let join = tokio::task::spawn_blocking(move || -> Result<R> {
        let mut owned = owned;
        let mut guard = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut session = match guard.take() {
            Some(mut session) => {
                if session.noop().is_ok() {
                    session
                } else {
                    open_session_with_keyring_refresh(&mut owned)?
                }
            }
            None => open_session_with_keyring_refresh(&mut owned)?,
        };
        let result = f(&mut session);
        if session.noop().is_ok() {
            *guard = Some(session);
        }
        result
    });
    join.await.context(tr!("imap-error-task-panicked"))?
}

/// Removes and closes the persistent session matching these credentials.
pub async fn close_session(auth: &ImapAuth<'_>) -> Result<()> {
    let owned = OwnedAuth::from(auth);
    let key = session_key(&owned);
    let slot = SESSION_POOL.get().and_then(|pool| {
        pool.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key)
    });
    let Some(slot) = slot else { return Ok(()) };
    tokio::task::spawn_blocking(move || {
        let session = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut session) = session {
            let _ = session.logout();
        }
    })
    .await
    .context("closing IMAP session")?;
    Ok(())
}

/// Owned snapshot of an `ImapAuth<'_>` so we can move it across thread
/// boundaries (`spawn_blocking` requires `'static + Send`).
pub(super) struct OwnedAuth {
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
    pub password: String,
}

impl From<&ImapAuth<'_>> for OwnedAuth {
    fn from(a: &ImapAuth<'_>) -> Self {
        Self {
            email: a.email.to_string(),
            display_name: a.display_name.to_string(),
            imap_host: a.imap_host.to_string(),
            imap_port: a.imap_port,
            imap_security: a.imap_security,
            imap_username: a.imap_username.to_string(),
            smtp_host: a.smtp_host.to_string(),
            smtp_port: a.smtp_port,
            smtp_security: a.smtp_security,
            smtp_username: a.smtp_username.to_string(),
            password: a.password.to_string(),
        }
    }
}

impl OwnedAuth {
    pub fn as_view(&self) -> ImapAuth<'_> {
        ImapAuth {
            email: &self.email,
            display_name: &self.display_name,
            imap_host: &self.imap_host,
            imap_port: self.imap_port,
            imap_security: self.imap_security,
            imap_username: &self.imap_username,
            smtp_host: &self.smtp_host,
            smtp_port: self.smtp_port,
            smtp_security: self.smtp_security,
            smtp_username: &self.smtp_username,
            password: &self.password,
        }
    }
}
