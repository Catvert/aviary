//! One running Aviary per user session, and a way to talk to it.
//!
//! Registering as the `mailto:` handler means the desktop answers every click
//! by running `aviary mailto:…` — a *new* process. Aviary keeps a session file,
//! a settings file, an SQLite mail cache and an SQLite outbox, none of which
//! expect a second writer; two instances would race on all four and the user
//! would watch tabs and drafts vanish as one overwrote the other's snapshot.
//!
//! So the first instance binds a Unix socket and becomes the primary. Later
//! ones connect, hand over what they were asked to do, and exit without ever
//! opening a window. The primary receives those requests on a channel and
//! serves them in the window that is already on screen — which is also what the
//! user wants from a mail client: the composer opens where their mailbox
//! already is.
//!
//! Unix-only, like the desktop entry that makes it necessary.

use crate::mailto::MailtoRequest;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Something a second invocation asked the running instance to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExternalRequest {
    /// Launched with no argument: just come to the front.
    Activate,
    /// Launched with a `mailto:` URL.
    Compose(MailtoRequest),
}

impl ExternalRequest {
    /// Reads the command line. Anything that is not a `mailto:` URL is ignored
    /// rather than refused: Aviary takes no options today, and a desktop file
    /// or a shell alias may well pass something we do not know about.
    pub fn from_args() -> Self {
        std::env::args()
            .skip(1)
            .find_map(|argument| crate::mailto::parse(&argument))
            .map(ExternalRequest::Compose)
            .unwrap_or(ExternalRequest::Activate)
    }
}

/// What `acquire` decided this process is.
#[cfg(unix)]
pub enum Acquisition {
    /// This process owns the session. It must keep `_listener` alive for as
    /// long as it runs, and drain `requests`.
    Primary {
        requests: mpsc::UnboundedReceiver<ExternalRequest>,
        _listener: SocketGuard,
    },
    /// Another instance is running and has been told what to do. Exit quietly.
    HandedOver,
}

/// Removes the socket file when the primary shuts down cleanly. A crash leaves
/// it behind, which is why connecting is what proves an instance is alive —
/// never the file's existence.
#[cfg(unix)]
pub struct SocketGuard(PathBuf);

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn socket_path() -> PathBuf {
    // XDG_RUNTIME_DIR is already per-user and cleared at logout, which is
    // exactly the lifetime this socket wants.
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("aviary.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    std::env::temp_dir().join(format!("aviary-{user}.sock"))
}

/// Claims the session, or hands `request` to whoever already holds it.
///
/// The caller must act on the return value before opening any window.
#[cfg(unix)]
pub fn acquire(request: ExternalRequest) -> Acquisition {
    acquire_at(socket_path(), request)
}

/// The socket path is a parameter so the handover can be exercised end to end
/// in a test directory instead of the session's real runtime dir.
#[cfg(unix)]
fn acquire_at(path: PathBuf, request: ExternalRequest) -> Acquisition {
    if send_to_running_instance(&path, &request) {
        log::info!("another Aviary instance is running; handed the request over");
        return Acquisition::HandedOver;
    }

    // Nobody answered. Either no instance is running, or one died without
    // cleaning up; removing the stale file is safe because a live instance
    // would have accepted the connection above.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            // Losing the bind race against a sibling process started at the
            // same moment is the one case worth retrying: that sibling is now
            // the primary and can take the request.
            if send_to_running_instance(&path, &request) {
                return Acquisition::HandedOver;
            }
            // Anything else — a read-only runtime dir, a path collision — must
            // not stop Aviary from starting. The single-instance guarantee is
            // lost, so `mailto:` may open a second process, but refusing to
            // launch would be worse.
            log::warn!("single-instance socket unavailable ({error:#}); continuing without it");
            let (tx, requests) = mpsc::unbounded_channel();
            let _ = tx.send(request);
            return Acquisition::Primary {
                requests,
                _listener: SocketGuard(PathBuf::new()),
            };
        }
    };

    let (tx, requests) = mpsc::unbounded_channel();
    // The request this process was itself launched with travels the same
    // channel as later ones, so the UI has a single code path to serve.
    let _ = tx.send(request);

    std::thread::Builder::new()
        .name("aviary-single-instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                match read_request(stream) {
                    Ok(request) => {
                        if tx.send(request).is_err() {
                            break; // The UI is gone; so is our reason to listen.
                        }
                    }
                    Err(error) => log::warn!("malformed single-instance request: {error:#}"),
                }
            }
        })
        .expect("failed to spawn the single-instance listener");

    Acquisition::Primary {
        requests,
        _listener: SocketGuard(path),
    }
}

