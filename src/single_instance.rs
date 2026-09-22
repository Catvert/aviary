//! One running Aviary per user session, and a way to talk to it.
//!
//! Registering as the `mailto:` handler means the desktop answers every click
//! by running `aviary mailto:…` — a *new* process. Aviary keeps a session file,
//! a settings file, an SQLite mail cache and an SQLite outbox, none of which
//! expect a second writer; two instances would race on all four and the user
//! would watch tabs and drafts vanish as one overwrote the other's snapshot.
//!
//! So the first instance starts a local server and becomes the primary. Later
//! ones connect, hand over what they were asked to do, and exit without ever
//! opening a window. The primary receives those requests on a channel and
//! serves them in the window that is already on screen — which is also what the
//! user wants from a mail client: the composer opens where their mailbox
//! already is.
//!
//! The transport differs per platform, the wire format does not: one JSON
//! object per line, so `read_request` and `write_request` are shared.
//!
//! - **Unix** — a socket in `XDG_RUNTIME_DIR`, which is per-user and cleared at
//!   logout, exactly the lifetime this wants. Without it (macOS), a `0700`
//!   directory of ours under the temp dir — never a bare, predictable path in
//!   `/tmp` that another user could claim first.
//! - **Windows** — a named pipe, through `interprocess`. `std` has no portable
//!   local socket, and the pipe is what the platform offers; it also disappears
//!   with the process, so there is no stale file to reason about.
//! - **Anywhere else** — no guarantee, and startup continues: this is a
//!   convenience, never a reason to refuse to launch.
//!
//! On macOS the desktop does *not* pass `mailto:` on the command line — the
//! Finder sends an Apple Event, which gpui surfaces through `on_open_urls`
//! (see `ui::run`). Both paths feed the same channel.

use crate::mailto::MailtoRequest;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use tokio::sync::mpsc;

#[cfg(unix)]
use std::path::PathBuf;

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
pub enum Acquisition {
    /// This process owns the session. It must keep `_listener` alive for as
    /// long as it runs, and drain `requests`.
    Primary {
        requests: mpsc::UnboundedReceiver<ExternalRequest>,
        /// The sending half, for request sources that live inside the process
        /// rather than in another one — macOS delivers `mailto:` as an Apple
        /// Event to the running app, not on the command line (see `ui::run`).
        sender: mpsc::UnboundedSender<ExternalRequest>,
        _listener: SessionGuard,
    },
    /// Another instance is running and has been told what to do. Exit quietly.
    HandedOver,
}

/// Cleans up whatever the platform leaves behind when the primary shuts down.
///
/// On Unix that is the socket file: a crash leaves it there, which is why
/// *connecting* is what proves an instance is alive — never the file's
/// existence. A named pipe needs no such care.
#[derive(Default)]
pub struct SessionGuard {
    #[cfg(unix)]
    socket: Option<PathBuf>,
}

#[cfg(unix)]
impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One JSON object per line, and a cap so a stray process cannot make the
/// primary allocate without bound.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Returns true when the request was handed over in full. A half-written line
/// would be discarded by the reader anyway, so any failure means "not
/// delivered" and the caller falls through to starting normally rather than
/// exiting having done nothing.
fn write_request(mut sink: impl Write, request: &ExternalRequest) -> bool {
    let Ok(payload) = serde_json::to_string(request) else {
        return false;
    };
    sink.write_all(payload.as_bytes()).is_ok()
        && sink.write_all(b"\n").is_ok()
        && sink.flush().is_ok()
}

