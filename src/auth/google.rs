use super::Tokens;
use crate::model::Provider;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Aviary's Google Cloud OAuth client (Desktop app). PKCE is used on top, but
/// Google's "Desktop app" client type still requires the secret in the code
/// exchange — Google itself treats it as "non-secret" since it ships embedded
/// in installed apps. Users may override either value via Settings → Comptes →
/// Google settings when self-hosting their own OAuth client.
///
/// The secret is **injected at build time** rather than written here. RFC 8252
/// classes a native app as a *public* client — one that cannot hold a secret,
/// which is why PKCE carries the actual security — so embedding it buys no
/// confidentiality: anyone can read it out of the binary. What keeping it out
/// of the sources does buy is concrete: the repository is public, and a literal
/// `GOCSPX-…` there is picked up by secret scanners and liable to be revoked
/// under us, breaking every signed-in Gmail account at its next refresh. It
/// also makes the secret rotatable without a source change, and lets distro
/// packagers build without one at all.
///
/// Official builds set `AVIARY_GOOGLE_CLIENT_SECRET` (see the release
/// workflow). Without it the constant is empty and Google sign-in asks the user
/// for their own registration in Preferences → Accounts, which is the same path
/// a restrictive tenant already takes.
pub const DEFAULT_GOOGLE_CLIENT_ID: &str = match option_env!("AVIARY_GOOGLE_CLIENT_ID") {
    Some(id) => id,
    None => "822913526576-9sj65peocr3qgsuvrlubavnf9j987qmt.apps.googleusercontent.com",
};
pub const DEFAULT_GOOGLE_CLIENT_SECRET: &str = match option_env!("AVIARY_GOOGLE_CLIENT_SECRET") {
    Some(secret) => secret,
    None => "",
};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const SCOPE: &str = concat!(
    "https://www.googleapis.com/auth/gmail.modify ",
    "https://www.googleapis.com/auth/gmail.send ",
    "https://www.googleapis.com/auth/calendar.events ",
    "https://www.googleapis.com/auth/contacts.readonly ",
    "https://www.googleapis.com/auth/contacts.other.readonly ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile",
);

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug)]
pub struct AuthSession {
    pub auth_url: String,
    pub redirect_uri: String,
    code_verifier: String,
    state: String,
    listener: TcpListener,
}

/// Phase 1 of the Google OAuth installed-app flow. Binds an ephemeral local
/// TCP port, builds the authorize URL with PKCE + state, and returns the URL
/// the caller should open in the user's browser. Phase 2 (`await_redirect`)
/// blocks the local listener until the browser hits the redirect.
pub async fn start_authorize(client_id: &str) -> Result<AuthSession> {
    if client_id.trim().is_empty() {
        bail!(tr!("auth-error-google-client-id-missing"));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let code_verifier = random_url_safe(64);
    let code_challenge = pkce_challenge(&code_verifier);
    let state = random_url_safe(24);

    let auth_url = format!(
        "{AUTH_URL}?client_id={cid}&redirect_uri={redir}&response_type=code\
         &scope={scope}&code_challenge={chall}&code_challenge_method=S256\
         &state={state}&access_type=offline&prompt=consent",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(&redirect_uri),
        scope = urlencoding::encode(SCOPE),
        chall = code_challenge,
        state = state,
    );

    Ok(AuthSession {
        auth_url,
        redirect_uri,
        code_verifier,
        state,
        listener,
    })
}

/// How long the whole redirect phase may take. The user has to reach Google's
/// consent screen in a browser, so this is generous — but never unbounded: an
/// abandoned sign-in must not leak the task and its bound port forever.
const REDIRECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Per-connection read budget. Browsers routinely open speculative sockets that
/// never send a byte; one of them must not stall the whole flow.
const REQUEST_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Phase 2: wait for the browser to redirect, parse the code, and exchange
/// it for tokens. Cancels (with an error) if the deadline passes or a state
/// mismatch suggests CSRF.
pub async fn await_redirect(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    session: AuthSession,
) -> Result<Tokens> {
    let AuthSession {
        listener,
        code_verifier,
        state,
        redirect_uri,
        ..
    } = session;

    let code = tokio::time::timeout(REDIRECT_TIMEOUT, accept_callback(&listener, &state))
        .await
        .map_err(|_| anyhow!(tr!("auth-error-google-timeout")))??;

    exchange_code(
        client,
        client_id,
        client_secret,
        &code,
        &code_verifier,
        &redirect_uri,
    )
    .await
}

/// Serves the loopback listener until the actual OAuth redirect arrives.
///
/// Accepting a single connection is not enough: browsers preconnect, fetch
/// `/favicon.ico`, and probe the port, any of which would otherwise consume the
/// one accept and hang the sign-in. Anything that isn't the redirect gets a 404
/// and the loop keeps waiting.
async fn accept_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _peer) = listener.accept().await?;
        let Some(path) = read_request_target(&mut stream).await else {
            continue;
        };
        if !is_oauth_redirect(&path) {
            let _ = respond(&mut stream, "404 Not Found", NOT_FOUND_PAGE).await;
            continue;
        }
        let result = parse_callback(&path, expected_state);
        let page = if result.is_ok() {
            SUCCESS_PAGE
        } else {
            FAILURE_PAGE
        };
        let _ = respond(&mut stream, "200 OK", page).await;
        return result;
    }
}

