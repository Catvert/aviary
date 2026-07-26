//! Local mailbox cache.
//!
//! The SQLite connection stays on a dedicated actor thread: the Tokio runtime
//! and, especially, the gpui thread never perform blocking disk I/O.

use super::protocol::{Evt, RecipientUsage, SearchScope};
use crate::model::{AccountId, Attachment, BodyFormat, CalendarInvitation, Message, MessageHeader};
use crate::search_query::SearchQuery;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const INBOX_KEY: &str = "__inbox__";
const SCHEMA_VERSION: &str = "7";

/// Upper bound on the body text handed to the full-text index, in bytes. A
/// newsletter can carry hundreds of kilobytes of boilerplate whose tail never
/// decides a search; truncating keeps the index proportional to what users
/// actually query while the message itself stays cached in full.
const MAX_INDEXED_BODY_BYTES: usize = 64 * 1024;

/// Old cache entries may predate provider-side RFC header unfolding. Keep
/// control characters out of gpui text runs while the refreshed header is
/// being fetched and overwrites the stored value.
fn normalize_cached_header(header: &mut MessageHeader) {
    for value in [&mut header.subject, &mut header.from, &mut header.preview] {
        if value.contains('\r') || value.contains('\n') {
            *value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
}

/// Body-only cache representation. The current header is stored separately
/// and injected on read, so synchronizing flags/tags never deserializes and
/// rewrites a potentially large body JSON document.
#[derive(Serialize, Deserialize)]
struct CachedBody {
    body: String,
    format: BodyFormat,
    attachments: Vec<Attachment>,
    raw_body: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    draft_id: Option<String>,
    #[serde(default)]
    invitation: Option<CalendarInvitation>,
}

impl CachedBody {
    fn from_message(message: &Message) -> Self {
        let mut attachments = message.attachments.clone();
        for attachment in &mut attachments {
            attachment.bytes = None;
        }
        Self {
            body: message.body.clone(),
            format: message.format,
            attachments,
            raw_body: message.raw_body.clone(),
            to: message.to.clone(),
            cc: message.cc.clone(),
            bcc: message.bcc.clone(),
            draft_id: message.draft_id.clone(),
            invitation: message.invitation.clone(),
        }
    }

    fn into_message(self, header: MessageHeader) -> Message {
        let tags = header.tags.clone();
        Message {
            header,
            body: self.body,
            format: self.format,
            inline_images: Vec::new(),
            attachments: self.attachments,
            tags,
            raw_body: self.raw_body,
            to: self.to,
            cc: self.cc,
            bcc: self.bcc,
            draft_id: self.draft_id,
            invitation: self.invitation,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStats {
    pub used_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Clone)]
pub struct MailCache {
    tx: mpsc::UnboundedSender<Request>,
}

enum Request {
    LoadHeader {
        account_id: AccountId,
        message_id: String,
        reply: oneshot::Sender<Result<Option<MessageHeader>>>,
    },
    LoadHeaders {
        account_id: AccountId,
        folder_id: Option<String>,
        limit: usize,
        skip: usize,
        reply: oneshot::Sender<Result<Vec<MessageHeader>>>,
    },
    StoreHeaders {
        account_id: AccountId,
        folder_id: Option<String>,
        headers: Vec<MessageHeader>,
    },
    ConversationTotals {
        account_id: AccountId,
        folder_id: Option<String>,
        reply: oneshot::Sender<Result<HashMap<String, usize>>>,
    },
    LoadMessage {
        account_id: AccountId,
        message_id: String,
        reply: oneshot::Sender<Result<Option<Message>>>,
    },
    Search {
        query: SearchQuery,
        account_id: Option<AccountId>,
        scope: SearchScope,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<MessageHeader>>>,
    },
    StoreMessage {
        account_id: AccountId,
        message: Box<Message>,
    },
    StoreAttachment {
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        bytes: Vec<u8>,
    },
    UpdateHeader {
        account_id: AccountId,
        message_id: String,
        mutation: HeaderMutation,
    },
    RenameTag {
        account_id: AccountId,
        old_tag: String,
        new_tag: String,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveMessage {
        account_id: AccountId,
        message_id: String,
    },
    RemoveFromFolder {
        account_id: AccountId,
        folder_id: Option<String>,
        message_id: String,
    },
    MoveMessage {
        account_id: AccountId,
        message_id: String,
        source_folder_id: Option<String>,
        target_folder_id: String,
        new_id: Option<String>,
    },
    PurgeAccount(AccountId),
    PurgeFolder {
        account_id: AccountId,
        folder_id: Option<String>,
    },
    Clear(oneshot::Sender<Result<()>>),
    SetLimit(u64, oneshot::Sender<Result<CacheStats>>),
    Stats(oneshot::Sender<Result<CacheStats>>),
    LoadCursor {
        account_id: AccountId,
        folder_id: Option<String>,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    StoreCursor {
        account_id: AccountId,
        folder_id: Option<String>,
        provider: String,
        cursor: String,
    },
    LoadRecipientUsage {
        reply: oneshot::Sender<Result<Vec<RecipientUsage>>>,
    },
    RecordRecipientUsage {
        emails: Vec<String>,
        reply: oneshot::Sender<Result<Vec<RecipientUsage>>>,
    },
}

enum HeaderMutation {
    Read(bool),
    Flag(bool),
    Tag { tag_id: String, added: bool },
}

/// The actor's database, or the error that kept it from opening. The cache is
/// disposable, so a failed open is not fatal: the actor keeps draining its
/// queue and every request is answered from this `Err`.
type ActorDb = Result<CacheDb>;

/// Serves a request whose sender awaits a value. Without a database, the open
/// error is forwarded so the caller falls back on the provider instead of
/// waiting for a reply that would never come.
fn answer<T>(
    db: &mut ActorDb,
    reply: oneshot::Sender<Result<T>>,
    query: impl FnOnce(&mut CacheDb) -> Result<T>,
) {
    let result = match db {
        Ok(db) => query(db),
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    };
    let _ = reply.send(result);
}

/// Runs a write nobody awaits. A failure only loses cached data, so it is
/// logged under `context` and the actor moves on.
fn apply(db: &mut ActorDb, context: &str, write: impl FnOnce(&mut CacheDb) -> Result<()>) {
    if let Ok(db) = db {
        if let Err(e) = write(db) {
            log::warn!("{context}: {e:#}");
        }
    }
}

impl MailCache {
    pub fn start(limit_mb: u64) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut db = CacheDb::open(limit_mb).map_err(|e| {
                log::error!("mail cache unavailable: {e:#}");
                e
            });
            while let Some(request) = rx.blocking_recv() {
                match request {
                    Request::LoadHeader {
                        account_id,
                        message_id,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.load_header(&account_id, &message_id)
                    }),
                    Request::LoadHeaders {
                        account_id,
                        folder_id,
                        limit,
                        skip,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.load_headers(&account_id, folder_id.as_deref(), limit, skip)
                    }),
                    Request::StoreHeaders {
                        account_id,
                        folder_id,
                        headers,
                    } => apply(&mut db, "header cache write failed", |db| {
                        db.store_headers(&account_id, folder_id.as_deref(), &headers)
                    }),
                    Request::ConversationTotals {
                        account_id,
                        folder_id,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.conversation_totals(&account_id, folder_id.as_deref())
                    }),
                    Request::LoadMessage {
                        account_id,
                        message_id,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.load_message(&account_id, &message_id)
                    }),
                    Request::Search {
                        query,
                        account_id,
                        scope,
                        limit,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.search(&query, account_id.as_ref(), &scope, limit)
                    }),
                    Request::StoreMessage {
                        account_id,
                        message,
                    } => apply(&mut db, "body cache", |db| {
                        db.store_message(&account_id, &message)
                    }),
                    Request::StoreAttachment {
                        account_id,
                        message_id,
                        attachment_id,
                        bytes,
                    } => apply(&mut db, "attachment cache write failed", |db| {
                        db.store_attachment(&account_id, &message_id, &attachment_id, &bytes)
                    }),
                    Request::UpdateHeader {
                        account_id,
                        message_id,
                        mutation,
                    } => apply(&mut db, "cache update failed", |db| {
                        db.update_header(&account_id, &message_id, mutation)
                    }),
                    Request::RenameTag {
                        account_id,
                        old_tag,
                        new_tag,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.rename_tag(&account_id, &old_tag, &new_tag)
                    }),
                    Request::RemoveMessage {
                        account_id,
                        message_id,
                    } => apply(&mut db, "cache deletion", |db| {
                        db.remove_message(&account_id, &message_id)
                    }),
                    Request::RemoveFromFolder {
                        account_id,
                        folder_id,
                        message_id,
                    } => apply(&mut db, "cache folder removal", |db| {
                        db.remove_from_folder(&account_id, folder_id.as_deref(), &message_id)
                    }),
                    Request::MoveMessage {
                        account_id,
                        message_id,
                        source_folder_id,
                        target_folder_id,
                        new_id,
                    } => apply(&mut db, "cache move failed", |db| {
                        db.move_message(
                            &account_id,
                            &message_id,
                            source_folder_id.as_deref(),
                            &target_folder_id,
                            new_id.as_deref(),
                        )
                    }),
                    Request::PurgeAccount(account_id) => {
                        apply(&mut db, "purging account cache", |db| {
                            db.purge_account(&account_id)
                        })
                    }
                    Request::PurgeFolder {
                        account_id,
                        folder_id,
                    } => apply(&mut db, "purging folder cache", |db| {
                        db.purge_folder(&account_id, folder_id.as_deref())
                    }),
                    Request::Clear(reply) => answer(&mut db, reply, |db| db.clear()),
                    Request::SetLimit(limit_mb, reply) => {
                        answer(&mut db, reply, |db| db.set_limit(limit_mb))
                    }
                    Request::Stats(reply) => answer(&mut db, reply, |db| db.stats()),
                    Request::LoadCursor {
                        account_id,
                        folder_id,
                        reply,
                    } => answer(&mut db, reply, |db| {
                        db.load_cursor(&account_id, folder_id.as_deref())
                    }),
                    Request::StoreCursor {
                        account_id,
                        folder_id,
                        provider,
                        cursor,
                    } => apply(&mut db, "sync cursor cache write failed", |db| {
                        db.store_cursor(&account_id, folder_id.as_deref(), &provider, &cursor)
                    }),
                    Request::LoadRecipientUsage { reply } => {
                        answer(&mut db, reply, |db| db.load_recipient_usage())
                    }
                    Request::RecordRecipientUsage { emails, reply } => {
                        answer(&mut db, reply, |db| db.record_recipient_usage(emails))
                    }
                }
            }
        });
        Self { tx }
    }

    pub async fn load_header(
        &self,
        account_id: AccountId,
        message_id: String,
    ) -> Result<Option<MessageHeader>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadHeader {
                account_id,
                message_id,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub async fn load_headers(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
        limit: usize,
        skip: usize,
    ) -> Result<Vec<MessageHeader>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadHeaders {
                account_id,
                folder_id,
                limit,
                skip,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub fn store_headers(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
        headers: Vec<MessageHeader>,
    ) {
        let _ = self.tx.send(Request::StoreHeaders {
            account_id,
            folder_id,
            headers,
        });
    }

    /// How many messages the cache knows of in each thread of a folder,
    /// restricted to threads with more than one — the only ones the message
    /// list renders as a group.
    ///
    /// A listed page holds a few dozen messages; a thread often spreads well
    /// beyond it. Counting here rather than in the list is what keeps a
    /// group's counter from creeping upward as the user scrolls, and it costs
    /// no network round trip.
    pub async fn conversation_totals(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
    ) -> Result<HashMap<String, usize>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::ConversationTotals {
                account_id,
                folder_id,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub async fn load_message(
        &self,
        account_id: AccountId,
        message_id: String,
    ) -> Result<Option<Message>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadMessage {
                account_id,
                message_id,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    /// Ranked full-text search over the cached mailbox. `account_id` restricts
    /// the scope; `None` searches every cached account at once — something no
    /// provider can do, since each only knows its own mailbox.
    pub async fn search(
        &self,
        query: SearchQuery,
        account_id: Option<AccountId>,
        scope: SearchScope,
        limit: usize,
    ) -> Result<Vec<MessageHeader>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Search {
                query,
                account_id,
                scope,
                limit,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub fn store_message(&self, account_id: AccountId, message: Message) {
        let _ = self.tx.send(Request::StoreMessage {
            account_id,
            message: Box::new(message),
        });
    }

    pub fn store_attachment(
        &self,
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        bytes: Vec<u8>,
    ) {
        let _ = self.tx.send(Request::StoreAttachment {
            account_id,
            message_id,
            attachment_id,
            bytes,
        });
    }

    pub fn set_read(&self, account_id: AccountId, message_id: String, read: bool) {
        self.update_header(account_id, message_id, HeaderMutation::Read(read));
    }

    pub fn set_flag(&self, account_id: AccountId, message_id: String, flagged: bool) {
        self.update_header(account_id, message_id, HeaderMutation::Flag(flagged));
    }

    pub fn set_tag(&self, account_id: AccountId, message_id: String, tag_id: String, added: bool) {
        self.update_header(
            account_id,
            message_id,
            HeaderMutation::Tag { tag_id, added },
        );
    }

    pub async fn rename_tag(
        &self,
        account_id: AccountId,
        old_tag: String,
        new_tag: String,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::RenameTag {
                account_id,
                old_tag,
                new_tag,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    fn update_header(&self, account_id: AccountId, message_id: String, mutation: HeaderMutation) {
        let _ = self.tx.send(Request::UpdateHeader {
            account_id,
            message_id,
            mutation,
        });
    }

    pub fn remove_message(&self, account_id: AccountId, message_id: String) {
        let _ = self.tx.send(Request::RemoveMessage {
            account_id,
            message_id,
        });
    }

    pub fn remove_from_folder(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
        message_id: String,
    ) {
        let _ = self.tx.send(Request::RemoveFromFolder {
            account_id,
            folder_id,
            message_id,
        });
    }

    pub fn move_message(
        &self,
        account_id: AccountId,
        message_id: String,
        source_folder_id: Option<String>,
        target_folder_id: String,
        new_id: Option<String>,
    ) {
        let _ = self.tx.send(Request::MoveMessage {
            account_id,
            message_id,
            source_folder_id,
            target_folder_id,
            new_id,
        });
    }

    pub fn purge_account(&self, account_id: AccountId) {
        let _ = self.tx.send(Request::PurgeAccount(account_id));
    }

    pub fn purge_folder(&self, account_id: AccountId, folder_id: Option<String>) {
        let _ = self.tx.send(Request::PurgeFolder {
            account_id,
            folder_id,
        });
    }

    pub async fn clear(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Clear(reply))
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub async fn set_limit(&self, limit_mb: u64) -> Result<CacheStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::SetLimit(limit_mb, reply))
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub async fn stats(&self) -> Result<CacheStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Stats(reply))
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub async fn load_cursor(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
    ) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadCursor {
                account_id,
                folder_id,
                reply,
            })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    pub fn store_cursor(
        &self,
        account_id: AccountId,
        folder_id: Option<String>,
        provider: String,
        cursor: String,
    ) {
        let _ = self.tx.send(Request::StoreCursor {
            account_id,
            folder_id,
            provider,
            cursor,
        });
    }

    pub async fn load_recipient_usage(&self) -> Result<Vec<RecipientUsage>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::LoadRecipientUsage { reply })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }

    /// Counts every unique address at most once for a single successful
    /// outgoing message or calendar invitation.
    pub async fn record_recipient_usage(&self, emails: Vec<String>) -> Result<Vec<RecipientUsage>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::RecordRecipientUsage { emails, reply })
            .context("cache actor stopped")?;
        rx.await.context("cache actor stopped")?
    }
}