fn read_request(source: impl Read) -> anyhow::Result<ExternalRequest> {
    let mut line = String::new();
    BufReader::new(source.take(MAX_REQUEST_BYTES)).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// Feeds every well-formed request into the UI's channel until the listener
/// dies or the UI is gone. Shared by both transports, which differ only in the
/// type of stream they yield.
fn serve_incoming<S: Read>(
    incoming: impl Iterator<Item = std::io::Result<S>>,
    tx: mpsc::UnboundedSender<ExternalRequest>,
) {
    for stream in incoming {
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
}

/// The primary's own launch request travels the same channel as the ones that
/// arrive later, so the UI has a single code path to serve.
fn primary(
    request: ExternalRequest,
    guard: SessionGuard,
) -> (mpsc::UnboundedSender<ExternalRequest>, Acquisition) {
    let (tx, requests) = mpsc::unbounded_channel();
    let _ = tx.send(request);
    (
        tx.clone(),
        Acquisition::Primary {
            requests,
            sender: tx,
            _listener: guard,
        },
    )
}

/// Startup continues without the guarantee. `mailto:` may then open a second
/// process, but refusing to launch would be worse.
fn ungoverned(request: ExternalRequest, error: impl std::fmt::Display) -> Acquisition {
    log::warn!("single-instance channel unavailable ({error}); continuing without it");
    primary(request, SessionGuard::default()).1
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

    /// Where the session socket lives, in a directory only this user can
    /// enter.
    ///
    /// XDG_RUNTIME_DIR is already per-user and cleared at logout, which is
    /// exactly the lifetime this socket wants — but it is still checked, since
    /// it comes from the environment. macOS does not set it, so the fallback is
    /// the one that runs there: a directory under the shared temp dir, created
    /// `0700` and refused unless it is ours. A socket placed directly in `/tmp`
    /// under a predictable name could be claimed first by another local user,
    /// who would then receive every `mailto:` a click hands over — or keep
    /// Aviary from ever becoming the primary.
    ///
    /// The directory is the whole access control: connecting to a Unix socket
    /// needs search permission on every directory above it, so nobody but this
    /// user (and root) can reach the socket, from either side. That is why the
    /// peer is not checked as well: `UnixStream::peer_cred` is still unstable,
    /// and `getpeereid` would mean a `libc` dependency on macOS for a check the
    /// directory already makes.
    fn socket_path() -> std::io::Result<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(dir);
            match verify_private_dir(&dir) {
                Ok(()) => return Ok(dir.join("aviary.sock")),
                Err(error) => log::warn!(
                    "XDG_RUNTIME_DIR is not a private directory ({error}); using the fallback"
                ),
            }
        }
        let dir = std::env::temp_dir().join(fallback_dir_name());
        ensure_private_dir(&dir)?;
        Ok(dir.join("aviary.sock"))
    }

    /// Stable across launches — the second process must find the first — so
    /// predictable by design; `ensure_private_dir` is what makes that safe.
    fn fallback_dir_name() -> String {
        let user = std::env::var("USER").unwrap_or_default();
        let user: String = user
            .chars()
            .filter(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
            .collect();
        let user = user.trim_start_matches('.');
        if user.is_empty() {
            "aviary-unknown".to_string()
        } else {
            format!("aviary-{user}")
        }
    }

    /// Creates `dir` as `0700`, or accepts it when it already exists and
    /// passes [`verify_private_dir`]. A directory someone else created first is
    /// refused rather than repaired: they may already have planted a socket in
    /// it, and startup then continues without the single-instance guarantee.
    pub(super) fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt as _;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        verify_private_dir(dir)
    }

    /// A real directory (not a symlink to one), owned by the current user, and
    /// closed to group and others. `/tmp` is sticky, so once this holds nobody
    /// else can rename or replace the directory under us.
    pub(super) fn verify_private_dir(dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(dir)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "{} is accessible to other users (mode {mode:o})",
                dir.display()
            )));
        }
        let owner = current_uid(dir)?;
        if metadata.uid() != owner {
            return Err(std::io::Error::other(format!(
                "{} belongs to another user",
                dir.display()
            )));
        }
        Ok(())
    }

    /// The effective uid, read off a file this process just created: `std`
    /// has no `geteuid`, and `libc` is only a dependency on Linux. Creating it
    /// inside `dir` doubles as a check that `dir` is writable at all.
    fn current_uid(dir: &Path) -> std::io::Result<u32> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        let mut suffix = [0_u8; 8];
        let suffix = match getrandom::fill(&mut suffix) {
            Ok(()) => hex::encode(suffix),
            Err(_) => std::process::id().to_string(),
        };
        let probe = dir.join(format!(".uid-probe-{suffix}"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&probe)?;
        let uid = file.metadata().map(|metadata| metadata.uid());
        drop(file);
        let _ = std::fs::remove_file(&probe);
        uid
    }

    pub(super) fn acquire(request: ExternalRequest) -> Acquisition {
        match socket_path() {
            Ok(path) => acquire_at(path, request),
            Err(error) => ungoverned(request, format_args!("{error}")),
        }
    }

    /// The socket path is a parameter so the handover can be exercised end to
    /// end in a test directory instead of the session's real runtime dir.
    pub(super) fn acquire_at(path: PathBuf, request: ExternalRequest) -> Acquisition {
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
                // same moment is the one case worth retrying: that sibling is
                // now the primary and can take the request.
                if send_to_running_instance(&path, &request) {
                    return Acquisition::HandedOver;
                }
                // Anything else — a read-only runtime dir, a path collision —
                // must not stop Aviary from starting.
                return ungoverned(request, format_args!("{error:#}"));
            }
        };

        // Belt and braces: the directory already keeps others out.
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        let guard = SessionGuard { socket: Some(path) };
        let (tx, acquisition) = primary(request, guard);

        std::thread::Builder::new()
            .name("aviary-single-instance".into())
            .spawn(move || serve_incoming(listener.incoming(), tx))
            .expect("failed to spawn the single-instance listener");

        acquisition
    }

    /// Returns true when a live instance accepted the request.
    fn send_to_running_instance(path: &PathBuf, request: &ExternalRequest) -> bool {
        let Ok(stream) = UnixStream::connect(path) else {
            return false;
        };
        write_request(stream, request)
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::*;
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};

    /// The pipe lives in the machine-wide namespace, so the user name keeps two
    /// sessions on the same host (a terminal server, a fast-user switch) from
    /// claiming each other's instance.
    fn pipe_name() -> std::io::Result<interprocess::local_socket::Name<'static>> {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        format!("aviary-{user}.sock").to_ns_name::<GenericNamespaced>()
    }

    pub(super) fn acquire(request: ExternalRequest) -> Acquisition {
        let name = match pipe_name() {
            Ok(name) => name,
            Err(error) => return ungoverned(request, error),
        };

        if let Ok(stream) = Stream::connect(name.clone()) {
            if write_request(stream, &request) {
                log::info!("another Aviary instance is running; handed the request over");
                return Acquisition::HandedOver;
            }
        }

        // A pipe with no server behind it cannot be connected to, so unlike the
        // Unix socket there is no stale name to clear away first.
        let listener = match ListenerOptions::new().name(name).create_sync() {
            Ok(listener) => listener,
            Err(error) => return ungoverned(request, error),
        };

        let (tx, acquisition) = primary(request, SessionGuard::default());

        std::thread::Builder::new()
            .name("aviary-single-instance".into())
            .spawn(move || serve_incoming(listener.incoming(), tx))
            .expect("failed to spawn the single-instance listener");

        acquisition
    }
}

