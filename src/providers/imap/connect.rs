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
use imap::extensions::idle::SetReadTimeout as _;
use imap::{ClientBuilder, Connection, ConnectionMode, Session, TlsKind};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub type ImapSession = Session<Connection>;
/// A pooled session and when it last served a call.
type SessionSlot = Arc<Mutex<Option<(ImapSession, Instant)>>>;

/// Longest silence tolerated from the server inside one command. Without it a
/// socket that died quietly — the machine slept, the network changed — parks
/// the blocking thread until TCP gives up, a quarter of an hour during which
/// it holds the account's session, a mailbox permit and, for a mutation, the
/// outbox drain: listing, opening and sending all freeze with no error. A
/// read timeout applies to each read, not to the whole command, so a large
/// FETCH that keeps flowing is unaffected.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// A pooled session idle for longer than this is not probed but replaced. A
/// `NOOP` on a dead socket costs the whole [`READ_TIMEOUT`] before the
/// reconnection that was needed anyway; reconnecting outright costs half a
/// second. Servers drop idle connections after 30 minutes at the latest
/// (RFC 3501 §5.4), and a laptop that slept has usually lost them sooner.
const MAX_POOLED_IDLE: Duration = Duration::from_secs(5 * 60);

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
    // The builder hands back a connected client with no way to reach its
    // socket, but the connection itself can be told its read timeout: take it
    // out, set it, and wrap it again. The greeting has been read by then and
    // nothing is buffered, so the round trip loses no byte.
    let mut connection = client.into_inner().with_context(|| {
        tr!("imap-error-connect", {
            host: auth.imap_host,
            port: auth.imap_port
        })
    })?;
    if let Err(error) = connection.set_read_timeout(Some(READ_TIMEOUT)) {
        log::warn!("IMAP read timeout could not be set: {error}");
    }
    let mut client = imap::Client::new(connection);
    client.greeting_read = true;
    let session = client
        .login(auth.imap_username, auth.password)
        .map_err(|(e, _client)| login_error(e))?;
    Ok(session)
}

/// A refused or interrupted LOGIN, with the `imap::Error` kept as the cause
/// rather than flattened into the message: `runtime::operations` walks the
/// chain to tell a dropped connection (retry) from a rejected password (don't).
/// The cause is rendered by `{:#}`, which is how the runtime reports it, so
/// the catalog entry no longer embeds it.
fn login_error(error: imap::Error) -> anyhow::Error {
    anyhow::Error::new(error).context(tr!("imap-error-login"))
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
        let pooled = guard.take().and_then(|(mut session, last_used)| {
            (last_used.elapsed() <= MAX_POOLED_IDLE && session.noop().is_ok()).then_some(session)
        });
        let mut session = match pooled {
            Some(session) => session,
            None => open_session_with_keyring_refresh(&mut owned)?,
        };
        let result = f(&mut session);
        // A call that succeeded has just proved the session works; only a
        // failure needs the probe to tell a refused command from a dead link.
        if result.is_ok() || session.noop().is_ok() {
            *guard = Some((session, Instant::now()));
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
        if let Some((mut session, _)) = session {
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

#[cfg(test)]
mod tests {
    use super::login_error;

    /// The transport failure must stay reachable in the chain — that is what
    /// the outbox retry policy downcasts — and appear once in the report.
    #[test]
    fn login_failure_keeps_its_cause_in_the_chain() {
        let error = login_error(imap::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        )));
        assert!(error.chain().any(|cause| matches!(
            cause.downcast_ref::<imap::Error>(),
            Some(imap::Error::Io(io)) if io.kind() == std::io::ErrorKind::ConnectionReset
        )));
        let rendered = format!("{error:#}");
        assert_eq!(
            rendered.matches("connection reset by peer").count(),
            1,
            "{rendered}"
        );
    }
}
