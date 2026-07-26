//! Session persistence on a dedicated thread.
//!
//! `AviaryApp` used to serialize and write the whole session (inside
//! settings.json) directly on the gpui thread at every autosave tick, which
//! froze the UI for seconds once the file grew. This actor receives already
//! built [`AppSession`] snapshots, serializes them compactly, skips the write
//! when nothing changed (fingerprint = last serialized form), and writes
//! `session.json` atomically. Snapshots that pile up while a write is in
//! progress are coalesced: only the most recent one is kept.

use super::settings::AppSession;
use std::sync::mpsc;

enum Req {
    Save(Box<AppSession>),
    Flush(mpsc::Sender<()>),
}

pub(crate) struct SessionStore {
    tx: mpsc::Sender<Req>,
}

impl SessionStore {
    /// `initial_fingerprint` is the serialized form of the session restored
    /// at startup, so an unchanged session is never rewritten.
    pub fn spawn(initial_fingerprint: String) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("session-store".into())
            .spawn(move || run(rx, initial_fingerprint))
            .expect("failed to spawn session-store thread");
        Self { tx }
    }

    pub fn save(&self, session: AppSession) {
        let _ = self.tx.send(Req::Save(Box::new(session)));
    }

    /// Blocks until every pending snapshot has been written. Only called on
    /// quit, where blocking the gpui thread briefly is acceptable.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Req::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(10));
        }
    }
}

fn run(rx: mpsc::Receiver<Req>, mut fingerprint: String) {
    while let Ok(first) = rx.recv() {
        let mut latest: Option<Box<AppSession>> = None;
        let mut flushes: Vec<mpsc::Sender<()>> = Vec::new();
        let mut accept = |req: Req, latest: &mut Option<Box<AppSession>>| match req {
            Req::Save(session) => *latest = Some(session),
            Req::Flush(ack) => flushes.push(ack),
        };
        accept(first, &mut latest);
        while let Ok(req) = rx.try_recv() {
            accept(req, &mut latest);
        }
        if let Some(session) = latest {
            match serde_json::to_string(&session) {
                Ok(json) if json != fingerprint => {
                    AppSession::store(json.as_bytes());
                    fingerprint = json;
                }
                Ok(_) => {}
                Err(e) => log::warn!("failed to serialize session: {e:#}"),
            }
        }
        for ack in flushes {
            let _ = ack.send(());
        }
    }
}