struct CacheDb {
    conn: Connection,
    limit_bytes: u64,
}

impl CacheDb {
    fn open(limit_mb: u64) -> Result<Self> {
        let dirs = directories::ProjectDirs::from("be", "acetics", "aviary")
            .context("configuration directory unavailable")?;
        std::fs::create_dir_all(dirs.config_dir())?;
        let path: PathBuf = dirs.config_dir().join("mail-cache.sqlite3");
        let conn = Connection::open(&path)?;
        set_private_permissions(&path);
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        Self::initialize_schema(&conn)?;
        let mut db = Self {
            conn,
            limit_bytes: limit_mb.max(100) * 1024 * 1024,
        };
        db.enforce_limit()?;
        Ok(db)
    }

    fn initialize_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(SCHEMA_VERSION) {
            if current.is_some() {
                log::info!("mail cache schema changed; discarding the old cache");
            }
            // Cached mail is disposable. A schema change starts from an empty
            // cache instead of carrying migration code for serialized bodies.
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS messages_fts_delete;
                 DROP TABLE IF EXISTS messages_fts;
                 DROP TABLE IF EXISTS folder_messages;
                 DROP TABLE IF EXISTS message_inline_images;
                 DROP TABLE IF EXISTS message_attachments;
                 DROP TABLE IF EXISTS messages;
                 DROP TABLE IF EXISTS sync_state;
                 DELETE FROM cache_meta;",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 account_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 header_json TEXT NOT NULL,
                 body_json TEXT,
                 last_access INTEGER NOT NULL,
                 cache_bytes INTEGER NOT NULL DEFAULT 0,
                 conversation_id TEXT,
                 PRIMARY KEY(account_id, message_id)
             );
             CREATE TABLE IF NOT EXISTS message_inline_images (
                 account_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 cid TEXT NOT NULL,
                 mime TEXT NOT NULL,
                 bytes BLOB NOT NULL,
                 PRIMARY KEY(account_id, message_id, position),
                 FOREIGN KEY(account_id, message_id)
                   REFERENCES messages(account_id, message_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS message_attachments (
                 account_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 attachment_id TEXT NOT NULL,
                 bytes BLOB NOT NULL,
                 PRIMARY KEY(account_id, message_id, attachment_id),
                 FOREIGN KEY(account_id, message_id)
                   REFERENCES messages(account_id, message_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS folder_messages (
                 account_id TEXT NOT NULL,
                 folder_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 received INTEGER NOT NULL,
                 PRIMARY KEY(account_id, folder_id, message_id),
                 FOREIGN KEY(account_id, message_id)
                   REFERENCES messages(account_id, message_id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS folder_messages_order
               ON folder_messages(account_id, folder_id, received DESC);
             CREATE INDEX IF NOT EXISTS messages_body_lru
               ON messages(last_access) WHERE body_json IS NOT NULL;
             CREATE TABLE IF NOT EXISTS sync_state (
                 account_id TEXT NOT NULL,
                 folder_id TEXT NOT NULL,
                 provider TEXT NOT NULL,
                 cursor TEXT,
                 auxiliary TEXT,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY(account_id, folder_id)
             );
             CREATE TABLE IF NOT EXISTS recipient_usage (
                 email TEXT PRIMARY KEY COLLATE NOCASE,
                 use_count INTEGER NOT NULL,
                 last_used INTEGER NOT NULL
             );",
        )?;
        // Full-text index over what a mailbox search actually targets. It is
        // `content=''` (contentless): FTS5 stores the inverted index only and
        // never a second copy of the text, which matters under a cache quota.
        // Rows are addressed by `messages.rowid` — SQLite keeps it stable
        // across the `ON CONFLICT DO UPDATE` upserts used throughout this
        // module — so results join straight back onto the source table.
        //
        // `contentless_delete=1` (SQLite 3.43+) is what allows deleting index
        // rows without replaying their original text.
        //
        // `remove_diacritics 2` folds accents, so "reunion" finds "Réunion":
        // no one types accents into a search field.
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                 subject, sender, recipients, body,
                 content = '', contentless_delete = 1,
                 tokenize = \"unicode61 remove_diacritics 2\"
             );
             -- Deletion is a trigger rather than a call at each site: messages
             -- are removed from half a dozen places (per-message, per-folder,
             -- per-account, purge, clear, id reassignment during a move) and
             -- an index row outliving its message would surface results that
             -- can no longer be opened.
             CREATE TRIGGER IF NOT EXISTS messages_fts_delete
             AFTER DELETE ON messages BEGIN
                 DELETE FROM messages_fts WHERE rowid = OLD.rowid;
             END;",
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO cache_meta(key, value)
             VALUES('schema_version', ?1)",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    fn folder_key(folder_id: Option<&str>) -> &str {
        folder_id.unwrap_or(INBOX_KEY)
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn load_header(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
    ) -> Result<Option<MessageHeader>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT header_json FROM messages
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id.0, message_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        let mut header = serde_json::from_str(&json)?;
        normalize_cached_header(&mut header);
        Ok(Some(header))
    }

    fn load_headers(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
        limit: usize,
        skip: usize,
    ) -> Result<Vec<MessageHeader>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.header_json
             FROM folder_messages fm
             JOIN messages m ON m.account_id=fm.account_id AND m.message_id=fm.message_id
             WHERE fm.account_id=?1 AND fm.folder_id=?2
             ORDER BY fm.received DESC LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![
                account_id.0,
                Self::folder_key(folder_id),
                limit as i64,
                skip as i64
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut headers = Vec::new();
        for row in rows {
            match serde_json::from_str(&row?) {
                Ok(mut header) => {
                    normalize_cached_header(&mut header);
                    headers.push(header);
                }
                Err(e) => log::warn!("unreadable cached header: {e:#}"),
            }
        }
        Ok(headers)
    }

    /// Ranked full-text search over the cached mailbox.
    ///
    /// `bm25` is FTS5's relevance function (the Lucene/Elasticsearch one);
    /// the weights make a hit in the subject outrank the same word buried in
    /// a body, and it returns *negative* scores with the best match lowest,
    /// hence the plain ascending sort.
    fn search(
        &mut self,
        query: &SearchQuery,
        account_id: Option<&AccountId>,
        scope: &SearchScope,
        limit: usize,
    ) -> Result<Vec<MessageHeader>> {
        let Some(expression) = fts_match_expression(query) else {
            return Ok(Vec::new());
        };
        // Date and flag filters are applied in Rust, on the deserialized
        // header, rather than through `json_extract` in SQL: the header is a
        // JSON document, so a SQL-side filter would parse it a second time for
        // every candidate row. Over-fetching then trimming keeps a filtered
        // query from returning a near-empty page.
        let over_fetch = if query.has_operators() {
            limit.saturating_mul(4).max(limit)
        } else {
            limit
        };
        // The folder restriction is an EXISTS rather than a join: a message
        // can sit in several folders (Gmail labels), and joining would return
        // it once per membership.
        let mut statement = self.conn.prepare(
            "SELECT m.account_id, m.header_json
               FROM messages_fts f
               JOIN messages m ON m.rowid = f.rowid
              WHERE messages_fts MATCH ?1
                AND (?2 IS NULL OR m.account_id = ?2)
                AND (?3 IS NULL OR EXISTS (
                      SELECT 1 FROM folder_messages fm
                       WHERE fm.account_id = m.account_id
                         AND fm.message_id = m.message_id
                         AND fm.folder_id = ?3
                    ))
              ORDER BY bm25(messages_fts, 10.0, 4.0, 2.0, 1.0)
              LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                expression,
                account_id.map(|id| id.0.as_str()),
                scope.folder().map(Self::folder_key),
                over_fetch as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut headers = Vec::new();
        for row in rows {
            let (account, header_json) = row?;
            match serde_json::from_str::<MessageHeader>(&header_json) {
                Ok(mut header) => {
                    normalize_cached_header(&mut header);
                    header.account_id = AccountId(account);
                    if query.matches(&header) {
                        headers.push(header);
                    }
                }
                Err(e) => log::warn!("unreadable cached header in search results: {e:#}"),
            }
            if headers.len() >= limit {
                break;
            }
        }
        Ok(headers)
    }

    fn store_headers(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
        headers: &[MessageHeader],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for header in headers {
            let json = serde_json::to_string(header)?;
            let json_len = json.len() as i64;
            tx.execute(
                "INSERT INTO messages(
                   account_id,message_id,header_json,last_access,cache_bytes,conversation_id
                 ) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(account_id,message_id) DO UPDATE SET
                   cache_bytes=MAX(
                     0,messages.cache_bytes-length(CAST(messages.header_json AS BLOB))
                   )+excluded.cache_bytes,
                   header_json=excluded.header_json,
                   conversation_id=excluded.conversation_id",
                params![
                    account_id.0,
                    header.id,
                    json,
                    Self::now(),
                    json_len,
                    header.conversation_id
                ],
            )?;
            tx.execute(
                "INSERT INTO folder_messages(account_id,folder_id,message_id,received)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(account_id,folder_id,message_id)
                 DO UPDATE SET received=excluded.received",
                params![
                    account_id.0,
                    Self::folder_key(folder_id),
                    header.id,
                    header.received.timestamp()
                ],
            )?;
            // Listing a folder is what makes the bulk of the mailbox
            // searchable: most messages are never opened, so this is the only
            // chance to index their subject and sender.
            let indexed: Option<(i64, Option<String>)> = tx
                .query_row(
                    "SELECT rowid,body_json FROM messages
                     WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, header.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((rowid, body_json)) = indexed {
                index_message(&tx, rowid, header, body_json.as_deref())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Per-thread message counts for one folder, threads of a single message
    /// excluded (`HAVING` clause): they render as ordinary rows and would make
    /// up the bulk of the result set for nothing.
    fn conversation_totals(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
    ) -> Result<HashMap<String, usize>> {
        let mut statement = self.conn.prepare(
            "SELECT m.conversation_id, COUNT(*) AS total
             FROM folder_messages f
             JOIN messages m
               ON m.account_id = f.account_id AND m.message_id = f.message_id
             WHERE f.account_id = ?1 AND f.folder_id = ?2
               AND m.conversation_id IS NOT NULL AND m.conversation_id <> ''
             GROUP BY m.conversation_id
             HAVING total > 1",
        )?;
        let rows = statement
            .query_map(params![account_id.0, Self::folder_key(folder_id)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    fn load_message(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
    ) -> Result<Option<Message>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT header_json,body_json FROM messages
                 WHERE account_id=?1 AND message_id=?2 AND body_json IS NOT NULL",
                params![account_id.0, message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((header_json, body_json)) = row else {
            return Ok(None);
        };
        self.conn.execute(
            "UPDATE messages SET last_access=?3 WHERE account_id=?1 AND message_id=?2",
            params![account_id.0, message_id, Self::now()],
        )?;
        let mut header: MessageHeader = serde_json::from_str(&header_json)?;
        normalize_cached_header(&mut header);
        let cached: CachedBody = serde_json::from_str(&body_json)?;
        let mut message = cached.into_message(header);
        let mut inline = self.conn.prepare(
            "SELECT cid,mime,bytes FROM message_inline_images
             WHERE account_id=?1 AND message_id=?2 ORDER BY position",
        )?;
        let images = inline.query_map(params![account_id.0, message_id], |row| {
            Ok(crate::model::InlineImage {
                cid: row.get(0)?,
                mime: row.get(1)?,
                bytes: row.get(2)?,
            })
        })?;
        message.inline_images = images.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(inline);

        let mut attachments = self.conn.prepare(
            "SELECT attachment_id,bytes FROM message_attachments
             WHERE account_id=?1 AND message_id=?2",
        )?;
        let payloads = attachments.query_map(params![account_id.0, message_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let payloads = payloads.collect::<rusqlite::Result<HashMap<_, _>>>()?;
        for (position, attachment) in message.attachments.iter_mut().enumerate() {
            if let Some(bytes) = payloads.get(&attachment_cache_id(position, attachment)) {
                attachment.bytes = Some(bytes.clone());
            }
        }
        crate::providers::remove_embedded_image_attachments(&mut message);
        Ok(Some(message))
    }

    fn store_message(&mut self, account_id: &AccountId, message: &Message) -> Result<()> {
        // A draft opens a composer and may still change rapidly;
        // Caching it would reopen two composers during the network refresh
        // that follows the local response.
        if message.draft_id.is_some() {
            return Ok(());
        }
        let cached = CachedBody::from_message(message);
        let header_json = serde_json::to_string(&message.header)?;
        let body_json = serde_json::to_string(&cached)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO messages(
               account_id,message_id,header_json,body_json,last_access,cache_bytes,
               conversation_id
             ) VALUES(?1,?2,?3,?4,?5,0,?6)
             ON CONFLICT(account_id,message_id) DO UPDATE SET
               header_json=excluded.header_json,
               body_json=excluded.body_json,
               last_access=excluded.last_access,
               conversation_id=excluded.conversation_id",
            params![
                account_id.0,
                message.header.id,
                header_json,
                body_json,
                Self::now(),
                message.header.conversation_id
            ],
        )?;
        tx.execute(
            "DELETE FROM message_inline_images
             WHERE account_id=?1 AND message_id=?2",
            params![account_id.0, message.header.id],
        )?;
        for (position, image) in message.inline_images.iter().enumerate() {
            tx.execute(
                "INSERT INTO message_inline_images(
                   account_id,message_id,position,cid,mime,bytes
                 ) VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    account_id.0,
                    message.header.id,
                    position as i64,
                    image.cid,
                    image.mime,
                    image.bytes
                ],
            )?;
        }
        let current_attachment_ids: HashSet<String> = message
            .attachments
            .iter()
            .enumerate()
            .map(|(position, attachment)| attachment_cache_id(position, attachment))
            .collect();
        let mut existing = tx.prepare(
            "SELECT attachment_id FROM message_attachments
             WHERE account_id=?1 AND message_id=?2",
        )?;
        let existing_ids = existing
            .query_map(params![account_id.0, message.header.id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        drop(existing);
        for attachment_id in existing_ids
            .into_iter()
            .filter(|id| !current_attachment_ids.contains(id))
        {
            tx.execute(
                "DELETE FROM message_attachments
                 WHERE account_id=?1 AND message_id=?2 AND attachment_id=?3",
                params![account_id.0, message.header.id, attachment_id],
            )?;
        }
        for (position, attachment) in message.attachments.iter().enumerate() {
            let Some(bytes) = &attachment.bytes else {
                continue;
            };
            let attachment_id = attachment_cache_id(position, attachment);
            tx.execute(
                "INSERT INTO message_attachments(
                   account_id,message_id,attachment_id,bytes
                 ) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(account_id,message_id,attachment_id)
                 DO UPDATE SET bytes=excluded.bytes",
                params![account_id.0, message.header.id, attachment_id, bytes],
            )?;
        }
        recompute_message_size(&tx, account_id, &message.header.id)?;
        let rowid: i64 = tx.query_row(
            "SELECT rowid FROM messages WHERE account_id=?1 AND message_id=?2",
            params![account_id.0, message.header.id],
            |row| row.get(0),
        )?;
        index_message(&tx, rowid, &message.header, Some(&body_json))?;
        tx.commit()?;
        self.enforce_limit()
    }

    fn store_attachment(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        attachment_id: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let body_exists: Option<bool> = tx
            .query_row(
                "SELECT body_json IS NOT NULL FROM messages
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id.0, message_id],
                |row| row.get(0),
            )
            .optional()?;
        if body_exists != Some(true) {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO message_attachments(
               account_id,message_id,attachment_id,bytes
             ) VALUES(?1,?2,?3,?4)
             ON CONFLICT(account_id,message_id,attachment_id)
             DO UPDATE SET bytes=excluded.bytes",
            params![account_id.0, message_id, attachment_id, bytes],
        )?;
        recompute_message_size(&tx, account_id, message_id)?;
        tx.commit()?;
        self.enforce_limit()
    }

    fn update_header(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        mutation: HeaderMutation,
    ) -> Result<()> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT header_json FROM messages
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id.0, message_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(header_json) = row else {
            return Ok(());
        };
        let mut header: MessageHeader = serde_json::from_str(&header_json)?;
        let apply = |header: &mut MessageHeader| match &mutation {
            HeaderMutation::Read(read) => header.is_read = *read,
            HeaderMutation::Flag(flagged) => header.is_flagged = *flagged,
            HeaderMutation::Tag { tag_id, added } => {
                header.tags.retain(|tag| tag != tag_id);
                if *added {
                    header.tags.push(tag_id.clone());
                }
            }
        };
        apply(&mut header);
        let header_json = serde_json::to_string(&header)?;
        self.conn.execute(
            "UPDATE messages SET
               cache_bytes=MAX(
                 0,cache_bytes-length(CAST(header_json AS BLOB))
               )+?3,
               header_json=?4
             WHERE account_id=?1 AND message_id=?2",
            params![
                account_id.0,
                message_id,
                header_json.len() as i64,
                header_json
            ],
        )?;
        Ok(())
    }

    fn rename_tag(&mut self, account_id: &AccountId, old_tag: &str, new_tag: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let rows = {
            let mut statement =
                tx.prepare("SELECT message_id,header_json FROM messages WHERE account_id=?1")?;
            let rows = statement
                .query_map(params![account_id.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        for (message_id, header_json) in rows {
            let mut header: MessageHeader = serde_json::from_str(&header_json)?;
            if !replace_tag_value(&mut header.tags, old_tag, new_tag) {
                continue;
            }
            let updated_json = serde_json::to_string(&header)?;
            tx.execute(
                "UPDATE messages SET
                   cache_bytes=MAX(
                     0,cache_bytes-length(CAST(header_json AS BLOB))
                   )+?3,
                   header_json=?4
                 WHERE account_id=?1 AND message_id=?2",
                params![
                    account_id.0,
                    message_id,
                    updated_json.len() as i64,
                    updated_json
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn remove_message(&mut self, account_id: &AccountId, message_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE account_id=?1 AND message_id=?2",
            params![account_id.0, message_id],
        )?;
        Ok(())
    }

    fn remove_from_folder(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
        message_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM folder_messages
             WHERE account_id=?1 AND folder_id=?2 AND message_id=?3",
            params![account_id.0, Self::folder_key(folder_id), message_id],
        )?;
        self.conn.execute(
            "DELETE FROM messages WHERE account_id=?1 AND message_id=?2 AND NOT EXISTS (
               SELECT 1 FROM folder_messages fm
               WHERE fm.account_id=messages.account_id AND fm.message_id=messages.message_id
             )",
            params![account_id.0, message_id],
        )?;
        Ok(())
    }

    fn move_message(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        source_folder_id: Option<&str>,
        target_folder_id: &str,
        new_id: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let received: Option<i64> = tx
            .query_row(
                "SELECT received FROM folder_messages
                 WHERE account_id=?1 AND folder_id=?2 AND message_id=?3",
                params![account_id.0, Self::folder_key(source_folder_id), message_id],
                |row| row.get(0),
            )
            .optional()?;
        let cached: Option<(String, Option<String>, i64, i64)> = tx
            .query_row(
                "SELECT header_json,body_json,last_access,cache_bytes FROM messages
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id.0, message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        tx.execute(
            "DELETE FROM folder_messages
             WHERE account_id=?1 AND folder_id=?2 AND message_id=?3",
            params![account_id.0, Self::folder_key(source_folder_id), message_id],
        )?;
        if let Some(new_id) = new_id.filter(|id| *id != message_id) {
            if let Some((header_json, body_json, last_access, cache_bytes)) = cached {
                let mut header: MessageHeader = serde_json::from_str(&header_json)?;
                header.id = new_id.to_string();
                let header_json = serde_json::to_string(&header)?;
                tx.execute(
                    "INSERT OR REPLACE INTO messages(
                       account_id,message_id,header_json,body_json,last_access,cache_bytes
                     ) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        account_id.0,
                        new_id,
                        header_json,
                        body_json,
                        last_access,
                        cache_bytes
                    ],
                )?;
                tx.execute(
                    "INSERT INTO message_inline_images(
                       account_id,message_id,position,cid,mime,bytes
                     ) SELECT account_id,?3,position,cid,mime,bytes
                       FROM message_inline_images
                       WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, message_id, new_id],
                )?;
                tx.execute(
                    "INSERT INTO message_attachments(
                       account_id,message_id,attachment_id,bytes
                     ) SELECT account_id,?3,attachment_id,bytes FROM message_attachments
                     WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, message_id, new_id],
                )?;
                recompute_message_size(&tx, account_id, new_id)?;
                // The reinsert above allocated a fresh rowid, and deleting the
                // old row below fires the index trigger — so the message has
                // to be indexed again under its new identity.
                let rowid: i64 = tx.query_row(
                    "SELECT rowid FROM messages WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, new_id],
                    |row| row.get(0),
                )?;
                index_message(&tx, rowid, &header, body_json.as_deref())?;
                tx.execute(
                    "UPDATE folder_messages SET message_id=?3
                     WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, message_id, new_id],
                )?;
                tx.execute(
                    "DELETE FROM messages WHERE account_id=?1 AND message_id=?2",
                    params![account_id.0, message_id],
                )?;
            }
        }
        let stored_id = new_id.unwrap_or(message_id);
        if let Some(received) = received {
            tx.execute(
                "INSERT OR REPLACE INTO folder_messages(account_id,folder_id,message_id,received)
                 VALUES(?1,?2,?3,?4)",
                params![account_id.0, target_folder_id, stored_id, received],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn purge_account(&mut self, account_id: &AccountId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE account_id=?1",
            params![account_id.0],
        )?;
        self.conn.execute(
            "DELETE FROM sync_state WHERE account_id=?1",
            params![account_id.0],
        )?;
        self.conn.execute_batch("PRAGMA incremental_vacuum")?;
        Ok(())
    }

    fn purge_folder(&mut self, account_id: &AccountId, folder_id: Option<&str>) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2",
            params![account_id.0, Self::folder_key(folder_id)],
        )?;
        tx.execute(
            "DELETE FROM sync_state WHERE account_id=?1 AND folder_id=?2",
            params![account_id.0, Self::folder_key(folder_id)],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE account_id=?1 AND NOT EXISTS (
               SELECT 1 FROM folder_messages fm
               WHERE fm.account_id=messages.account_id AND fm.message_id=messages.message_id
             ) AND message_id NOT LIKE 'aviary-sent-%'",
            params![account_id.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM folder_messages;
             DELETE FROM messages;
             DELETE FROM sync_state;
             DELETE FROM recipient_usage;
             PRAGMA wal_checkpoint(TRUNCATE);
             PRAGMA incremental_vacuum;",
        )?;
        Ok(())
    }

    fn set_limit(&mut self, limit_mb: u64) -> Result<CacheStats> {
        self.limit_bytes = limit_mb.max(100) * 1024 * 1024;
        self.enforce_limit()?;
        self.stats()
    }

    fn stats(&mut self) -> Result<CacheStats> {
        let used: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(cache_bytes),0) FROM messages",
            [],
            |row| row.get(0),
        )?;
        Ok(CacheStats {
            used_bytes: used.max(0) as u64,
            limit_bytes: self.limit_bytes,
        })
    }

    fn load_cursor(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT cursor FROM sync_state WHERE account_id=?1 AND folder_id=?2",
                params![account_id.0, Self::folder_key(folder_id)],
                |row| row.get(0),
            )
            .optional()
            .map(|cursor| cursor.filter(|value: &String| !value.is_empty()))
            .map_err(Into::into)
    }

    fn store_cursor(
        &mut self,
        account_id: &AccountId,
        folder_id: Option<&str>,
        provider: &str,
        cursor: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state(account_id,folder_id,provider,cursor,updated_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(account_id,folder_id) DO UPDATE SET
               provider=excluded.provider,cursor=excluded.cursor,updated_at=excluded.updated_at",
            params![
                account_id.0,
                Self::folder_key(folder_id),
                provider,
                cursor,
                Self::now()
            ],
        )?;
        Ok(())
    }

    fn load_recipient_usage(&mut self) -> Result<Vec<RecipientUsage>> {
        let mut statement = self.conn.prepare(
            "SELECT email,use_count,last_used
             FROM recipient_usage
             ORDER BY use_count DESC,last_used DESC,email ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(RecipientUsage {
                email: row.get(0)?,
                use_count: row.get::<_, i64>(1)?.max(0) as u64,
                last_used: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn record_recipient_usage(&mut self, emails: Vec<String>) -> Result<Vec<RecipientUsage>> {
        let mut normalized = HashSet::new();
        for email in emails {
            let email = email.trim().to_lowercase();
            if !email.is_empty() {
                normalized.insert(email);
            }
        }
        if normalized.is_empty() {
            return Ok(Vec::new());
        }

        let now = Self::now();
        let tx = self.conn.transaction()?;
        for email in &normalized {
            tx.execute(
                "INSERT INTO recipient_usage(email,use_count,last_used)
                 VALUES(?1,1,?2)
                 ON CONFLICT(email) DO UPDATE SET
                   use_count=recipient_usage.use_count+1,
                   last_used=excluded.last_used",
                params![email, now],
            )?;
        }

        let mut entries = Vec::with_capacity(normalized.len());
        {
            let mut statement = tx.prepare(
                "SELECT email,use_count,last_used
                 FROM recipient_usage WHERE email=?1",
            )?;
            for email in normalized {
                entries.push(statement.query_row(params![email], |row| {
                    Ok(RecipientUsage {
                        email: row.get(0)?,
                        use_count: row.get::<_, i64>(1)?.max(0) as u64,
                        last_used: row.get(2)?,
                    })
                })?);
            }
        }
        tx.commit()?;
        Ok(entries)
    }

    fn enforce_limit(&mut self) -> Result<()> {
        let mut used = self.stats()?.used_bytes;
        if used <= self.limit_bytes {
            return Ok(());
        }
        let mut statement = self.conn.prepare(
            "SELECT account_id,message_id,
                    MAX(
                      0,cache_bytes-length(CAST(header_json AS BLOB))
                    ) AS reclaimable
             FROM messages
             WHERE body_json IS NOT NULL
               AND message_id NOT LIKE 'aviary-sent-%'
             ORDER BY last_access ASC",
        )?;
        let candidates = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?.max(0) as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut victims = Vec::new();
        for (account_id, message_id, reclaimable) in candidates {
            victims.push((account_id, message_id));
            used = used.saturating_sub(reclaimable);
            if used <= self.limit_bytes {
                break;
            }
        }
        if victims.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for (account_id, message_id) in victims {
            tx.execute(
                "DELETE FROM message_inline_images
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id, message_id],
            )?;
            tx.execute(
                "DELETE FROM message_attachments
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id, message_id],
            )?;
            tx.execute(
                "UPDATE messages SET
                   body_json=NULL,
                   cache_bytes=length(CAST(header_json AS BLOB))
                 WHERE account_id=?1 AND message_id=?2",
                params![account_id, message_id],
            )?;
            // The body is gone but the header survives, so the message stays
            // searchable by subject and sender. Leaving the old body indexed
            // would return hits on text the cache can no longer show.
            let indexed: Option<(i64, String)> = tx
                .query_row(
                    "SELECT rowid,header_json FROM messages
                     WHERE account_id=?1 AND message_id=?2",
                    params![account_id, message_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((rowid, header_json)) = indexed {
                match serde_json::from_str::<MessageHeader>(&header_json) {
                    Ok(header) => index_message(&tx, rowid, &header, None)?,
                    Err(e) => log::warn!("unreadable cached header while evicting: {e:#}"),
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Renders a parsed query into an FTS5 `MATCH` expression.
///
/// Nothing typed by the user reaches `MATCH` unquoted. FTS5 has a query syntax
/// of its own, so an ordinary keystroke would break it: a bare `facture:` reads
/// as a filter on a column that does not exist, an unbalanced `"` is a syntax
/// error, and either fails the *whole* statement rather than returning nothing.
/// Quoting each term as a literal phrase also makes the punctuation inert.
///
/// Field operators become FTS5 column filters, which is what makes `objet:`
/// cheaper and sharper than post-filtering: `subject:"contrat"` only walks the
/// subject postings.
///
/// The last free-text term gets a prefix `*` so results narrow while the user
/// is still typing — "fact" already matches "facture".
///
/// Returns `None` when nothing in the query can drive the index (a query made
/// only of date or flag filters, say), so the caller can fall back instead of
/// running a `MATCH` that matches nothing.
fn fts_match_expression(query: &SearchQuery) -> Option<String> {
    fn phrase(term: &str) -> Option<String> {
        // A quote would end the literal phrase being built; the tokenizer
        // ignores it anyway.
        let cleaned = term.replace('"', " ");
        let cleaned = cleaned.trim();
        cleaned
            .chars()
            .any(char::is_alphanumeric)
            .then(|| format!("\"{cleaned}\""))
    }

    let mut clauses: Vec<String> = Vec::new();
    for (column, terms) in [
        ("subject", &query.subject),
        ("sender", &query.from),
        ("recipients", &query.to),
    ] {
        for term in terms {
            if let Some(phrase) = phrase(term) {
                clauses.push(format!("{column}:{phrase}"));
            }
        }
    }
    let free: Vec<String> = query.terms.iter().filter_map(|term| phrase(term)).collect();
    let last = free.len().saturating_sub(1);
    for (index, phrase) in free.into_iter().enumerate() {
        if index == last {
            clauses.push(format!("{phrase}*"));
        } else {
            clauses.push(phrase);
        }
    }
    (!clauses.is_empty()).then(|| clauses.join(" AND "))
}

/// Text handed to the full-text index for a message body.
///
/// The cached body is the Markdown the provider layer converts HTML into, so
/// it still carries link targets, image references and the odd inline tag.
/// URLs are the worst offenders: one tracking link contributes dozens of junk
/// tokens, and indexing them would let a search for a person's name match
/// every newsletter that happens to carry it inside a query string. Link
/// *text* is kept — that is what the reader saw and what they will search for.
fn searchable_body_text(body: &str) -> String {
    let mut out = String::with_capacity(body.len().min(MAX_INDEXED_BODY_BYTES));
    let mut chars = body.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        match c {
            // Residual inline HTML: drop the whole tag.
            '<' => {
                for (_, c) in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
                out.push(' ');
            }
            // `![alt](url)` — an image contributes nothing searchable.
            '!' if chars.peek().is_some_and(|(_, next)| *next == '[') => {
                chars.next();
                for (_, c) in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                }
                skip_link_target(&mut chars);
                out.push(' ');
            }
            // `[text](url)` — keep the text, drop the target.
            ']' => skip_link_target(&mut chars),
            '[' => out.push(' '),
            // Markdown punctuation carries no meaning once tokenized.
            '#' | '*' | '_' | '`' | '>' | '|' | '~' => out.push(' '),
            c if c.is_whitespace() => out.push(' '),
            c => out.push(c),
        }
        if out.len() >= MAX_INDEXED_BODY_BYTES {
            break;
        }
    }
    // FTS5 tokenizes on its own, but collapsing here keeps the stored text
    // (and the tests) predictable.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Consumes a `(...)` link target, tolerating the nested parentheses that
/// appear in real URLs. A no-op when the `]` was not part of a link.
fn skip_link_target(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    if !chars.peek().is_some_and(|(_, next)| *next == '(') {
        return;
    }
    chars.next();
    let mut depth = 1usize;
    for (_, c) in chars.by_ref() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// (Re)builds the full-text entry for one cached message.
///
/// A contentless FTS5 table has no UPDATE, so replacing an entry means delete
/// then insert. Messages whose body was never fetched — or was evicted — fall
/// back to the provider's preview, the only body text available for them.
fn index_message(
    conn: &Connection,
    rowid: i64,
    header: &MessageHeader,
    body_json: Option<&str>,
) -> Result<()> {
    let cached = body_json.and_then(|json| serde_json::from_str::<CachedBody>(json).ok());
    let body = cached
        .as_ref()
        .map(|cached| searchable_body_text(&cached.body))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| searchable_body_text(&header.preview));
    // Recipients only exist once a message has been fetched: a folder listing
    // carries none. `to:` therefore searches opened messages only — which is
    // where it matters, since on received mail the recipient is the user.
    let recipients = cached
        .as_ref()
        .map(|cached| {
            cached
                .to
                .iter()
                .chain(cached.cc.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    conn.execute("DELETE FROM messages_fts WHERE rowid=?1", params![rowid])?;
    conn.execute(
        "INSERT INTO messages_fts(rowid,subject,sender,recipients,body)
         VALUES(?1,?2,?3,?4,?5)",
        params![rowid, header.subject, header.from, recipients, body],
    )?;
    Ok(())
}

fn attachment_cache_id(position: usize, attachment: &Attachment) -> String {
    if attachment.id.is_empty() {
        format!("position:{position}")
    } else {
        attachment.id.clone()
    }
}

fn recompute_message_size(
    tx: &rusqlite::Transaction<'_>,
    account_id: &AccountId,
    message_id: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE messages SET cache_bytes=
           length(CAST(header_json AS BLOB))
           +COALESCE(length(CAST(body_json AS BLOB)),0)
           +COALESCE((
             SELECT SUM(length(bytes)) FROM message_inline_images images
             WHERE images.account_id=messages.account_id
               AND images.message_id=messages.message_id
           ),0)
           +COALESCE((
             SELECT SUM(length(bytes)) FROM message_attachments attachments
             WHERE attachments.account_id=messages.account_id
               AND attachments.message_id=messages.message_id
           ),0)
         WHERE account_id=?1 AND message_id=?2",
        params![account_id.0, message_id],
    )?;
    Ok(())
}

fn replace_tag_value(tags: &mut Vec<String>, old_tag: &str, new_tag: &str) -> bool {
    let mut replaced = false;
    for tag in tags.iter_mut() {
        if tag == old_tag {
            tag.clear();
            tag.push_str(new_tag);
            replaced = true;
        }
    }
    if replaced {
        let mut seen = HashSet::new();
        tags.retain(|tag| seen.insert(tag.clone()));
    }
    replaced
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

/// Applies a new size cap and reports the resulting usage.
pub(super) async fn apply_limit(global: Arc<super::BgGlobal>, limit_mb: u64) {
    match global.cache.set_limit(limit_mb).await {
        Ok(stats) => global.emit(Evt::MailCacheStats {
            used_bytes: stats.used_bytes,
            limit_bytes: stats.limit_bytes,
        }),
        Err(e) => global.emit(Evt::Error(
            tr!("runtime-error-cache", { error: format!("{e:#}") }).to_string(),
        )),
    }
}

/// Empties the cache, then reports the usage the UI should now display.
pub(super) async fn clear_and_report(global: Arc<super::BgGlobal>) {
    match global.cache.clear().await {
        Ok(()) => {
            global.emit(Evt::MailCacheCleared);
            if let Ok(stats) = global.cache.stats().await {
                global.emit(Evt::MailCacheStats {
                    used_bytes: stats.used_bytes,
                    limit_bytes: stats.limit_bytes,
                });
            }
        }
        Err(e) => global.emit(Evt::Error(
            tr!("runtime-error-cache", { error: format!("{e:#}") }).to_string(),
        )),
    }
}

/// Reports current usage. A failure here is only a display detail, so it is
/// logged rather than surfaced.
pub(super) async fn report_stats(global: Arc<super::BgGlobal>) {
    match global.cache.stats().await {
        Ok(stats) => global.emit(Evt::MailCacheStats {
            used_bytes: stats.used_bytes,
            limit_bytes: stats.limit_bytes,
        }),
        Err(e) => log::warn!("cache statistics: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, BodyFormat, InlineImage};

    fn test_db() -> CacheDb {
        let conn = Connection::open_in_memory().expect("in-memory cache");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("foreign keys");
        CacheDb::initialize_schema(&conn).expect("cache schema");
        CacheDb {
            conn,
            limit_bytes: 100 * 1024 * 1024,
        }
    }

    fn message() -> Message {
        Message {
            header: MessageHeader {
                id: "message-a".into(),
                account_id: AccountId("account-a".into()),
                subject: "Message de test".into(),
                from: "Contact A <contact-a@example.test>".into(),
                received: chrono::DateTime::UNIX_EPOCH,
                preview: "Contenu synthétique".into(),
                is_read: true,
                is_flagged: false,
                has_attachments: true,
                tags: Vec::new(),
                last_action: None,
                last_action_at: None,
                conversation_id: Some("conversation-a".into()),
                internet_message_id: None,
            },
            body: "![Logo](bytes://cid-logo)".into(),
            format: BodyFormat::Markdown,
            inline_images: vec![InlineImage {
                cid: "logo".into(),
                mime: "image/png".into(),
                bytes: vec![1, 2, 3],
            }],
            attachments: vec![Attachment {
                id: "attachment-a".into(),
                filename: "document.bin".into(),
                mime: "application/octet-stream".into(),
                size: 3,
                bytes: Some(vec![4, 5, 6]),
            }],
            tags: Vec::new(),
            raw_body: Some("<img src=\"cid:logo\">".into()),
            to: vec!["Contact B <contact-b@example.test>".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            draft_id: None,
            invitation: None,
        }
    }

    #[test]
    fn binary_payloads_round_trip_through_blob_tables() {
        let mut db = test_db();
        let message = message();
        let account_id = message.header.account_id.clone();
        db.store_message(&account_id, &message)
            .expect("store message");

        let body_json: String = db
            .conn
            .query_row("SELECT body_json FROM messages", [], |row| row.get(0))
            .expect("body metadata");
        assert!(!body_json.contains("AQL"), "image bytes leaked into JSON");
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT typeof(bytes) FROM message_inline_images",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("inline image storage"),
            "blob"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT typeof(bytes) FROM message_attachments", [], |row| {
                    row.get::<_, String>(0)
                },)
                .expect("attachment storage"),
            "blob"
        );

        let restored = db
            .load_message(&account_id, &message.header.id)
            .expect("load message")
            .expect("cached message");
        assert_eq!(restored.inline_images, message.inline_images);
        assert_eq!(restored.attachments[0].bytes, Some(vec![4, 5, 6]));
    }

    #[test]
    fn header_refresh_does_not_rewrite_cached_body() {
        let mut db = test_db();
        let message = message();
        let account_id = message.header.account_id.clone();
        db.store_message(&account_id, &message)
            .expect("store message");
        let before: String = db
            .conn
            .query_row("SELECT body_json FROM messages", [], |row| row.get(0))
            .expect("body metadata");

        let mut header = message.header.clone();
        header.subject = "Sujet actualisé".into();
        header.tags = vec!["tag-a".into()];
        db.store_headers(&account_id, None, &[header])
            .expect("refresh header");

        let after: String = db
            .conn
            .query_row("SELECT body_json FROM messages", [], |row| row.get(0))
            .expect("body metadata");
        assert_eq!(before, after);
        let restored = db
            .load_message(&account_id, &message.header.id)
            .expect("load message")
            .expect("cached message");
        assert_eq!(restored.header.subject, "Sujet actualisé");
        assert_eq!(restored.tags, vec!["tag-a"]);
    }

    #[test]
    fn tag_rename_updates_every_cached_header_for_the_account() {
        let mut db = test_db();
        let mut message = message();
        message.header.tags = vec!["Étiquette A".into(), "Étiquette B".into()];
        message.tags = message.header.tags.clone();
        let account_id = message.header.account_id.clone();
        db.store_message(&account_id, &message)
            .expect("store message");

        db.rename_tag(&account_id, "Étiquette A", "Étiquette B")
            .expect("rename cached tag");

        let restored = db
            .load_message(&account_id, &message.header.id)
            .expect("load message")
            .expect("cached message");
        assert_eq!(restored.header.tags, vec!["Étiquette B"]);
        assert_eq!(restored.tags, vec!["Étiquette B"]);
    }

    #[test]
    fn lazy_attachment_survives_message_metadata_refresh() {
        let mut db = test_db();
        let mut message = message();
        message.attachments[0].bytes = None;
        let account_id = message.header.account_id.clone();
        db.store_message(&account_id, &message)
            .expect("store metadata");
        db.store_attachment(
            &account_id,
            &message.header.id,
            &message.attachments[0].id,
            &[7, 8, 9],
        )
        .expect("store lazy attachment");

        // A provider refresh carries metadata only and must retain an
        // attachment the user already downloaded.
        db.store_message(&account_id, &message)
            .expect("refresh metadata");
        let restored = db
            .load_message(&account_id, &message.header.id)
            .expect("load message")
            .expect("cached message");
        assert_eq!(restored.attachments[0].bytes, Some(vec![7, 8, 9]));
    }

    #[test]
    fn cache_limit_evicts_oldest_bodies_in_one_pass() {
        let mut db = test_db();
        let mut first = message();
        first.header.id = "message-old".into();
        first.body = "a".repeat(2048);
        let mut second = message();
        second.header.id = "message-new".into();
        second.body = "b".repeat(2048);
        let account_id = first.header.account_id.clone();
        db.store_message(&account_id, &first)
            .expect("store old body");
        db.store_message(&account_id, &second)
            .expect("store new body");
        db.conn
            .execute(
                "UPDATE messages SET last_access=CASE message_id
                   WHEN 'message-old' THEN 1 ELSE 2 END",
                [],
            )
            .expect("set LRU order");
        let total = db.stats().expect("cache stats").used_bytes;
        let reclaimable: u64 = db
            .conn
            .query_row(
                "SELECT cache_bytes-length(CAST(header_json AS BLOB))
                 FROM messages WHERE message_id='message-old'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("reclaimable bytes") as u64;
        db.limit_bytes = total.saturating_sub(reclaimable).saturating_add(1);
        db.enforce_limit().expect("enforce cache limit");

        let bodies: Vec<(String, bool)> = {
            let mut statement = db
                .conn
                .prepare(
                    "SELECT message_id,body_json IS NOT NULL
                     FROM messages ORDER BY message_id",
                )
                .expect("prepare body query");
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query bodies")
                .collect::<rusqlite::Result<_>>()
                .expect("collect bodies")
        };
        assert_eq!(
            bodies,
            vec![("message-new".into(), true), ("message-old".into(), false)]
        );
    }

    #[test]
    fn old_schema_is_discarded_instead_of_migrated() {
        let conn = Connection::open_in_memory().expect("in-memory cache");
        conn.execute_batch(
            "CREATE TABLE cache_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO cache_meta VALUES('schema_version', '1');
             CREATE TABLE messages (
               account_id TEXT NOT NULL,
               message_id TEXT NOT NULL,
               header_json TEXT NOT NULL,
               body_json TEXT,
               body_bytes INTEGER NOT NULL DEFAULT 0,
               last_access INTEGER NOT NULL,
               PRIMARY KEY(account_id,message_id)
             );
             INSERT INTO messages VALUES('account-a','message-a','{}',NULL,0,0);",
        )
        .expect("legacy schema");

        CacheDb::initialize_schema(&conn).expect("replace schema");

        let messages: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .expect("message count");
        let version: String = conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema version");
        assert_eq!(messages, 0);
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn recipient_usage_is_normalized_deduplicated_and_persisted() {
        let mut db = test_db();
        let first = db
            .record_recipient_usage(vec![
                "CONTACT-A@EXAMPLE.TEST".into(),
                "contact-a@example.test".into(),
                " contact-b@example.test ".into(),
            ])
            .expect("record recipient usage");
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|entry| entry.use_count == 1));

        let second = db
            .record_recipient_usage(vec!["contact-a@example.test".into()])
            .expect("increment recipient usage");
        assert_eq!(second[0].email, "contact-a@example.test");
        assert_eq!(second[0].use_count, 2);

        let loaded = db.load_recipient_usage().expect("load recipient usage");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].email, "contact-a@example.test");
        assert_eq!(loaded[0].use_count, 2);

        db.clear().expect("clear local cache");
        assert!(db
            .load_recipient_usage()
            .expect("load cleared recipient usage")
            .is_empty());
    }

    fn header(id: &str, subject: &str, from: &str, preview: &str) -> MessageHeader {
        MessageHeader {
            id: id.into(),
            account_id: AccountId("account-a".into()),
            subject: subject.into(),
            from: from.into(),
            received: chrono::DateTime::UNIX_EPOCH,
            preview: preview.into(),
            is_read: true,
            is_flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: None,
            internet_message_id: None,
        }
    }

    fn match_expression(query: &str) -> Option<String> {
        fts_match_expression(&SearchQuery::parse(query))
    }

    fn search_ids(db: &mut CacheDb, query: &str) -> Vec<String> {
        db.search(&SearchQuery::parse(query), None, &SearchScope::Account, 20)
            .expect("search")
            .into_iter()
            .map(|header| header.id)
            .collect()
    }

    /// Link targets would otherwise flood the index: a single tracking URL
    /// contributes dozens of tokens, and a customer's name inside a query
    /// string would match every newsletter carrying it.
    #[test]
    fn indexed_body_keeps_link_text_and_drops_urls() {
        let markdown = "# Facture\n\nVoir [le détail](https://tracker.example.test/r?u=alice) \
                        et ![logo](bytes://cid-logo) <span>fin</span>";
        let text = searchable_body_text(markdown);
        assert!(text.contains("Facture"), "{text}");
        assert!(text.contains("le détail"), "{text}");
        assert!(text.contains("fin"), "{text}");
        assert!(
            !text.contains("tracker"),
            "URL leaked into the index: {text}"
        );
        assert!(!text.contains("alice"), "URL leaked into the index: {text}");
        assert!(!text.contains("cid-logo"), "{text}");
        assert!(!text.contains('#') && !text.contains('<'), "{text}");
    }

    #[test]
    fn indexed_body_is_truncated_to_the_cap() {
        let huge = "mot ".repeat(MAX_INDEXED_BODY_BYTES);
        assert!(searchable_body_text(&huge).len() <= MAX_INDEXED_BODY_BYTES);
    }

    /// Raw input reaching `MATCH` would fail the whole statement — `facture:`
    /// reads as a filter on a column that does not exist.
    #[test]
    fn match_expression_quotes_terms_and_prefixes_the_last_one() {
        assert_eq!(match_expression("facture").as_deref(), Some("\"facture\"*"));
        assert_eq!(
            match_expression("facture urgent").as_deref(),
            Some("\"facture\" AND \"urgent\"*")
        );
        // Syntax characters must survive as literals, not as operators.
        assert_eq!(
            match_expression("truc: test").as_deref(),
            Some("\"truc:\" AND \"test\"*")
        );
        assert!(match_expression("   ").is_none());
        assert!(match_expression("-").is_none());
    }

    /// Field operators become FTS5 column filters rather than post-filters, so
    /// `objet:` only walks the subject postings.
    #[test]
    fn match_expression_maps_operators_onto_columns() {
        assert_eq!(
            match_expression("objet:contrat").as_deref(),
            Some("subject:\"contrat\"")
        );
        assert_eq!(
            match_expression("de:alice contrat").as_deref(),
            Some("sender:\"alice\" AND \"contrat\"*")
        );
        assert_eq!(
            match_expression("à:bob").as_deref(),
            Some("recipients:\"bob\"")
        );
        // A query made only of non-text filters cannot drive the index.
        assert!(match_expression("est:non-lu").is_none());
    }

    #[test]
    fn search_matches_subject_sender_and_body_without_accents() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut message = message();
        message.header = header(
            "message-a",
            "Réunion budgétaire",
            "Contact A <contact-a@example.test>",
            "",
        );
        message.body = "Le dossier est prêt pour la révision".into();
        db.store_message(&account_id, &message).expect("store");

        // Unaccented input must find accented text: nobody types accents into
        // a search field.
        assert_eq!(search_ids(&mut db, "reunion"), vec!["message-a"]);
        assert_eq!(search_ids(&mut db, "budgetaire"), vec!["message-a"]);
        assert_eq!(search_ids(&mut db, "revision"), vec!["message-a"]);
        // Sender, including the address split by the tokenizer.
        assert_eq!(
            search_ids(&mut db, "contact-a@example.test"),
            vec!["message-a"]
        );
        // Prefix search while typing.
        assert_eq!(search_ids(&mut db, "reun"), vec!["message-a"]);
        // Implicit AND across terms.
        assert_eq!(search_ids(&mut db, "reunion dossier"), vec!["message-a"]);
        assert!(search_ids(&mut db, "reunion absente").is_empty());
    }

    /// Most messages are listed but never opened; the folder listing is their
    /// only chance to become searchable.
    #[test]
    fn listed_messages_are_searchable_before_their_body_is_fetched() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        db.store_headers(
            &account_id,
            None,
            &[header(
                "message-b",
                "Convocation",
                "Contact B <contact-b@example.test>",
                "ordre du jour provisoire",
            )],
        )
        .expect("store headers");

        assert_eq!(search_ids(&mut db, "convocation"), vec!["message-b"]);
        // The preview stands in for the body that was never downloaded.
        assert_eq!(search_ids(&mut db, "provisoire"), vec!["message-b"]);
    }

    /// A subject hit must outrank the same word buried in a body, otherwise
    /// relevance ordering is no better than chronological.
    #[test]
    fn subject_matches_rank_above_body_matches() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut in_body = message();
        in_body.header = header("body-hit", "Sans rapport", "Contact A <a@example.test>", "");
        in_body.body = "le contrat est joint".into();
        db.store_message(&account_id, &in_body).expect("store");

        let mut in_subject = message();
        in_subject.header = header("subject-hit", "Contrat", "Contact B <b@example.test>", "");
        in_subject.body = "sans rapport".into();
        db.store_message(&account_id, &in_subject).expect("store");

        assert_eq!(
            search_ids(&mut db, "contrat"),
            vec!["subject-hit", "body-hit"]
        );
    }

    #[test]
    fn search_can_be_scoped_to_one_account() {
        let mut db = test_db();
        for account in ["account-a", "account-b"] {
            let account_id = AccountId(account.into());
            let mut message = message();
            message.header = header(
                &format!("message-{account}"),
                "Convocation",
                "Contact <contact@example.test>",
                "",
            );
            message.header.account_id = account_id.clone();
            db.store_message(&account_id, &message).expect("store");
        }

        let all = SearchQuery::parse("convocation");
        assert_eq!(
            db.search(&all, None, &SearchScope::Account, 20)
                .expect("all")
                .len(),
            2
        );
        let scoped = db
            .search(
                &all,
                Some(&AccountId("account-b".into())),
                &SearchScope::Account,
                20,
            )
            .expect("scoped");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "message-account-b");
        // The account a hit belongs to must survive, so results stay openable
        // in unified mode.
        assert_eq!(scoped[0].account_id, AccountId("account-b".into()));
    }

    /// Scoping to a folder must not drop the account-wide behaviour, and a
    /// message living in several folders (Gmail labels) must come back once,
    /// not once per membership.
    #[test]
    fn search_can_be_scoped_to_one_folder() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut message = message();
        message.header = header("message-a", "Convocation", "Contact <c@example.test>", "");
        db.store_message(&account_id, &message).expect("store");
        // Listed in the inbox and under a user folder at once.
        db.store_headers(&account_id, None, &[message.header.clone()])
            .expect("inbox listing");
        db.store_headers(&account_id, Some("folder-work"), &[message.header.clone()])
            .expect("folder listing");

        let query = SearchQuery::parse("convocation");
        let ids = |db: &mut CacheDb, scope: SearchScope| -> Vec<String> {
            db.search(&query, None, &scope, 20)
                .expect("search")
                .into_iter()
                .map(|header| header.id)
                .collect()
        };

        assert_eq!(ids(&mut db, SearchScope::Account), vec!["message-a"]);
        assert_eq!(
            ids(&mut db, SearchScope::Folder(Some("folder-work".into()))),
            vec!["message-a"]
        );
        // `None` is the inbox, not "every folder".
        assert_eq!(ids(&mut db, SearchScope::Folder(None)), vec!["message-a"]);
        assert!(ids(&mut db, SearchScope::Folder(Some("folder-other".into()))).is_empty());
    }

    /// Messages are deleted from half a dozen call sites; an index row
    /// outliving its message would offer results that cannot be opened.
    #[test]
    fn deleting_a_message_drops_it_from_the_index() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut message = message();
        message.header = header("message-a", "Convocation", "Contact <c@example.test>", "");
        db.store_message(&account_id, &message).expect("store");
        assert_eq!(search_ids(&mut db, "convocation").len(), 1);

        db.remove_message(&account_id, "message-a").expect("remove");
        assert!(search_ids(&mut db, "convocation").is_empty());

        // Same through the account purge, which deletes in bulk.
        db.store_message(&account_id, &message).expect("re-store");
        assert_eq!(search_ids(&mut db, "convocation").len(), 1);
        db.purge_account(&account_id).expect("purge");
        assert!(search_ids(&mut db, "convocation").is_empty());
    }

    /// The whole point of counting here rather than in the message list: the
    /// cache holds messages the loaded page does not, and it must count every
    /// one of them — while leaving out threads of a single message, which the
    /// list renders as ordinary rows.
    #[test]
    fn conversation_totals_count_the_folder_not_the_page() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let threaded = |id: &str, conversation: &str| {
            let mut header = header(id, "Contrat", "Contact A <contact-a@example.test>", "");
            header.conversation_id = Some(conversation.into());
            header
        };

        db.store_headers(
            &account_id,
            None,
            &[
                threaded("message-a", "conversation-1"),
                threaded("message-b", "conversation-1"),
                threaded("message-c", "conversation-1"),
                threaded("message-d", "conversation-2"),
                header(
                    "message-e",
                    "Sans fil",
                    "Contact B <contact-b@example.test>",
                    "",
                ),
            ],
        )
        .expect("store headers");

        let totals = db
            .conversation_totals(&account_id, None)
            .expect("conversation totals");
        assert_eq!(totals.get("conversation-1"), Some(&3));
        assert_eq!(
            totals.get("conversation-2"),
            None,
            "a lone message is not a group"
        );
        assert_eq!(totals.len(), 1);
    }

    /// Counts are per folder: a thread spread across the inbox and an archive
    /// must not report the archived messages while the inbox is on screen.
    #[test]
    fn conversation_totals_stay_within_one_folder() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let threaded = |id: &str| {
            let mut header = header(id, "Contrat", "Contact A <contact-a@example.test>", "");
            header.conversation_id = Some("conversation-1".into());
            header
        };

        db.store_headers(
            &account_id,
            None,
            &[threaded("message-a"), threaded("message-b")],
        )
        .expect("store inbox headers");
        db.store_headers(
            &account_id,
            Some("folder-archive"),
            &[threaded("message-c")],
        )
        .expect("store archived header");

        assert_eq!(
            db.conversation_totals(&account_id, None)
                .expect("inbox totals")
                .get("conversation-1"),
            Some(&2)
        );
        // One message alone in the archive: not a group there.
        assert!(db
            .conversation_totals(&account_id, Some("folder-archive"))
            .expect("archive totals")
            .is_empty());
    }

    /// Eviction frees bodies but keeps headers. Search has to follow: still
    /// findable by subject, no longer by body text the cache cannot show.
    #[test]
    fn evicted_bodies_leave_the_index_but_headers_stay_searchable() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut message = message();
        message.header = header("message-a", "Convocation", "Contact <c@example.test>", "");
        message.body = "clause de confidentialite".into();
        db.store_message(&account_id, &message).expect("store");
        assert_eq!(search_ids(&mut db, "confidentialite").len(), 1);

        db.limit_bytes = 0;
        db.enforce_limit().expect("evict");

        assert_eq!(search_ids(&mut db, "convocation").len(), 1);
        assert!(
            search_ids(&mut db, "confidentialite").is_empty(),
            "evicted body text must leave the index"
        );
    }

    /// Providers reassign ids on move (Graph always, IMAP sometimes). The
    /// message must remain findable, and only once.
    #[test]
    fn moving_a_message_reindexes_it_under_its_new_id() {
        let mut db = test_db();
        let account_id = AccountId("account-a".into());
        let mut message = message();
        message.header = header("old-id", "Convocation", "Contact <c@example.test>", "");
        db.store_message(&account_id, &message).expect("store");
        db.store_headers(&account_id, None, &[message.header.clone()])
            .expect("list");

        db.move_message(&account_id, "old-id", None, "target", Some("new-id"))
            .expect("move");

        assert_eq!(search_ids(&mut db, "convocation"), vec!["new-id"]);
    }
}