/// Reads the request target from the start line, tolerating a request split
/// across packets. Returns `None` for a connection that stays silent, sends
/// something that isn't HTTP, or floods us.
async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    const MAX_START_LINE: usize = 8192;

    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(2).position(|pair| pair == b"\r\n") {
            let line = std::str::from_utf8(&buffer[..end]).ok()?;
            return line.split_whitespace().nth(1).map(str::to_string);
        }
        if buffer.len() > MAX_START_LINE {
            return None;
        }
    }
}

/// True when the target carries the parameters Google appends to the redirect,
/// which distinguishes it from the browser's incidental requests.
fn is_oauth_redirect(path: &str) -> bool {
    let Some(query) = path.split_once('?').map(|(_, query)| query) else {
        return false;
    };
    query.split('&').any(|pair| {
        matches!(
            pair.split_once('=').map_or(pair, |(key, _)| key),
            "code" | "error"
        )
    })
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

const SUCCESS_PAGE: &str =
    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Aviary</title>\
     <style>body{font-family:system-ui,-apple-system,sans-serif;max-width:520px;margin:6em auto;\
     padding:2em;text-align:center;background:#1f2329;color:#e5e7eb;border-radius:12px}\
     h1{color:#84d784;margin:0 0 .5em}p{color:#9ca3af}</style></head>\
     <body><h1>✓ Authentication successful</h1>\
     <p>You can close this tab and return to Aviary.</p></body></html>";

const FAILURE_PAGE: &str =
    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Aviary</title>\
     <style>body{font-family:system-ui,sans-serif;max-width:520px;margin:6em auto;\
     padding:2em;text-align:center;background:#1f2329;color:#e5e7eb;border-radius:12px}\
     h1{color:#f87171;margin:0 0 .5em}p{color:#9ca3af}</style></head>\
     <body><h1>Authentication failed</h1>\
     <p>Return to Aviary to start the sign-in again.</p></body></html>";

const NOT_FOUND_PAGE: &str =
    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Aviary</title></head>\
     <body></body></html>";

fn parse_callback(path: &str, expected_state: &str) -> Result<String> {
    let query = path.split_once('?').map(|x| x.1).unwrap_or("");
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut err: Option<String> = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = urlencoding::decode(v).unwrap_or_default().into_owned();
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => err = Some(v),
            _ => {}
        }
    }
    if let Some(e) = err {
        bail!(tr!("auth-error-google-denied", { error: e }));
    }
    let state = state.context(tr!("auth-error-google-state-missing"))?;
    if state != expected_state {
        bail!("invalid OAuth state (possible CSRF)");
    }
    code.ok_or_else(|| anyhow!(tr!("auth-error-google-code-missing")))
}

async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(tr!("auth-error-google-code-exchange", { status: status, body: body }));
    }
    let tr: TokenResponse = resp.json().await?;
    Ok(Tokens {
        provider: Provider::Google,
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        expires_at: Utc::now() + Duration::seconds(tr.expires_in - 60),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        tenant: String::new(),
        imap_config: None,
    })
}

pub async fn refresh(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(tr!("auth-error-google-refresh", { status: status, body: body }));
    }
    let tr: TokenResponse = resp.json().await?;
    Ok(Tokens {
        provider: Provider::Google,
        access_token: tr.access_token,
        // Google's refresh-token responses don't always re-issue the rt;
        // keep the existing one in that case (caller handles).
        refresh_token: tr.refresh_token,
        expires_at: Utc::now() + Duration::seconds(tr.expires_in - 60),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        tenant: String::new(),
        imap_config: None,
    })
}

