//! Durable mail mutations and outgoing messages.
//!
//! This store is intentionally separate from `mail-cache.sqlite3`: cached mail
//! is disposable and may be cleared or recreated after a schema bump, while an
//! unsent message or an acknowledged local mutation must survive both.

use super::{MessageMutationKind, OutgoingMail, QuickActionExecution, QuickActionStep};
use crate::model::AccountId;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum OperationKind {
    Delete {
        id: String,
    },
    Move {
        message_id: String,
        source_folder_id: Option<String>,
        target_folder_id: String,
    },
    SetFlag {
        id: String,
        flagged: bool,
    },
    MarkRead {
        id: String,
        read: bool,
    },
    Send {
        compose_id: u64,
        reply_to: Option<String>,
        #[serde(default)]
        reply_all: bool,
        forward_of: Option<String>,
        draft_id: Option<String>,
        mail: OutgoingMail,
    },
    QuickAction {
        execution: QuickActionExecution,
        next_step: usize,
    },
    /// Adds or removes a tag. Only reaches the outbox through an undo window
    /// (`Cmd::ScheduleOperations`): a tag change the user can take back has to
    /// survive the process closing inside that window like every other one.
    SetTag {
        message_id: String,
        tag_id: String,
        added: bool,
    },
}

impl OperationKind {
    pub(super) fn message_id(&self) -> Option<&str> {
        match self {
            Self::Delete { id } | Self::SetFlag { id, .. } | Self::MarkRead { id, .. } => Some(id),
            Self::Move { message_id, .. } | Self::SetTag { message_id, .. } => Some(message_id),
            Self::QuickAction { execution, .. } => Some(&execution.message_id),
            Self::Send { .. } => None,
        }
    }

    pub(super) fn compose_id(&self) -> Option<u64> {
        match self {
            Self::Send { compose_id, .. } => Some(*compose_id),
            _ => None,
        }
    }

    pub(super) fn quick_action(&self) -> Option<(&QuickActionExecution, usize)> {
        match self {
            Self::QuickAction {
                execution,
                next_step,
            } => Some((execution, *next_step)),
            _ => None,
        }
    }

    pub(super) fn message_mutation_kind(&self) -> Option<MessageMutationKind> {
        match self {
            Self::Delete { .. } => Some(MessageMutationKind::Delete),
            Self::Move { .. } => Some(MessageMutationKind::Move),
            Self::SetFlag { flagged, .. } => Some(MessageMutationKind::SetFlag(*flagged)),
            Self::MarkRead { read, .. } => Some(MessageMutationKind::MarkRead(*read)),
            Self::Send { .. } | Self::QuickAction { .. } | Self::SetTag { .. } => None,
        }
    }