/// Returns true when a live instance accepted the request.
#[cfg(unix)]
fn send_to_running_instance(path: &PathBuf, request: &ExternalRequest) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let Ok(payload) = serde_json::to_string(request) else {
        return false;
    };
    // A half-written line would be discarded by the reader anyway; treat any
    // write failure as "not delivered" so this process falls through to
    // starting normally rather than exiting having done nothing.
    stream.write_all(payload.as_bytes()).is_ok()
        && stream.write_all(b"\n").is_ok()
        && stream.flush().is_ok()
}

#[cfg(unix)]
fn read_request(stream: UnixStream) -> anyhow::Result<ExternalRequest> {
    // One JSON object per line, and a cap so a stray process cannot make the
    // primary allocate without bound.
    const MAX_REQUEST_BYTES: u64 = 64 * 1024;

    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES)).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mailto_argument_becomes_a_compose_request() {
        // `from_args` reads the real process arguments, so the mapping is
        // tested through the same parser it uses.
        let request = crate::mailto::parse("mailto:a@example.com?subject=Hi")
            .map(ExternalRequest::Compose)
            .expect("a mailto URL");
        let ExternalRequest::Compose(mailto) = request else {
            panic!("expected a compose request");
        };
        assert_eq!(mailto.to, "a@example.com");
        assert_eq!(mailto.subject, "Hi");
    }

    #[test]
    fn requests_survive_the_round_trip() {
        let original = ExternalRequest::Compose(MailtoRequest {
            to: "a@example.com".into(),
            subject: "Devis n°2".into(),
            body: "Bonjour,\nÀ demain".into(),
            ..Default::default()
        });
        let encoded = serde_json::to_string(&original).expect("serializable");
        assert!(
            !encoded.contains('\n'),
            "the wire format is one line per request"
        );

        let decoded: ExternalRequest = serde_json::from_str(&encoded).expect("deserializable");
        let ExternalRequest::Compose(mailto) = decoded else {
            panic!("expected a compose request");
        };
        assert_eq!(mailto.subject, "Devis n°2");
        assert_eq!(mailto.body, "Bonjour,\nÀ demain");
    }

    #[cfg(unix)]
    fn scratch_socket(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("aviary-test-{name}.sock"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The whole point of the module: a second launch must not become a second
    /// instance, and what it was asked to do must reach the first one.
    #[cfg(unix)]
    #[test]
    fn a_second_launch_hands_its_request_over_instead_of_starting() {
        let path = scratch_socket("handover");

        let Acquisition::Primary {
            mut requests,
            _listener,
        } = acquire_at(path.clone(), ExternalRequest::Activate)
        else {
            panic!("the first acquisition owns the session");
        };

        // The launch arguments of the primary itself arrive on the same channel.
        assert!(matches!(
            requests.blocking_recv(),
            Some(ExternalRequest::Activate)
        ));

        let second = acquire_at(
            path.clone(),
            ExternalRequest::Compose(MailtoRequest {
                to: "contact@example.com".into(),
                subject: "Depuis le bureau".into(),
                ..Default::default()
            }),
        );
        assert!(
            matches!(second, Acquisition::HandedOver),
            "a second launch must not claim the session"
        );

        let Some(ExternalRequest::Compose(mailto)) = requests.blocking_recv() else {
            panic!("the primary never received the handed-over request");
        };
        assert_eq!(mailto.to, "contact@example.com");
        assert_eq!(mailto.subject, "Depuis le bureau");
    }

    /// A crash leaves the socket file behind. Its mere presence must not
    /// convince the next launch that an instance is alive, or Aviary would
    /// refuse to start until someone deleted the file by hand.
    #[cfg(unix)]
    #[test]
    fn a_stale_socket_file_does_not_block_startup() {
        let path = scratch_socket("stale");
        std::fs::write(&path, b"not a socket").expect("writable temp dir");

        let acquisition = acquire_at(path.clone(), ExternalRequest::Activate);
        assert!(
            matches!(acquisition, Acquisition::Primary { .. }),
            "a leftover file must not stop the session from being claimed"
        );
    }
}
