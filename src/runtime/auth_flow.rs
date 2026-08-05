use super::{AuthOwned, BgAccount, BgGlobal, Evt};
use crate::auth::{self, ImapConfig, Tokens};
use crate::model::{AccountId, Provider};
use crate::providers::{ImapAuth, Session};
use chrono::{Duration, Utc};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, Semaphore};

/// Force-refresh `tokens` if the stored `access_token` is expired or close
/// to expiring. We do this proactively at resume time because cached access
/// tokens are short-lived (Microsoft: 1h, Google: 1h) and stale ones cause
/// `/me` to fail before we even get a chance to register the account.
async fn ensure_fresh_token(global: &BgGlobal, tokens: &mut Tokens) -> anyhow::Result<()> {
    if tokens.expires_at > Utc::now() + Duration::seconds(60) {
        return Ok(());
    }
    let rt = match tokens.refresh_token.clone() {
        Some(rt) => rt,
        None => return Ok(()), // no refresh possible — let /me fail with a clear error
    };
    let new = match tokens.provider {
        Provider::Microsoft => {
            auth::microsoft::refresh(&global.http, &tokens.client_id, &tokens.tenant, &rt).await?
        }
        Provider::Google => {
            let mut t =
                auth::google::refresh(&global.http, &tokens.client_id, &tokens.client_secret, &rt)
                    .await?;
            if t.refresh_token.is_none() {
                t.refresh_token = Some(rt);
            }
            t
        }
        // IMAP has no refresh; the credentials never expire from our point of
        // view (a server-side password change is reported as a LOGIN failure
        // at the next call).
        Provider::Imap => return Ok(()),
    };
    *tokens = new;
    Ok(())
}

pub(super) async fn login_flow(global: Arc<BgGlobal>, client_id: String, tenant: String) {
    let start = match auth::microsoft::start_device_code(&global.http, &client_id, &tenant).await {
        Ok(s) => s,
        Err(e) => {
            global.emit(Evt::Error(e.to_string()));
            return;
        }
    };
    global.emit(Evt::DeviceCode {
        user_code: start.user_code.clone(),
        verification_uri: start.verification_uri.clone(),
        message: start.message.clone(),
    });
    let _ = open::that(&start.verification_uri);

    let interval = std::time::Duration::from_secs(start.interval.max(1) as u64);
    let deadline = Utc::now() + Duration::seconds(start.expires_in);
    loop {
        tokio::time::sleep(interval).await;
        if Utc::now() >= deadline {
            global.emit(Evt::Error(
                tr!("auth-error-device-code-expired").to_string(),
            ));
            return;
        }
        match auth::microsoft::poll_device_code(
            &global.http,
            &client_id,
            &tenant,
            &start.device_code,
        )
        .await
        {
            Ok(Some(tokens)) => {
                // Persist as pending in case /me fails — next launch will retry resume.
                let _ = auth::save_pending_tokens(&tokens);
                global.emit(Evt::Authenticated);
                let g = global.clone();
                tokio::spawn(async move {
                    resume_session(g, tokens, None).await;
                });
                return;
            }
            Ok(None) => continue,
            Err(e) => {
                global.emit(Evt::Error(e.to_string()));
                return;
            }
        }
    }
}

pub(super) async fn google_login_flow(
    global: Arc<BgGlobal>,
    client_id: String,
    client_secret: String,
) {
    let session = match auth::google::start_authorize(&client_id).await {
        Ok(s) => s,
        Err(e) => {
            global.emit(Evt::Error(e.to_string()));
            return;
        }
    };
    global.emit(Evt::GoogleAuthOpening {
        auth_url: session.auth_url.clone(),
    });
    let _ = open::that(&session.auth_url);
    let tokens =
        match auth::google::await_redirect(&global.http, &client_id, &client_secret, session).await
        {
            Ok(t) => t,
            Err(e) => {
                global.emit(Evt::Error(e.to_string()));
                return;
            }
        };
    let _ = auth::save_pending_tokens(&tokens);
    global.emit(Evt::Authenticated);
    tokio::spawn(async move {
        resume_session(global, tokens, None).await;
    });
}