// ---------------------------------------------------------------------------
// Anywhere else
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn acquire(request: ExternalRequest) -> Acquisition {
        ungoverned(request, "no local socket transport on this platform")
    }
}

/// Claims the session, or hands `request` to whoever already holds it.
///
/// The caller must act on the return value before opening any window.
pub fn acquire(request: ExternalRequest) -> Acquisition {
    platform::acquire(request)
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

    /// Both transports carry the same bytes, so the framing is worth testing
    /// once on whichever platform runs the suite.
    #[test]
    fn a_written_request_reads_back_identically() {
        let mut wire = Vec::new();
        assert!(write_request(
            &mut wire,
            &ExternalRequest::Compose(MailtoRequest {
                to: "contact@example.com".into(),
                ..Default::default()
            })
        ));

        let ExternalRequest::Compose(mailto) =
            read_request(wire.as_slice()).expect("a well-formed request")
        else {
            panic!("expected a compose request");
        };
        assert_eq!(mailto.to, "contact@example.com");
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
            ..
        } = platform::acquire_at(path.clone(), ExternalRequest::Activate)
        else {
            panic!("the first acquisition owns the session");
        };

        // The launch arguments of the primary itself arrive on the same channel.
        assert!(matches!(
            requests.blocking_recv(),
            Some(ExternalRequest::Activate)
        ));

        let second = platform::acquire_at(
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

        let acquisition = platform::acquire_at(path.clone(), ExternalRequest::Activate);
        assert!(
            matches!(acquisition, Acquisition::Primary { .. }),
            "a leftover file must not stop the session from being claimed"
        );
    }

    #[cfg(unix)]
    fn scratch_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("aviary-test-dir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The fallback directory is created closed to everyone else, and a
    /// second launch accepts the one the first created.
    #[cfg(unix)]
    #[test]
    fn the_fallback_directory_is_private_and_reusable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir("private");
        platform::ensure_private_dir(&dir).expect("a fresh directory is accepted");
        let mode = std::fs::metadata(&dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "unexpected mode {mode:o}");
        platform::ensure_private_dir(&dir).expect("our own directory is accepted again");
        assert_eq!(
            std::fs::read_dir(&dir).expect("readable").count(),
            0,
            "the ownership probe must not be left behind"
        );

        // The handover still works through a socket inside it.
        let path = dir.join("aviary.sock");
        let Acquisition::Primary { _listener, .. } =
            platform::acquire_at(path.clone(), ExternalRequest::Activate)
        else {
            panic!("the first acquisition owns the session");
        };
        assert!(matches!(
            platform::acquire_at(path, ExternalRequest::Activate),
            Acquisition::HandedOver
        ));
        drop(_listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory others can enter may already hold their socket: refused,
    /// not repaired.
    #[cfg(unix)]
    #[test]
    fn a_directory_open_to_others_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir("open");
        std::fs::create_dir(&dir).expect("creatable");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(platform::ensure_private_dir(&dir).is_err());
        let mode = std::fs::metadata(&dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o777, "a refused directory is left as found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink planted under the expected name would point the socket
    /// wherever its author likes.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_refused() {
        let target = scratch_dir("symlink-target");
        let link = scratch_dir("symlink");
        platform::ensure_private_dir(&target).expect("a real private directory");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(platform::ensure_private_dir(&link).is_err());
        assert!(platform::verify_private_dir(&link).is_err());
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[cfg(unix)]
    #[test]
    fn a_regular_file_in_place_of_the_directory_is_refused() {
        let path = scratch_dir("file");
        std::fs::write(&path, b"").expect("writable temp dir");
        assert!(platform::ensure_private_dir(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
