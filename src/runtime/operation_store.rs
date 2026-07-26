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
        reply_all: bool,
        forward_of: Option<String>,
        draft_id: Option<String>,
        mail: OutgoingMail,
    },
    QuickAction {
        execution: QuickActionExecution,
        next_step: usize,
    },
}

impl OperationKind {
    pub(super) fn message_id(&self) -> Option<&str> {
        match self {
            Self::Delete { id } | Self::SetFlag { id, .. } | Self::MarkRead { id, .. } => Some(id),
            Self::Move { message_id, .. } => Some(message_id),
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
            Self::Send { .. } | Self::QuickAction { .. } => None,
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
        reply: oneshot::Sender<Result<Vec<StoredOperation>>>,
    },
    NextAttemptAt {
        account_id: AccountId,
        reply: oneshot::Sender<Result<Option<i64>>>,
    },
    TakeInterrupted {
        account_id: AccountId,
        reply: oneshot::Sender<Result<Vec<StoredOperation>>>,
    },
    MarkExecuting {
        id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    Remove {
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

    pub(super) async fn load_due(&self, account_id: AccountId) -> Result<Vec<StoredOperation>> {
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

    pub(super) async fn take_interrupted(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<StoredOperation>> {
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

    pub(super) fn purge_account(&self, account_id: AccountId) {
        let _ = self.tx.send(Request::PurgeAccount(account_id));
    }
}

struct OperationDb {
    conn: Connection,
}

impl OperationDb {
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
        Ok(Self { conn })
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
            let kind: OperationKind = serde_json::from_str(&json)?;
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

    fn load_due(&mut self, account_id: &AccountId, now: i64) -> Result<Vec<StoredOperation>> {
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
        let mut operations = Vec::new();
        for row in rows {
            let (id, json, attempts) = row?;
            operations.push(StoredOperation {
                id,
                account_id: account_id.clone(),
                kind: serde_json::from_str(&json)
                    .with_context(|| format!("unreadable durable operation {id}"))?,
                attempts: attempts.max(0) as u32,
            });
        }
        Ok(operations)
    }

    fn next_attempt_at(&mut self, account_id: &AccountId) -> Result<Option<i64>> {
        let next = self.conn.query_row(
            "SELECT MIN(next_attempt_at) FROM pending_operations
             WHERE account_id=?1 AND state='pending'",
            params![account_id.0],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(next)
    }

    fn take_interrupted(&mut self, account_id: &AccountId) -> Result<Vec<StoredOperation>> {
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
            let mut operations = Vec::new();
            for row in rows {
                let (id, json, attempts) = row?;
                operations.push(StoredOperation {
                    id,
                    account_id: account_id.clone(),
                    kind: serde_json::from_str(&json)
                        .with_context(|| format!("unreadable interrupted operation {id}"))?,
                    attempts: attempts.max(0) as u32,
                });
            }
            operations
        };
        transaction.execute(
            "DELETE FROM pending_operations
             WHERE account_id=?1 AND state='executing'",
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
        let mut db = OperationDb { conn };
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
        let mut db = OperationDb { conn };
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

        assert_eq!(db.load_due(&account_id, now).unwrap().len(), 1);
        db.defer(operation.id, 1, now + 60, "synthetic failure")
            .unwrap();
        assert!(db.load_due(&account_id, now).unwrap().is_empty());
        assert_eq!(db.load_due(&account_id, now + 60).unwrap()[0].attempts, 1);
        db.mark_executing(operation.id).unwrap();
        assert!(db.load_due(&account_id, now + 60).unwrap().is_empty());
        let interrupted = db.take_interrupted(&account_id).unwrap();
        assert_eq!(interrupted.len(), 1);
        assert!(db.take_interrupted(&account_id).unwrap().is_empty());
    }

    #[test]
    fn quick_action_checkpoint_keeps_only_the_next_step_pending() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb { conn };
        let account_id = AccountId("mailbox@example.test".into());
        let execution = QuickActionExecution {
            execution_id: 7,
            action_name: "Route A".into(),
            message_id: "message-a".into(),
            steps: vec![
                QuickActionStep::MarkRead { read: true },
                QuickActionStep::Move {
                    source_folder_id: Some("inbox".into()),
                    target_folder_id: "archive".into(),
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
            .unwrap();
        let (_, next_step) = due[0].kind.quick_action().unwrap();
        assert_eq!(next_step, 1);
    }

    #[test]
    fn delayed_quick_action_can_be_cancelled_before_it_is_due() {
        let conn = Connection::open_in_memory().unwrap();
        OperationDb::initialize_schema(&conn).unwrap();
        let mut db = OperationDb { conn };
        let account_id = AccountId("mailbox@example.test".into());
        let now = chrono::Utc::now().timestamp();
        let execution = QuickActionExecution {
            execution_id: 9,
            action_name: "Route B".into(),
            message_id: "message-b".into(),
            steps: vec![QuickActionStep::MarkRead { read: true }],
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

        assert!(db.load_due(&account_id, now).unwrap().is_empty());
        assert_eq!(
            db.cancel_quick_action(&account_id, 9).unwrap().as_deref(),
            Some("Route B")
        );
        assert!(db.load_due(&account_id, now + 30).unwrap().is_empty());
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
                QuickActionStep::MarkRead { read: true },
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
}