/// Bring up a `BgAccount` from a freshly issued or restored token set:
/// fetch `/me`, then register and announce the account. This is the single
/// entry point that
/// guarantees `accounts` map keys equal `account.id` before any other
/// task runs against the account.
pub(super) async fn resume_session(
    global: Arc<BgGlobal>,
    mut tokens: Tokens,
    stored_id: Option<AccountId>,
) {
    let provider = tokens.provider;
    let (client_id, client_secret, tenant) = match provider {
        Provider::Microsoft => (
            tokens.client_id.clone(),
            String::new(),
            tokens.tenant.clone(),
        ),
        Provider::Google => (
            tokens.client_id.clone(),
            tokens.client_secret.clone(),
            String::new(),
        ),
        // IMAP doesn't have OAuth client material; we keep these empty.
        Provider::Imap => (String::new(), String::new(), String::new()),
    };
    // Refresh stale access tokens before calling /me. Without this, restarting
    // after >1h fails with "InvalidAuthenticationToken" because the cached
    // access_token has expired.
    if let Err(e) = ensure_fresh_token(&global, &mut tokens).await {
        let error = tr!("auth-error-refresh", { error: format!("{e:#}") });
        emit_restore_error(&global, stored_id, provider, error.to_string());
        return;
    }

    // Build the auth context for the initial `/me` call. For IMAP this means
    // pulling the password from the keyring; for OAuth it's just the bearer.
    let imap_password = if matches!(provider, Provider::Imap) {
        let password_id = AccountId(
            tokens
                .imap_config
                .as_ref()
                .map(|c| c.email.clone())
                .unwrap_or_default(),
        );
        match tokio::task::spawn_blocking(move || auth::imap::load_password(&password_id)).await {
            Ok(Ok(p)) => Some(p),
            Ok(Err(e)) => {
                let error = tr!(
                    "auth-error-imap-password-missing",
                    { error: format!("{e:#}") }
                );
                emit_restore_error(&global, stored_id, provider, error.to_string());
                return;
            }
            Err(e) => {
                let error = tr!("auth-error-imap-password-missing", {
                    error: format!("{e:#}")
                });
                emit_restore_error(&global, stored_id, provider, error.to_string());
                return;
            }
        }
    } else {
        None
    };
    let access_token = tokens.access_token.clone();
    let session = match (provider, &tokens.imap_config, imap_password.as_deref()) {
        (Provider::Imap, Some(cfg), Some(pwd)) => Session::Imap(ImapAuth::from_config(cfg, pwd)),
        (Provider::Imap, _, _) => {
            emit_restore_error(
                &global,
                stored_id,
                provider,
                tr!("auth-error-imap-config-incomplete").to_string(),
            );
            return;
        }
        (Provider::Microsoft, _, _) => Session::Graph {
            client: crate::providers::graph::Client::without_gate(&global.http, &access_token),
            tenant: &tenant,
        },
        (Provider::Google, _, _) => Session::Gmail {
            client: &global.http,
            token: &access_token,
        },
    };
    let account = match session.get_me().await {
        Ok(a) => a,
        Err(e) => {
            let error = tr!("auth-error-me", { error: format!("{e:#}") });
            emit_restore_error(&global, stored_id, provider, error.to_string());
            return;
        }
    };
    let id = account.id.clone();
    if let Err(e) = auth::save_tokens(&id, &tokens) {
        log::warn!("save tokens for {id}: {e:#}");
    }
    let _ = auth::clear_pending_tokens();

    let already_registered = global.accounts.read().await.contains_key(&id);
    if already_registered {
        // Re-bind: refresh the info, replace tokens. Don't spawn duplicate loaders.
        if let Some(existing) = global.account(&id).await {
            *existing.info.write().await = account.clone();
            *existing.tokens.write().await = tokens;
            *existing.imap_password.write().await = imap_password;
            global.emit(Evt::AccountReady(account));
            tokio::spawn(super::operations::drain_account(existing));
        }
        return;
    }

    let bg = Arc::new(BgAccount {
        id: id.clone(),
        provider,
        tenant,
        client_id,
        client_secret,
        info: RwLock::new(account.clone()),
        tokens: RwLock::new(tokens),
        logged_out: AtomicBool::new(false),
        imap_password: RwLock::new(imap_password),
        seen_ids: Mutex::new(HashSet::new()),
        auto_refresh: Mutex::new(None),
        mail_sync: Mutex::new(()),
        operation_drain: Mutex::new(()),
        operation_retry: Mutex::new(None),
        global: global.clone(),
        // Stay below Graph's nominal four concurrent Outlook requests. This
        // gate is acquired by every actual HTTP attempt, including fan-outs
        // hidden inside one provider operation.
        graph_request_gate: Arc::new(crate::providers::graph::RequestGate::new(3)),
        throttle_retry: Mutex::new(None),
        // Coarsely bound concurrent provider operations as well as their
        // individual HTTP requests. Gmail and IMAP also benefit from avoiding
        // unbounded UI-triggered fan-outs.
        mailbox_gate: Arc::new(Semaphore::new(3)),
        deferred_read_gate: Arc::new(Semaphore::new(1)),
    });
    global.accounts.write().await.insert(id, bg.clone());
    global.emit(Evt::AccountReady(account));
    tokio::spawn(super::operations::drain_account(bg));
}