fn random_url_safe(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::fill(&mut buf).expect("getrandom");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

fn pkce_challenge(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn send_request(port: u16, request: &str) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connecting to the loopback listener");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("writing the request");
        stream
    }

    #[test]
    fn only_the_redirect_carries_oauth_parameters() {
        assert!(is_oauth_redirect("/callback?code=abc&state=xyz"));
        assert!(is_oauth_redirect("/callback?error=access_denied"));
        assert!(!is_oauth_redirect("/favicon.ico"));
        assert!(!is_oauth_redirect("/callback"));
        assert!(!is_oauth_redirect("/callback?scope=mail"));
    }

    /// Browsers preconnect and fetch `/favicon.ico`; accepting a single
    /// connection used to let any of those swallow the sign-in.
    #[tokio::test]
    async fn incidental_requests_do_not_consume_the_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding the loopback listener");
        let port = listener.local_addr().expect("local address").port();
        let waiting = tokio::spawn(async move { accept_callback(&listener, "state-1").await });

        let _noise = send_request(port, "GET /favicon.ico HTTP/1.1\r\nHost: local\r\n\r\n").await;
        let _redirect = send_request(
            port,
            "GET /callback?code=granted&state=state-1 HTTP/1.1\r\nHost: local\r\n\r\n",
        )
        .await;

        let code = waiting
            .await
            .expect("listener task")
            .expect("redirect accepted");
        assert_eq!(code, "granted");
    }

    #[tokio::test]
    async fn a_mismatched_state_is_rejected() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding the loopback listener");
        let port = listener.local_addr().expect("local address").port();
        let waiting = tokio::spawn(async move { accept_callback(&listener, "state-1").await });

        let _forged = send_request(
            port,
            "GET /callback?code=granted&state=attacker HTTP/1.1\r\nHost: local\r\n\r\n",
        )
        .await;

        let error = waiting
            .await
            .expect("listener task")
            .expect_err("a forged state must not yield a code");
        assert!(error.to_string().contains("CSRF"), "{error}");
    }

    /// A request split across packets must still be understood.
    #[tokio::test]
    async fn a_fragmented_request_line_is_reassembled() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding the loopback listener");
        let port = listener.local_addr().expect("local address").port();
        let waiting = tokio::spawn(async move { accept_callback(&listener, "state-1").await });

        let mut stream = send_request(port, "GET /callback?code=gra").await;
        stream
            .write_all(b"nted&state=state-1 HTTP/1.1\r\nHost: local\r\n\r\n")
            .await
            .expect("writing the rest of the request");

        let code = waiting
            .await
            .expect("listener task")
            .expect("redirect accepted");
        assert_eq!(code, "granted");
    }
}