    pub(super) fn is_send(&self) -> bool {
        match self {
            Self::Send { .. } => true,
            Self::QuickAction {
                execution,
                next_step,
            } => matches!(
                execution.steps.get(*next_step),
                Some(QuickActionStep::Forward { .. } | QuickActionStep::Reply { .. })
            ),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct StoredOperation {
    pub id: i64,
    pub account_id: AccountId,
    pub kind: OperationKind,
    pub attempts: u32,
}

/// A row whose `kind_json` no longer decodes — written by a newer build, or
/// damaged on disk. It is set aside (`state='unreadable'`) instead of failing
/// the whole batch, which would otherwise block every other operation of the
/// account for good.
#[derive(Debug, Clone)]
pub(super) struct UnreadableOperation {
    pub id: i64,
    /// Salvaged from the raw JSON when the row still looks like a send, so the
    /// failure can be reported to its composer.
    pub compose_id: Option<u64>,
    pub error: String,
}

/// Result of loading a batch of rows: what can run, and what was set aside.
#[derive(Debug, Default)]
pub(super) struct LoadedOperations {
    pub ready: Vec<StoredOperation>,
    pub unreadable: Vec<UnreadableOperation>,
}

/// Decodes one row, turning a failure into an [`UnreadableOperation`] rather
/// than an error that would abort the batch.
fn decode_kind(id: i64, json: &str) -> std::result::Result<OperationKind, UnreadableOperation> {
    serde_json::from_str(json).map_err(|error| {
        log::error!("durable operation {id} is unreadable and was set aside: {error}");
        UnreadableOperation {
            id,
            compose_id: salvage_compose_id(json),
            error: error.to_string(),
        }
    })
}

fn salvage_compose_id(json: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("Send")?
        .get("compose_id")?
        .as_u64()
}

#[derive(Clone)]
pub(crate) struct OperationStore {
    tx: mpsc::UnboundedSender<Request>,
}

enum Request {
    Enqueue {
        account_id: AccountId,
        kind: Box<OperationKind>,
        execute_at: i64,
        reply: oneshot::Sender<Result<StoredOperation>>,
    },
    LoadDue {
        account_id: AccountId,
        now: i64,
        reply: oneshot::Sender<Result<LoadedOperations>>,
    },
    NextAttemptAt {
        account_id: AccountId,
        reply: oneshot::Sender<Result<Option<i64>>>,
    },
    TakeInterrupted {
        account_id: AccountId,
        reply: oneshot::Sender<Result<LoadedOperations>>,
    },
    MarkExecuting {
        id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    Remove {
        id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    MarkDelivered {
        id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    Defer {
        id: i64,
        attempts: u32,
        next_attempt_at: i64,
        error: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ReplaceKind {
        id: i64,
        kind: Box<OperationKind>,
        reply: oneshot::Sender<Result<()>>,
    },
    CancelQuickAction {
        account_id: AccountId,
        execution_id: u64,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    /// Inserts a whole undo window's worth of operations at once, all due at
    /// `execute_at`, and remembers their ids under `schedule_id`.
    EnqueueScheduled {
        schedule_id: u64,
        items: Vec<(AccountId, OperationKind)>,
        execute_at: i64,
        reply: oneshot::Sender<Result<Vec<StoredOperation>>>,
    },
    /// Removes the rows of a schedule that have not come due yet, replying
    /// with how many were removed out of how many were scheduled.
    CancelScheduled {
        schedule_id: u64,
        now: i64,
        reply: oneshot::Sender<Result<(usize, usize)>>,
    },
    PurgeAccount(AccountId),
}

impl OperationStore {
    pub(super) fn start() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("operation-store".into())
            .spawn(move || {
                let mut db = OperationDb::open().map_err(|error| {
                    log::error!("durable operation store unavailable: {error:#}");
                    error
                });
                // Row ids of the undo windows still open, by schedule. Kept in
                // memory on purpose: the undo button does not survive a
                // restart, and the UI's schedule ids restart with it — a
                // persisted id could cancel another run's operations.
                let mut scheduled: std::collections::HashMap<u64, (i64, Vec<i64>)> =
                    std::collections::HashMap::new();
                while let Some(request) = rx.blocking_recv() {
                    match request {
                        Request::Enqueue {
                            account_id,
                            kind,
                            execute_at,
                            reply,
                        } => {
                            let result = match &mut db {
                                Ok(db) => db.enqueue_at(account_id, *kind, execute_at),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::LoadDue {
                            account_id,
                            now,
                            reply,
                        } => {
                            let result = match &mut db {
                                Ok(db) => db.load_due(&account_id, now),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::NextAttemptAt { account_id, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.next_attempt_at(&account_id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::TakeInterrupted { account_id, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.take_interrupted(&account_id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::MarkExecuting { id, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.mark_executing(id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::Remove { id, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.remove(id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::MarkDelivered { id, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.mark_delivered(id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::Defer {
                            id,
                            attempts,
                            next_attempt_at,
                            error,
                            reply,
                        } => {
                            let result = match &mut db {
                                Ok(db) => db.defer(id, attempts, next_attempt_at, &error),
                                Err(store_error) => Err(anyhow::anyhow!(store_error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::ReplaceKind { id, kind, reply } => {
                            let result = match &mut db {
                                Ok(db) => db.replace_kind(id, *kind),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::CancelQuickAction {
                            account_id,
                            execution_id,
                            reply,
                        } => {
                            let result = match &mut db {
                                Ok(db) => db.cancel_quick_action(&account_id, execution_id),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::EnqueueScheduled {
                            schedule_id,
                            items,
                            execute_at,
                            reply,
                        } => {
                            let result = match &mut db {
                                Ok(db) => db.enqueue_many_at(items, execute_at),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            if let Ok(operations) = &result {
                                // Windows long past can no longer be cancelled.
                                let horizon = chrono::Utc::now().timestamp() - 3600;
                                scheduled.retain(|_, (due, _)| *due > horizon);
                                scheduled.insert(
                                    schedule_id,
                                    (
                                        execute_at,
                                        operations.iter().map(|operation| operation.id).collect(),
                                    ),
                                );
                            }
                            let _ = reply.send(result);
                        }
                        Request::CancelScheduled {
                            schedule_id,
                            now,
                            reply,
                        } => {
                            let ids = scheduled
                                .remove(&schedule_id)
                                .map(|(_, ids)| ids)
                                .unwrap_or_default();
                            let result = match &mut db {
                                Ok(db) => db
                                    .cancel_not_yet_due(&ids, now)
                                    .map(|cancelled| (cancelled, ids.len())),
                                Err(error) => Err(anyhow::anyhow!(error.to_string())),
                            };
                            let _ = reply.send(result);
                        }
                        Request::PurgeAccount(account_id) => {
                            if let Ok(db) = &mut db {
                                if let Err(error) = db.purge_account(&account_id) {
                                    log::warn!("purging durable operations: {error:#}");
                                }
                            }
                        }
                    }
                }
            })
            .expect("failed to spawn operation-store thread");
        Self { tx }
    }

    pub(super) async fn enqueue(
        &self,
        account_id: AccountId,
        kind: OperationKind,
    ) -> Result<StoredOperation> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Enqueue {
                account_id,
                kind: Box::new(kind),
                execute_at: chrono::Utc::now().timestamp(),
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn enqueue_at(
        &self,
        account_id: AccountId,
        kind: OperationKind,
        execute_at: i64,
    ) -> Result<StoredOperation> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Enqueue {
                account_id,
                kind: Box::new(kind),
                execute_at,
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn load_due(&self, account_id: AccountId) -> Result<LoadedOperations> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadDue {
                account_id,
                now: chrono::Utc::now().timestamp(),
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    /// Earliest `next_attempt_at` still pending for the account, whether it is
    /// already due or scheduled for later. `None` means the outbox is empty.
    pub(super) async fn next_attempt_at(&self, account_id: AccountId) -> Result<Option<i64>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::NextAttemptAt { account_id, reply })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn take_interrupted(&self, account_id: AccountId) -> Result<LoadedOperations> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::TakeInterrupted { account_id, reply })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn mark_executing(&self, id: i64) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::MarkExecuting { id, reply })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn remove(&self, id: i64) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Remove { id, reply })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    /// Takes a completed operation out of the queue for good when `remove`
    /// keeps failing: it will be neither replayed nor reported as interrupted.
    ///
    /// The id is recorded in memory before the disk is touched, so even when
    /// the update fails too the row stays inert for the rest of the session —
    /// what matters most, since the next drain is seconds away. A row that did
    /// reach `state='delivered'` is purged at the next start or drain.
    pub(super) async fn mark_delivered(&self, id: i64) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::MarkDelivered { id, reply })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn defer(
        &self,
        id: i64,
        attempts: u32,
        next_attempt_at: i64,
        error: String,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Defer {
                id,
                attempts,
                next_attempt_at,
                error,
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn replace_kind(&self, id: i64, kind: OperationKind) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::ReplaceKind {
                id,
                kind: Box::new(kind),
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) async fn cancel_quick_action(
        &self,
        account_id: AccountId,
        execution_id: u64,
    ) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::CancelQuickAction {
                account_id,
                execution_id,
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    /// Persists the operations of one undo window, due at `execute_at`, in a
    /// single transaction: a batch is either durable whole or not at all.
    pub(super) async fn enqueue_scheduled(
        &self,
        schedule_id: u64,
        items: Vec<(AccountId, OperationKind)>,
        execute_at: i64,
    ) -> Result<Vec<StoredOperation>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::EnqueueScheduled {
                schedule_id,
                items,
                execute_at,
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    /// Takes back what an undo window scheduled, as long as it has not come
    /// due. Replies `(cancelled, scheduled)`.
    pub(super) async fn cancel_scheduled(&self, schedule_id: u64) -> Result<(usize, usize)> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::CancelScheduled {
                schedule_id,
                now: chrono::Utc::now().timestamp(),
                reply,
            })
            .context("operation store stopped")?;
        rx.await.context("operation store stopped")?
    }

    pub(super) fn purge_account(&self, account_id: AccountId) {
        let _ = self.tx.send(Request::PurgeAccount(account_id));
    }
}

struct OperationDb {
    conn: Connection,
    /// Completed operations whose row could not be deleted. They are skipped
    /// by every query that feeds a drain — `load_due`, `next_attempt_at`,
    /// `take_interrupted` — and their deletion is retried before each of them.
    /// Row ids are `AUTOINCREMENT`, never reused, so a stale entry is inert.
    delivered: HashSet<i64>,
}

impl OperationDb {
    fn with_connection(conn: Connection) -> Self {
        Self {
            conn,
            delivered: HashSet::new(),
        }
    }

    fn open() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("be", "acetics", "aviary")
            .context("configuration directory unavailable")?;
        std::fs::create_dir_all(dirs.config_dir())?;
        let path: PathBuf = dirs.config_dir().join("operations.sqlite3");
        let conn = Connection::open(&path)?;
        set_private_permissions(&path);
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Self::initialize_schema(&conn)?;
        // Delivered rows are only kept so a failing delete could not replay
        // them; a fresh start has nothing left to protect.
        conn.execute("DELETE FROM pending_operations WHERE state='delivered'", [])?;
        Ok(Self::with_connection(conn))
    }

    fn initialize_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS operation_meta (
                 key TEXT PRIMARY KEY,
                 value INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS pending_operations (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_id TEXT NOT NULL,
                 kind_json TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 next_attempt_at INTEGER NOT NULL,
                 state TEXT NOT NULL DEFAULT 'pending',
                 last_error TEXT,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS pending_operations_due
               ON pending_operations(account_id,state,next_attempt_at,id);",
        )?;
        let current: Option<i64> = conn
            .query_row(
                "SELECT value FROM operation_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            anyhow::ensure!(
                current == SCHEMA_VERSION,
                "unsupported durable operation schema {current}"
            );
        }
        conn.execute(
            "INSERT OR REPLACE INTO operation_meta(key,value)
             VALUES('schema_version',?1)",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    fn enqueue_at(
        &mut self,
        account_id: AccountId,
        kind: OperationKind,
        execute_at: i64,
    ) -> Result<StoredOperation> {
        let now = chrono::Utc::now().timestamp();
        let kind_json = serde_json::to_string(&kind)?;
        self.conn.execute(
            "INSERT INTO pending_operations(
               account_id,kind_json,attempts,next_attempt_at,state,created_at
             ) VALUES(?1,?2,0,?3,'pending',?4)",
            params![account_id.0, kind_json, execute_at, now],
        )?;
        Ok(StoredOperation {
            id: self.conn.last_insert_rowid(),
            account_id,
            kind,
            attempts: 0,
        })
    }

    fn cancel_quick_action(
        &mut self,
        account_id: &AccountId,
        execution_id: u64,
    ) -> Result<Option<String>> {
        let mut statement = self.conn.prepare(
            "SELECT id,kind_json FROM pending_operations
             WHERE account_id=?1 AND state='pending' ORDER BY id",
        )?;
        let rows = statement.query_map(params![account_id.0], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut found = None;
        for row in rows {
            let (id, json) = row?;
            // An unreadable row cannot be the action being cancelled; skip it
            // rather than make every cancellation fail. `load_due` sets it
            // aside and reports it.
            let Ok(kind) = serde_json::from_str::<OperationKind>(&json) else {
                log::warn!("skipping unreadable durable operation {id} while cancelling");
                continue;
            };
            if let OperationKind::QuickAction { execution, .. } = kind {
                if execution.execution_id == execution_id {
                    found = Some((id, execution.action_name));
                    break;
                }
            }
        }
        drop(statement);
        let Some((id, name)) = found else {
            return Ok(None);
        };
        self.conn.execute(
            "DELETE FROM pending_operations WHERE id=?1 AND state='pending'",
            params![id],
        )?;
        Ok(Some(name))
    }

    fn load_due(&mut self, account_id: &AccountId, now: i64) -> Result<LoadedOperations> {
        self.retry_delivered_cleanup();
        let mut statement = self.conn.prepare(
            "SELECT id,kind_json,attempts
             FROM pending_operations
             WHERE account_id=?1 AND state='pending' AND next_attempt_at<=?2
             ORDER BY id",
        )?;
        let rows = statement.query_map(params![account_id.0, now], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut loaded = LoadedOperations::default();
        for row in rows {
            let (id, json, attempts) = row?;
            if self.delivered.contains(&id) {
                continue;
            }
            match decode_kind(id, &json) {
                Ok(kind) => loaded.ready.push(StoredOperation {
                    id,
                    account_id: account_id.clone(),
                    kind,
                    attempts: attempts.max(0) as u32,
                }),
                Err(unreadable) => loaded.unreadable.push(unreadable),
            }
        }
        drop(statement);
        self.set_aside(&loaded.unreadable)?;
        Ok(loaded)
    }

    /// Moves undecodable rows out of the queue while keeping them on disk: they
    /// no longer block the batch nor drive the retry timer, and a later build
    /// that understands them — or a person investigating — still finds them.
    fn set_aside(&self, unreadable: &[UnreadableOperation]) -> Result<()> {
        for operation in unreadable {
            self.conn.execute(
                "UPDATE pending_operations SET state='unreadable',last_error=?2 WHERE id=?1",
                params![operation.id, operation.error],
            )?;
        }
        Ok(())
    }

    fn next_attempt_at(&mut self, account_id: &AccountId) -> Result<Option<i64>> {
        self.retry_delivered_cleanup();
        if self.delivered.is_empty() {
            let next = self.conn.query_row(
                "SELECT MIN(next_attempt_at) FROM pending_operations
                 WHERE account_id=?1 AND state='pending'",
                params![account_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            return Ok(next);
        }
        // A delivered row still `pending` on disk must not arm the retry
        // timer: its deadline is past, so it would re-arm a drain that skips
        // it, forever.
        let mut statement = self.conn.prepare(
            "SELECT id,next_attempt_at FROM pending_operations
             WHERE account_id=?1 AND state='pending'",
        )?;
        let rows = statement.query_map(params![account_id.0], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut next: Option<i64> = None;
        for row in rows {
            let (id, at) = row?;
            if !self.delivered.contains(&id) {
                next = Some(next.map_or(at, |next| next.min(at)));
            }
        }
        Ok(next)
    }

    /// Retries the deletion of completed rows that `remove` failed on. Best
    /// effort: whatever still fails stays in `delivered`, and is skipped.
    fn retry_delivered_cleanup(&mut self) {
        if self.delivered.is_empty() {
            return;
        }
        let conn = &self.conn;
        self.delivered.retain(|id| {
            conn.execute("DELETE FROM pending_operations WHERE id=?1", params![id])
                .is_err()
        });
    }

    fn mark_delivered(&mut self, id: i64) -> Result<()> {
        self.delivered.insert(id);
        self.conn.execute(
            "UPDATE pending_operations SET state='delivered',last_error=NULL WHERE id=?1",
            params![id],
        )?;
        Ok(())
    }

    fn take_interrupted(&mut self, account_id: &AccountId) -> Result<LoadedOperations> {
        self.retry_delivered_cleanup();
        let delivered = &self.delivered;
        let transaction = self.conn.transaction()?;
        let operations = {
            let mut statement = transaction.prepare(
                "SELECT id,kind_json,attempts
                 FROM pending_operations
                 WHERE account_id=?1 AND state='executing'
                 ORDER BY id",
            )?;
            let rows = statement.query_map(params![account_id.0], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            let mut loaded = LoadedOperations::default();
            for row in rows {
                let (id, json, attempts) = row?;
                // Confirmed delivered, only its row outlived the send: it is
                // deleted below with the others, but it is not uncertain.
                if delivered.contains(&id) {
                    continue;
                }
                match decode_kind(id, &json) {
                    Ok(kind) => loaded.ready.push(StoredOperation {
                        id,
                        account_id: account_id.clone(),
                        kind,
                        attempts: attempts.max(0) as u32,
                    }),
                    Err(unreadable) => loaded.unreadable.push(unreadable),
                }
            }
            loaded
        };
        // Set aside before the DELETE below, which only removes what is still
        // `executing`: an unreadable row is kept for inspection, not dropped.
        for operation in &operations.unreadable {
            transaction.execute(
                "UPDATE pending_operations SET state='unreadable',last_error=?2 WHERE id=?1",
                params![operation.id, operation.error],
            )?;
        }
        transaction.execute(
            "DELETE FROM pending_operations
             WHERE account_id=?1 AND state IN ('executing','delivered')",
            params![account_id.0],
        )?;
        transaction.commit()?;
        Ok(operations)
    }

    fn mark_executing(&mut self, id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE pending_operations SET state='executing' WHERE id=?1 AND state='pending'",
            params![id],
        )?;
        anyhow::ensure!(changed == 1, "operation {id} is no longer pending");
        Ok(())
    }

    fn remove(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_operations WHERE id=?1", params![id])?;
        self.delivered.remove(&id);
        Ok(())
    }

    fn defer(&mut self, id: i64, attempts: u32, next_attempt_at: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_operations SET
               attempts=?2,next_attempt_at=?3,state='pending',last_error=?4
             WHERE id=?1",
            params![id, attempts, next_attempt_at, error],
        )?;
        Ok(())
    }

    fn replace_kind(&mut self, id: i64, kind: OperationKind) -> Result<()> {
        let kind_json = serde_json::to_string(&kind)?;
        let changed = self.conn.execute(
            "UPDATE pending_operations SET
               kind_json=?2,attempts=0,next_attempt_at=?3,state='pending',last_error=NULL
             WHERE id=?1",
            params![id, kind_json, chrono::Utc::now().timestamp()],
        )?;
        anyhow::ensure!(changed == 1, "operation {id} is no longer available");
        Ok(())
    }

    fn purge_account(&mut self, account_id: &AccountId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_operations WHERE account_id=?1",
            params![account_id.0],
        )?;
        Ok(())
    }

    fn enqueue_many_at(
        &mut self,
        items: Vec<(AccountId, OperationKind)>,
        execute_at: i64,
    ) -> Result<Vec<StoredOperation>> {
        let now = chrono::Utc::now().timestamp();
        let transaction = self.conn.transaction()?;
        let mut operations = Vec::with_capacity(items.len());
        for (account_id, kind) in items {
            let kind_json = serde_json::to_string(&kind)?;
            transaction.execute(
                "INSERT INTO pending_operations(
                   account_id,kind_json,attempts,next_attempt_at,state,created_at
                 ) VALUES(?1,?2,0,?3,'pending',?4)",
                params![account_id.0, kind_json, execute_at, now],
            )?;
            operations.push(StoredOperation {
                id: transaction.last_insert_rowid(),
                account_id,
                kind,
                attempts: 0,
            });
        }
        transaction.commit()?;
        Ok(operations)
    }

    /// Deletes those of `ids` that no drain can have picked up yet: still
    /// pending, never attempted, and due strictly after `now`. A row due now
    /// may already be running — `load_due` takes rows due at or before the
    /// current second — so it is left alone and reported as not cancelled.
    fn cancel_not_yet_due(&mut self, ids: &[i64], now: i64) -> Result<usize> {
        let transaction = self.conn.transaction()?;
        let mut cancelled = 0;
        for id in ids {
            cancelled += transaction.execute(
                "DELETE FROM pending_operations
                 WHERE id=?1 AND state='pending' AND attempts=0 AND next_attempt_at>?2",
                params![id, now],
            )?;
        }
        transaction.commit()?;
        Ok(cancelled)
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_kind_round_trips_without_losing_targets() {
        let kind = OperationKind::Move {
            message_id: "message-1".into(),
            source_folder_id: Some("inbox".into()),
            target_folder_id: "archive".into(),
        };
        let encoded = serde_json::to_string(&kind).unwrap();
        let decoded: OperationKind = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.message_id(), Some("message-1"));
    }

    /// The deadline drives `operations::arm_retry_timer`; without it the
    /// exponential backoff would only run whenever something else happened to
    /// drain the account.
    #[test]
    fn the_next_deadline_tracks_a_deferred_retry() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb::with_connection(conn);
        let account_id = AccountId("account@example.test".into());
        let now = chrono::Utc::now().timestamp();

        assert_eq!(db.next_attempt_at(&account_id).unwrap(), None);

        let operation = db
            .enqueue_at(
                account_id.clone(),
                OperationKind::Delete {
                    id: "message-1".into(),
                },
                now,
            )
            .unwrap();
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), Some(now));

        db.defer(operation.id, 1, now + 300, "synthetic failure")
            .unwrap();
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), Some(now + 300));

        db.remove(operation.id).unwrap();
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), None);
    }

    #[test]
    fn queue_defers_and_removes_operations_transactionally() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb::with_connection(conn);
        let account_id = AccountId("account@example.test".into());
        let operation = db
            .enqueue_at(
                account_id.clone(),
                OperationKind::Delete {
                    id: "message-1".into(),
                },
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        let now = chrono::Utc::now().timestamp();

        assert_eq!(db.load_due(&account_id, now).unwrap().ready.len(), 1);
        db.defer(operation.id, 1, now + 60, "synthetic failure")
            .unwrap();
        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(
            db.load_due(&account_id, now + 60).unwrap().ready[0].attempts,
            1
        );
        db.mark_executing(operation.id).unwrap();
        assert!(db.load_due(&account_id, now + 60).unwrap().ready.is_empty());
        let interrupted = db.take_interrupted(&account_id).unwrap();
        assert_eq!(interrupted.ready.len(), 1);
        assert!(db.take_interrupted(&account_id).unwrap().ready.is_empty());
    }

    #[test]
    fn quick_action_checkpoint_keeps_only_the_next_step_pending() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb::with_connection(conn);
        let account_id = AccountId("mailbox@example.test".into());
        let execution = QuickActionExecution {
            execution_id: 7,
            action_name: "Route A".into(),
            message_id: "message-a".into(),
            steps: vec![
                QuickActionStep::MarkRead {
                    read: true,
                    message_id: None,
                },
                QuickActionStep::Move {
                    source_folder_id: Some("inbox".into()),
                    target_folder_id: "archive".into(),
                    message_id: None,
                },
            ],
        };
        let operation = db
            .enqueue_at(
                account_id.clone(),
                OperationKind::QuickAction {
                    execution: execution.clone(),
                    next_step: 0,
                },
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        db.replace_kind(
            operation.id,
            OperationKind::QuickAction {
                execution,
                next_step: 1,
            },
        )
        .unwrap();

        let due = db
            .load_due(&account_id, chrono::Utc::now().timestamp())
            .unwrap()
            .ready;
        let (_, next_step) = due[0].kind.quick_action().unwrap();
        assert_eq!(next_step, 1);
    }

    #[test]
    fn delayed_quick_action_can_be_cancelled_before_it_is_due() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb::with_connection(conn);
        let account_id = AccountId("mailbox@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let execution = QuickActionExecution {
            execution_id: 9,
            action_name: "Route B".into(),
            message_id: "message-b".into(),
            steps: vec![QuickActionStep::MarkRead {
                read: true,
                message_id: None,
            }],
        };
        db.enqueue_at(
            account_id.clone(),
            OperationKind::QuickAction {
                execution,
                next_step: 0,
            },
            now + 30,
        )
        .unwrap();

        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(
            db.cancel_quick_action(&account_id, 9).unwrap().as_deref(),
            Some("Route B")
        );
        assert!(db.load_due(&account_id, now + 30).unwrap().ready.is_empty());
    }

    #[test]
    fn quick_reply_is_a_non_repeatable_send_step() {
        let execution = QuickActionExecution {
            execution_id: 11,
            action_name: "Reply A".into(),
            message_id: "message-c".into(),
            steps: vec![
                QuickActionStep::Reply {
                    mail: OutgoingMail {
                        to: vec!["contact-a@example.test".into()],
                        cc: Vec::new(),
                        bcc: Vec::new(),
                        subject: "Synthetic subject".into(),
                        body: "<p>Synthetic response</p>".into(),
                        body_is_html: true,
                        attachments: Vec::new(),
                        files: Vec::new(),
                    },
                    reply_all: false,
                },
                QuickActionStep::MarkRead {
                    read: true,
                    message_id: None,
                },
            ],
        };
        let send = OperationKind::QuickAction {
            execution: execution.clone(),
            next_step: 0,
        };
        let triage = OperationKind::QuickAction {
            execution,
            next_step: 1,
        };

        assert!(send.is_send());
        assert!(!triage.is_send());
    }

    fn memory_db() -> OperationDb {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        OperationDb::with_connection(conn)
    }

    fn insert_raw(db: &OperationDb, account_id: &AccountId, json: &str, state: &str) -> i64 {
        db.conn
            .execute(
                "INSERT INTO pending_operations(
                   account_id,kind_json,attempts,next_attempt_at,state,created_at
                 ) VALUES(?1,?2,0,0,?3,0)",
                params![account_id.0, json, state],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    /// One undecodable row used to fail the whole batch, and since it was
    /// never removed, every later drain of the account failed the same way.
    #[test]
    fn an_unreadable_row_is_set_aside_without_blocking_the_batch() {
        let mut db = memory_db();
        let account_id = AccountId("account@example.test".into());
        let broken = insert_raw(&db, &account_id, r#"{"Teleport":{}}"#, "pending");
        db.enqueue_at(
            account_id.clone(),
            OperationKind::Delete {
                id: "message-1".into(),
            },
            0,
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();

        let loaded = db.load_due(&account_id, now).unwrap();
        assert_eq!(loaded.ready.len(), 1);
        assert_eq!(loaded.unreadable.len(), 1);
        assert_eq!(loaded.unreadable[0].id, broken);
        assert_eq!(loaded.unreadable[0].compose_id, None);

        // Reported once, then out of the queue and of the retry deadline.
        let again = db.load_due(&account_id, now).unwrap();
        assert!(again.unreadable.is_empty());
        let state: String = db
            .conn
            .query_row(
                "SELECT state FROM pending_operations WHERE id=?1",
                params![broken],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "unreadable");
        db.remove(again.ready[0].id).unwrap();
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), None);
    }

    /// A send must not vanish silently: its composer is named when the row
    /// still says which one it was.
    #[test]
    fn an_unreadable_send_keeps_its_compose_id() {
        let mut db = memory_db();
        let account_id = AccountId("account@example.test".into());
        insert_raw(
            &db,
            &account_id,
            r#"{"Send":{"compose_id":42,"mail":"not a mail"}}"#,
            "executing",
        );

        let interrupted = db.take_interrupted(&account_id).unwrap();
        assert!(interrupted.ready.is_empty());
        assert_eq!(interrupted.unreadable[0].compose_id, Some(42));
        // Kept on disk rather than deleted with the readable interrupted rows.
        let kept: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_operations WHERE state='unreadable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
    }

    #[test]
    fn cancelling_a_quick_action_skips_unreadable_rows() {
        let mut db = memory_db();
        let account_id = AccountId("mailbox@example.test".into());
        insert_raw(&db, &account_id, "{not json", "pending");
        db.enqueue_at(
            account_id.clone(),
            OperationKind::QuickAction {
                execution: QuickActionExecution {
                    execution_id: 5,
                    action_name: "Route C".into(),
                    message_id: "message-d".into(),
                    steps: vec![QuickActionStep::MarkRead {
                        read: true,
                        message_id: None,
                    }],
                },
                next_step: 0,
            },
            0,
        )
        .unwrap();

        assert_eq!(
            db.cancel_quick_action(&account_id, 5).unwrap().as_deref(),
            Some("Route C")
        );
    }

    /// Rows written by an older build lack whatever fields were added since;
    /// `#[serde(default)]` keeps them readable.
    #[test]
    fn a_send_written_before_a_field_was_added_still_decodes() {
        let json = r#"{"Send":{"compose_id":3,"reply_to":null,"forward_of":null,
            "draft_id":null,"mail":{"to":["contact-a@example.test"],
            "subject":"Synthetic","body":"<p>x</p>"}}}"#;
        let kind: OperationKind = serde_json::from_str(json).unwrap();
        assert_eq!(kind.compose_id(), Some(3));
    }

    /// An undo window is persisted whole, stays out of `load_due` until its
    /// deadline, and can be taken back until then — but not once it is due,
    /// since a drain may already be running it.
    #[test]
    fn a_scheduled_window_is_durable_and_cancellable_until_due() {
        let mut db = memory_db();
        let account_id = AccountId("mailbox@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let operations = db
            .enqueue_many_at(
                vec![
                    (
                        account_id.clone(),
                        OperationKind::Delete {
                            id: "INBOX:1".into(),
                        },
                    ),
                    (
                        account_id.clone(),
                        OperationKind::SetTag {
                            message_id: "INBOX:2".into(),
                            tag_id: "tag-a".into(),
                            added: true,
                        },
                    ),
                ],
                now + 10,
            )
            .unwrap();
        let ids: Vec<_> = operations.iter().map(|operation| operation.id).collect();

        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(db.load_due(&account_id, now + 10).unwrap().ready.len(), 2);
        // Due: a drain may be executing it, so it is not taken back.
        assert_eq!(db.cancel_not_yet_due(&ids, now + 10).unwrap(), 0);
        assert_eq!(db.cancel_not_yet_due(&ids, now).unwrap(), 2);
        assert!(db.load_due(&account_id, now + 10).unwrap().ready.is_empty());
    }

    fn delete_operation(db: &mut OperationDb, account_id: &AccountId, now: i64) -> i64 {
        db.enqueue_at(
            account_id.clone(),
            OperationKind::Delete {
                id: "message-1".into(),
            },
            now,
        )
        .unwrap()
        .id
    }

    /// Makes every write to the queue fail, as a full or locked disk would.
    fn break_writes(db: &OperationDb) {
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER no_delete BEFORE DELETE ON pending_operations
                   BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;
                 CREATE TEMP TRIGGER no_update BEFORE UPDATE ON pending_operations
                   BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;",
            )
            .unwrap();
    }

    fn repair_writes(db: &OperationDb) {
        db.conn
            .execute_batch("DROP TRIGGER no_delete; DROP TRIGGER no_update;")
            .unwrap();
    }

    /// A send confirmed by the provider whose row could not be deleted is
    /// delivered, not interrupted: the next drain must not report it as
    /// "delivery uncertain".
    #[test]
    fn a_delivered_row_is_not_reported_as_interrupted() {
        let mut db = memory_db();
        let account_id = AccountId("account@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let id = delete_operation(&mut db, &account_id, now);
        db.mark_executing(id).unwrap();

        db.mark_delivered(id).unwrap();

        assert!(db.take_interrupted(&account_id).unwrap().ready.is_empty());
        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), None);
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pending_operations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "the drain purges delivered rows");
    }

    /// When the disk refuses both the delete and the state change, the row is
    /// still kept out of the queue for the session — neither replayed nor
    /// reported — and deleted as soon as the disk accepts it again.
    #[test]
    fn a_delivered_row_stays_inert_when_the_disk_refuses_every_write() {
        let mut db = memory_db();
        let account_id = AccountId("account@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let sent = delete_operation(&mut db, &account_id, now);
        db.mark_executing(sent).unwrap();
        // A quick action's last checkpoint leaves its row pending.
        let finished = delete_operation(&mut db, &account_id, now);

        break_writes(&db);
        assert!(db.remove(sent).is_err());
        assert!(db.mark_delivered(sent).is_err());
        assert!(db.remove(finished).is_err());
        assert!(db.mark_delivered(finished).is_err());

        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(
            db.next_attempt_at(&account_id).unwrap(),
            None,
            "a delivered row must not arm the retry timer"
        );

        repair_writes(&db);
        assert!(db.take_interrupted(&account_id).unwrap().ready.is_empty());
        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert!(db.delivered.is_empty());
    }

    /// Delivered rows left on disk by a previous run are purged by the next
    /// drain, and never counted as pending in the meantime.
    #[test]
    fn a_delivered_row_on_disk_is_purged_by_the_next_drain() {
        let mut db = memory_db();
        let account_id = AccountId("account@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let id = delete_operation(&mut db, &account_id, now);
        db.mark_delivered(id).unwrap();
        // A restart forgets the in-memory set; the disk state must suffice.
        db.delivered.clear();

        assert!(db.load_due(&account_id, now).unwrap().ready.is_empty());
        assert_eq!(db.next_attempt_at(&account_id).unwrap(), None);
        assert!(db.take_interrupted(&account_id).unwrap().ready.is_empty());
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pending_operations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }
}