fn emit_restore_error(
    global: &BgGlobal,
    stored_id: Option<AccountId>,
    provider: Provider,
    error: String,
) {
    if let Some(account_id) = stored_id {
        global.emit(Evt::AccountRestoreFailed {
            account_id,
            provider,
            error,
        });
    } else {
        global.emit(Evt::Error(error));
    }
}

pub(super) async fn logout(global: Arc<BgGlobal>, account_id: AccountId) {
    let removed = {
        let mut accounts = global.accounts.write().await;
        if let Some(account) = accounts.get(&account_id) {
            account.logged_out.store(true, Ordering::Release);
        }
        accounts.remove(&account_id)
    };
    let was_imap = removed
        .as_ref()
        .is_some_and(|a| matches!(a.provider, Provider::Imap));
    if let Some(acc) = removed {
        if let Some(handle) = acc.auto_refresh.lock().await.take() {
            handle.abort();
        }
        let imap_auth = if matches!(acc.provider, Provider::Imap) {
            let config = acc.tokens.read().await.imap_config.clone();
            let password = acc.imap_password.read().await.clone();
            config
                .zip(password)
                .map(|(config, password)| AuthOwned::Imap { config, password })
        } else {
            None
        };
        if let Some(auth) = imap_auth {
            if let Err(e) = acc.session(&auth).disconnect().await {
                log::warn!("closing session for account {account_id}: {e:#}");
            }
        }
        // Wait for a refresh that passed its first atomic check. It either
        // observes `logged_out` before saving, or finishes saving before this
        // guard is acquired; clearing under the same lock therefore wins.
        let _tokens = acc.tokens.write().await;
        let _ = auth::clear_tokens(&account_id);
    } else {
        let _ = auth::clear_tokens(&account_id);
    }
    global.cache.purge_account(account_id.clone());
    global.operations.purge_account(account_id.clone());
    if was_imap {
        // Best-effort: a missing keyring entry isn't a logout failure.
        if let Err(e) = auth::imap::delete_password(&account_id) {
            log::warn!("delete IMAP keyring entry for {account_id}: {e:#}");
        }
    }
    global.emit(Evt::LoggedOut { account_id });
}

/// Drive the "Add IMAP account" flow. The UI hands us a fully-populated
/// `ImapConfig` plus a password; we (1) test the connection by issuing a
/// minimal `/me` against the IMAP server (which validates LOGIN) and only
/// then (2) persist the password to the keyring + the config to disk and
/// register the `BgAccount`. This ordering ensures we never persist
/// credentials that don't actually work.
pub(super) async fn imap_login_flow(global: Arc<BgGlobal>, config: ImapConfig, password: String) {
    let provisional_id = AccountId(config.email.clone());
    let session = Session::Imap(ImapAuth::from_config(&config, &password));
    let account = match session.get_me().await {
        Ok(a) => a,
        Err(e) => {
            global.emit(Evt::Error(
                tr!("auth-error-imap-login-failed", { error: format!("{e:#}") }).to_string(),
            ));
            return;
        }
    };
    let id = account.id.clone();
    if let Err(e) = auth::imap::save_password(&id, &password) {
        global.emit(Evt::Error(
            tr!("auth-error-keyring", { error: format!("{e:#}") }).to_string(),
        ));
        return;
    }
    // If the canonical id differs from the provisional one we used to test,
    // make sure we don't leave a stale keyring entry behind.
    if id != provisional_id {
        let _ = auth::imap::delete_password(&provisional_id);
    }
    let tokens = Tokens {
        provider: Provider::Imap,
        access_token: String::new(),
        refresh_token: None,
        // Far future — IMAP creds are valid until the server says otherwise.
        expires_at: Utc::now() + Duration::days(365 * 100),
        client_id: String::new(),
        client_secret: String::new(),
        tenant: String::new(),
        imap_config: Some(config),
    };
    if let Err(e) = auth::save_tokens(&id, &tokens) {
        global.emit(Evt::Error(
            tr!("auth-error-save-tokens", { error: format!("{e:#}") }).to_string(),
        ));
        return;
    }
    global.emit(Evt::Authenticated);
    tokio::spawn(async move {
        resume_session(global, tokens, None).await;
    });
}
