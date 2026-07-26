use crate::auth::{self, ImapConfig, Tokens};
use crate::model::{Account, AccountId, Provider};
use crate::providers::{ImapAuth, Session};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::task::JoinHandle;

mod ai;
mod auth_flow;
mod calendar;
mod contacts;
mod folders;
mod ical;
mod languagetool;
mod mail_cache;
mod mailbox;
mod operation_store;
mod operations;
mod protocol;
mod send;
mod sender_history;
mod tags;

pub use protocol::{
    AiEditRequest, Cmd, EventDraft, Evt, MessageMutationKind, OutgoingMail, QuickActionExecution,
    QuickActionStep, RecipientUsage, SearchScope, UnifiedAccountPage,
};

/// Handle for the background Tokio runtime, published for tasks outside that
/// runtime which must issue an occasional network request (remote images in
/// Blitz rendering; see `ui/blitz_body.rs`). Empty until `spawn` has started.
pub static TOKIO_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

pub fn spawn(
    mail_cache_limit_mb: u64,
    languagetool_settings: crate::proofreading::LanguageToolSettings,
) -> (mpsc::UnboundedSender<Cmd>, mpsc::UnboundedReceiver<Evt>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let _ = TOKIO_HANDLE.set(rt.handle().clone());
        rt.block_on(run(
            cmd_rx,
            evt_tx,
            mail_cache_limit_mb,
            languagetool_settings,
        ));
    });
    (cmd_tx, evt_rx)
}

pub(super) struct BgGlobal {
    pub http: reqwest::Client,
    pub evt_tx: mpsc::UnboundedSender<Evt>,
    pub accounts: RwLock<HashMap<AccountId, Arc<BgAccount>>>,
    pub cache: mail_cache::MailCache,
    pub operations: operation_store::OperationStore,
    languagetool: Arc<languagetool::LanguageToolManager>,
    ical: Arc<ical::IcalManager>,
    unified_mailbox: Mutex<Option<mailbox::UnifiedPagination>>,
    unified_request_id: AtomicU64,
    /// Only one open request feeds the main reader. Replacing this handle also
    /// cancels a provider request that is already running or waiting on the
    /// mailbox semaphore.
    message_open_task: Mutex<Option<JoinHandle<()>>>,
    /// Conversation load associated with the same main reader.
    message_thread_task: Mutex<Option<JoinHandle<()>>>,
}

impl BgGlobal {
    pub(super) fn emit(&self, e: Evt) {
        let _ = self.evt_tx.send(e);
    }

    pub(super) async fn account(&self, id: &AccountId) -> Option<Arc<BgAccount>> {
        self.accounts.read().await.get(id).cloned()
    }

    pub(super) async fn record_recipient_usage(&self, emails: Vec<String>) {
        match self.cache.record_recipient_usage(emails).await {
            Ok(entries) if !entries.is_empty() => self.emit(Evt::RecipientUsage { entries }),
            Ok(_) => {}
            Err(error) => log::warn!("recipient usage cache write failed: {error:#}"),
        }
    }

    /// Spawns `task` for the targeted account if connected; commands for an
    /// account that has disappeared are silently ignored.
    async fn spawn_on<Fut>(&self, id: &AccountId, task: impl FnOnce(Arc<BgAccount>) -> Fut)
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(acc) = self.account(id).await {
            tokio::spawn(task(acc));
        }
    }

    async fn cancel_open_message(&self) {
        if let Some(task) = self.message_thread_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.message_open_task.lock().await.take() {
            task.abort();
        }
    }

    async fn replace_open_message(&self, account_id: &AccountId, id: String) {
        let account = self.account(account_id).await;
        if let Some(task) = self.message_thread_task.lock().await.take() {
            task.abort();
        }
        let mut current = self.message_open_task.lock().await;
        if let Some(task) = current.take() {
            task.abort();
        }
        if let Some(account) = account {
            *current = Some(tokio::spawn(mailbox::open_message(account, id)));
        }
    }

    async fn replace_message_thread(&self, account_id: &AccountId, conversation_id: String) {
        let account = self.account(account_id).await;
        let mut current = self.message_thread_task.lock().await;
        if let Some(task) = current.take() {
            task.abort();
        }
        if let Some(account) = account {
            *current = Some(tokio::spawn(mailbox::load_thread(account, conversation_id)));
        }
    }
}

pub(super) struct BgAccount {
    pub id: AccountId,
    pub provider: Provider,
    pub tenant: String,
    pub client_id: String,
    /// Google-only — empty for Microsoft. Required by Google's "Desktop app"
    /// token endpoint even though it's bundled in the binary.
    pub client_secret: String,
    pub info: RwLock<Account>,
    pub tokens: RwLock<Tokens>,
    /// Set before logout removes persisted credentials. In-flight tasks keep
    /// an `Arc<BgAccount>`, so they must not refresh and recreate the token
    /// file after the account disappeared from `BgGlobal::accounts`.
    pub logged_out: AtomicBool,
    /// IMAP keyring access is synchronous and may involve D-Bus. Load it once
    /// while restoring the account, then reuse it for provider calls.
    pub imap_password: RwLock<Option<String>>,
    pub seen_ids: Mutex<HashSet<String>>,
    pub auto_refresh: Mutex<Option<JoinHandle<()>>>,
    pub mail_sync: Mutex<()>,
    /// Serializes replay so a reconnect and a newly submitted command cannot
    /// execute the same durable operation concurrently.
    pub operation_drain: Mutex<()>,
    /// Wakes the outbox at the backoff deadline computed by `operations`.
    /// Without it, a deferred retry would only run the next time something else
    /// happened to drain the account.
    pub operation_retry: Mutex<Option<JoinHandle<()>>>,
    pub global: Arc<BgGlobal>,
    /// Caps actual in-flight Microsoft Graph requests for this account.
    /// Provider operations may issue several requests internally, so this
    /// transport-level gate is the authoritative MailboxConcurrency guard.
    pub graph_request_gate: Arc<Semaphore>,
    /// Coarse per-account operation scheduler shared by every provider.
    /// This prevents large UI fan-outs (for example kanban columns) from
    /// starting an unbounded number of mailbox operations. For Graph, the
    /// transport-level `graph_request_gate` separately limits the HTTP
    /// requests hidden inside those operations.
    pub mailbox_gate: Arc<Semaphore>,
    /// Read receipts are deliberately outside the message-open critical path.
    /// Serialize them separately so rapid navigation cannot occupy every
    /// mailbox permit with low-priority provider updates.
    pub deferred_read_gate: Arc<Semaphore>,
}

/// Owned credentials for one provider call. Constructed each time by
/// `BgAccount::ensure_auth` so we don't have to hold the tokens lock across
/// `.await`. Borrowed by `BgAccount::session` for dispatch.
pub(super) enum AuthOwned {
    Bearer(String),
    Imap {
        config: ImapConfig,
        password: String,
    },
}

impl BgAccount {
    pub(super) fn emit(&self, e: Evt) {
        self.global.emit(e);
    }

    /// Provider session ready for one call. The provider/credentials pairing
    /// is guaranteed because `auth` always comes from `ensure_auth` on this
    /// same account.
    pub(super) fn session<'a>(&'a self, auth: &'a AuthOwned) -> Session<'a> {
        match (self.provider, auth) {
            (Provider::Microsoft, AuthOwned::Bearer(token)) => Session::Graph {
                client: crate::providers::graph::Client::new(
                    &self.global.http,
                    token,
                    &self.graph_request_gate,
                ),
                tenant: &self.tenant,
            },
            (Provider::Google, AuthOwned::Bearer(token)) => Session::Gmail {
                client: &self.global.http,
                token,
            },
            (Provider::Imap, AuthOwned::Imap { config, password }) => {
                Session::Imap(ImapAuth::from_config(config, password))
            }
            _ => unreachable!("ensure_auth guarantees the provider/credentials pairing"),
        }
    }

    /// Runs `ensure_auth` with standard runtime-task error handling: emits
    /// `Evt::Error` and returns `None` on failure.
    pub(super) async fn auth_or_report(&self) -> Option<AuthOwned> {
        match self.ensure_auth().await {
            Ok(a) => Some(a),
            Err(e) => {
                self.emit(Evt::Error(e.to_string()));
                None
            }
        }
    }

    /// Reserve one of the provider calls that count against a mailbox's
    /// concurrency budget. Graph applies this limit across messages,
    /// folders and Outlook categories, so every runtime mailbox operation
    /// must use the same per-account gate.
    pub(super) async fn mailbox_permit(&self) -> OwnedSemaphorePermit {
        self.mailbox_gate
            .clone()
            .acquire_owned()
            .await
            .expect("mailbox semaphore is never closed")
    }

    /// Returns up-to-date credentials for one provider call. For OAuth
    /// providers this transparently refreshes the access token if it's
    /// within 30 seconds of expiry; for IMAP it pairs the cached keyring
    /// password with the persisted `ImapConfig`.
    pub(super) async fn ensure_auth(&self) -> Result<AuthOwned> {
        if self.logged_out.load(Ordering::Acquire) {
            anyhow::bail!(tr!("runtime-error-account-logged-out"));
        }
        if matches!(self.provider, Provider::Imap) {
            let config = {
                let guard = self.tokens.read().await;
                guard
                    .imap_config
                    .clone()
                    .context(tr!("runtime-error-imap-config-missing"))?
            };
            let password = self
                .imap_password
                .read()
                .await
                .clone()
                .context(tr!("runtime-error-imap-password-missing"))?;
            return Ok(AuthOwned::Imap { config, password });
        }

        let mut guard = self.tokens.write().await;
        if guard.expires_at <= Utc::now() + Duration::seconds(30) {
            let rt = guard
                .refresh_token
                .clone()
                .context(tr!("auth-error-refresh-token-missing"))?;
            let new = match self.provider {
                Provider::Microsoft => {
                    auth::microsoft::refresh(&self.global.http, &self.client_id, &self.tenant, &rt)
                        .await?
                }
                Provider::Google => {
                    let mut t = auth::google::refresh(
                        &self.global.http,
                        &self.client_id,
                        &self.client_secret,
                        &rt,
                    )
                    .await?;
                    // Google often omits the refresh_token in the response — keep the
                    // existing one so we can refresh again.
                    if t.refresh_token.is_none() {
                        t.refresh_token = Some(rt);
                    }
                    t
                }
                Provider::Imap => unreachable!("IMAP path handled above"),
            };
            if self.logged_out.load(Ordering::Acquire) {
                anyhow::bail!(tr!("runtime-error-account-logged-out"));
            }
            auth::save_tokens(&self.id, &new)?;
            *guard = new;
        }
        Ok(AuthOwned::Bearer(guard.access_token.clone()))
    }
}

async fn run(
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    evt_tx: mpsc::UnboundedSender<Evt>,
    mail_cache_limit_mb: u64,
    languagetool_settings: crate::proofreading::LanguageToolSettings,
) {
    let http = build_http_client();
    let languagetool = languagetool::LanguageToolManager::new(
        http.clone(),
        evt_tx.clone(),
        languagetool_settings.clone(),
    );
    let global = Arc::new(BgGlobal {
        http: http.clone(),
        evt_tx: evt_tx.clone(),
        accounts: RwLock::new(HashMap::new()),
        cache: mail_cache::MailCache::start(mail_cache_limit_mb),
        operations: operation_store::OperationStore::start(),
        languagetool: languagetool.clone(),
        ical: ical::IcalManager::new(http, evt_tx),
        unified_mailbox: Mutex::new(None),
        unified_request_id: AtomicU64::new(0),
        message_open_task: Mutex::new(None),
        message_thread_task: Mutex::new(None),
    });
    {
        let global = global.clone();
        tokio::spawn(async move {
            match global.cache.load_recipient_usage().await {
                Ok(entries) => global.emit(Evt::RecipientUsage { entries }),
                Err(error) => log::warn!("recipient usage cache read failed: {error:#}"),
            }
        });
    }
    tokio::spawn(async move {
        languagetool.configure(languagetool_settings).await;
    });

    let mut persisted = auth::list_persisted_accounts();
    // Resumes authentication interrupted before `/me` could associate the
    // tokens with their final account.
    if let Some(pending) = auth::load_pending_tokens() {
        persisted.push((crate::model::AccountId(String::new()), pending));
    }
    if !persisted.is_empty() {
        global.emit(Evt::Authenticated);
        for (stored_id, tokens) in persisted {
            let g = global.clone();
            tokio::spawn(async move {
                let stored_id = (!stored_id.0.is_empty()).then_some(stored_id);
                auth_flow::resume_session(g, tokens, stored_id).await;
            });
        }
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::ConfigureLanguageTool(config) => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move { manager.configure(config).await });
            }
            Cmd::InstallLanguageTool => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move { manager.install().await });
            }
            Cmd::UninstallLanguageTool => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move { manager.uninstall().await });
            }
            Cmd::TestLanguageTool(config) => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move { manager.test_config(config).await });
            }
            Cmd::ResetLanguageTool => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move { manager.reset().await });
            }
            Cmd::CheckLanguageTool {
                editor_id,
                block_id,
                revision,
                text,
                ui_language,
            } => {
                let manager = global.languagetool.clone();
                tokio::spawn(async move {
                    manager
                        .check(editor_id, block_id, revision, text, ui_language)
                        .await;
                });
            }
            Cmd::EditMailWithAi(request) => {
                tokio::spawn(ai::edit_mail(global.clone(), request));
            }
            Cmd::StartLogin { client_id, tenant } => {
                tokio::spawn(auth_flow::login_flow(global.clone(), client_id, tenant));
            }
            Cmd::StartGoogleLogin {
                client_id,
                client_secret,
            } => {
                tokio::spawn(auth_flow::google_login_flow(
                    global.clone(),
                    client_id,
                    client_secret,
                ));
            }
            Cmd::StartImapLogin { config, password } => {
                tokio::spawn(auth_flow::imap_login_flow(global.clone(), config, password));
            }
            Cmd::Logout(account_id) => {
                tokio::spawn(auth_flow::logout(global.clone(), account_id));
            }
            Cmd::SetMailCacheLimit { limit_mb } => {
                tokio::spawn(mail_cache::apply_limit(global.clone(), limit_mb));
            }
            Cmd::ClearMailCache => {
                tokio::spawn(mail_cache::clear_and_report(global.clone()));
            }
            Cmd::GetMailCacheStats => {
                tokio::spawn(mail_cache::report_stats(global.clone()));
            }
            Cmd::Refresh {
                account_id,
                folder_id,
                limit,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        mailbox::refresh_inbox(acc, folder_id, limit)
                    })
                    .await;
            }
            Cmd::RefreshUnified {
                request_id,
                accounts,
                page_size,
            } => {
                global
                    .unified_request_id
                    .store(request_id, Ordering::Release);
                *global.unified_mailbox.lock().await = None;
                tokio::spawn(mailbox::refresh_unified(
                    global.clone(),
                    request_id,
                    accounts,
                    page_size,
                ));
            }
            Cmd::LoadMore {
                account_id,
                folder_id,
                skip,
                limit,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        mailbox::load_more(acc, folder_id, skip, limit)
                    })
                    .await;
            }
            Cmd::LoadMoreUnified { request_id } => {
                tokio::spawn(mailbox::load_more_unified(global.clone(), request_id));
            }
            Cmd::OpenMessage { account_id, id } => {
                global.replace_open_message(&account_id, id).await;
            }
            Cmd::CancelOpenMessage => {
                global.cancel_open_message().await;
            }
            Cmd::LoadCachedMessage { account_id, id } => {
                tokio::spawn(mailbox::load_cached_message(global.clone(), account_id, id));
            }
            Cmd::LoadQuickActionMessage {
                request_id,
                account_id,
                id,
            } => {
                global
                    .spawn_on(&account_id, |account| {
                        mailbox::load_quick_action_message(account, request_id, id)
                    })
                    .await;
            }
            Cmd::FetchAttachment {
                account_id,
                message_id,
                attachment_id,
            } => {
                global
                    .spawn_on(&account_id, |account| {
                        mailbox::fetch_attachment(account, message_id, attachment_id)
                    })
                    .await;
            }
            Cmd::LoadThreadMessage { account_id, id } => {
                global
                    .spawn_on(&account_id, |acc| mailbox::load_thread_message(acc, id))
                    .await;
            }
            Cmd::DeleteMessage { account_id, id } => {
                tokio::spawn(operations::submit(
                    global.clone(),
                    account_id,
                    operation_store::OperationKind::Delete { id },
                ));
            }
            Cmd::LoadThread {
                account_id,
                conversation_id,
            } => {
                global
                    .replace_message_thread(&account_id, conversation_id)
                    .await;
            }
            Cmd::Search {
                account_id,
                query,
                scope,
                limit,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        mailbox::search_messages(acc, query, scope, limit)
                    })
                    .await;
            }
            Cmd::SetFlag {
                account_id,
                id,
                flagged,
            } => {
                tokio::spawn(operations::submit(
                    global.clone(),
                    account_id,
                    operation_store::OperationKind::SetFlag { id, flagged },
                ));
            }
            Cmd::MarkRead {
                account_id,
                id,
                read,
            } => {
                tokio::spawn(operations::submit(
                    global.clone(),
                    account_id,
                    operation_store::OperationKind::MarkRead { id, read },
                ));
            }
            // No spawn: schedules or cancels the account's periodic task.
            Cmd::SetAutoRefresh {
                account_id,
                folder_id,
                secs,
                limit,
            } => {
                if let Some(acc) = global.account(&account_id).await {
                    mailbox::set_auto_refresh(acc, folder_id, secs, limit).await;
                }
            }
            Cmd::LoadFolders { account_id } => {
                global.spawn_on(&account_id, folders::load).await;
            }
            Cmd::CreateFolder {
                account_id,
                name,
                parent_id,
            } => {
                global
                    .spawn_on(&account_id, |acc| folders::create(acc, name, parent_id))
                    .await;
            }
            Cmd::RenameFolder {
                account_id,
                id,
                new_name,
            } => {
                global
                    .spawn_on(&account_id, |acc| folders::rename(acc, id, new_name))
                    .await;
            }
            Cmd::DeleteFolder { account_id, id } => {
                global
                    .spawn_on(&account_id, |acc| folders::delete(acc, id))
                    .await;
            }
            Cmd::MoveMessage {
                account_id,
                message_id,
                source_folder_id,
                target_folder_id,
            } => {
                tokio::spawn(operations::submit(
                    global.clone(),
                    account_id,
                    operation_store::OperationKind::Move {
                        message_id,
                        source_folder_id,
                        target_folder_id,
                    },
                ));
            }
            Cmd::ScheduleQuickAction {
                account_id,
                execution,
                delay_secs,
            } => {
                operations::schedule_quick_action(
                    global.clone(),
                    account_id,
                    execution,
                    delay_secs,
                )
                .await;
            }
            Cmd::CancelQuickAction {
                account_id,
                execution_id,
            } => {
                operations::cancel_quick_action(global.clone(), account_id, execution_id).await;
            }
            Cmd::LoadTags { account_id } => {
                global.spawn_on(&account_id, tags::load).await;
            }
            Cmd::CreateTag {
                account_id,
                name,
                color,
            } => {
                global
                    .spawn_on(&account_id, |acc| tags::create(acc, name, color))
                    .await;
            }
            Cmd::RenameTag {
                account_id,
                id,
                new_name,
            } => {
                global
                    .spawn_on(&account_id, |acc| tags::rename(acc, id, new_name))
                    .await;
            }
            Cmd::SetTagColor {
                account_id,
                id,
                color,
            } => {
                global
                    .spawn_on(&account_id, |acc| tags::set_color(acc, id, color))
                    .await;
            }
            Cmd::DeleteTag { account_id, id } => {
                global
                    .spawn_on(&account_id, |acc| tags::delete(acc, id))
                    .await;
            }
            Cmd::AddTag {
                account_id,
                message_id,
                tag_id,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        tags::add_to_message(acc, message_id, tag_id)
                    })
                    .await;
            }
            Cmd::RemoveTag {
                account_id,
                message_id,
                tag_id,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        tags::remove_from_message(acc, message_id, tag_id)
                    })
                    .await;
            }
            Cmd::LoadTagListing {
                account_id,
                tag_id,
                limit,
            } => {
                global
                    .spawn_on(&account_id, |acc| tags::load_listing(acc, tag_id, limit))
                    .await;
            }
            Cmd::LoadCalendar {
                account_id,
                from,
                to,
            } => {
                global
                    .spawn_on(&account_id, |acc| calendar::load_calendar(acc, from, to))
                    .await;
            }
            Cmd::RespondToInvitation {
                account_id,
                message_id,
                event_id,
                response,
            } => {
                global
                    .spawn_on(&account_id, |account| {
                        calendar::respond_to_invitation(account, message_id, event_id, response)
                    })
                    .await;
            }
            Cmd::ConfigureIcalSubscriptions(subscriptions) => {
                global.ical.clone().configure(subscriptions).await;
            }
            Cmd::LoadIcalCalendar {
                subscription_id,
                from,
                to,
                force_refresh,
            } => {
                let manager = global.ical.clone();
                tokio::spawn(async move {
                    manager
                        .load_range(&subscription_id, from, to, force_refresh)
                        .await;
                });
            }
            Cmd::RefreshIcalSubscription { subscription_id } => {
                let manager = global.ical.clone();
                tokio::spawn(async move {
                    manager.refresh(&subscription_id, true).await;
                });
            }
            Cmd::DeleteIcalSubscriptionCache { subscription_id } => {
                let manager = global.ical.clone();
                tokio::spawn(async move {
                    manager.delete_cache(&subscription_id).await;
                });
            }
            Cmd::CreateEvent {
                request_id,
                account_id,
                event,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        calendar::create_event(acc, request_id, event)
                    })
                    .await;
            }
            Cmd::UpdateCalendarEvent {
                request_id,
                account_id,
                event_id,
                event,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        calendar::update_event(acc, request_id, event_id, event)
                    })
                    .await;
            }
            Cmd::DeleteCalendarEvent {
                account_id,
                event_id,
            } => {
                global
                    .spawn_on(&account_id, |acc| calendar::delete_event(acc, event_id))
                    .await;
            }
            Cmd::MoveCalendarEvent {
                account_id,
                event_id,
                start,
                end,
                previous_start,
                previous_end,
                all_day,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        calendar::move_event(
                            acc,
                            event_id,
                            start,
                            end,
                            previous_start,
                            previous_end,
                            all_day,
                        )
                    })
                    .await;
            }
            Cmd::LoadContacts { account_id } => {
                global.spawn_on(&account_id, contacts::load_contacts).await;
            }
            Cmd::LoadSenderHistory {
                account_id,
                email,
                limit,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        sender_history::load_sender_history(acc, email, limit)
                    })
                    .await;
            }
            Cmd::LoadMoreSenderHistory {
                account_id,
                email,
                next_link,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        sender_history::load_more_sender_history(acc, email, next_link)
                    })
                    .await;
            }
            Cmd::SendMail {
                account_id,
                compose_id,
                reply_to,
                reply_all,
                forward_of,
                draft_id,
                mail,
            } => {
                tokio::spawn(operations::submit(
                    global.clone(),
                    account_id,
                    operation_store::OperationKind::Send {
                        compose_id,
                        reply_to,
                        reply_all,
                        forward_of,
                        draft_id,
                        mail,
                    },
                ));
            }
            Cmd::FetchSentCopy {
                account_id,
                related_to,
                snapshot_id,
                sent_id,
                internet_message_id,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        send::fetch_sent_copy(
                            acc,
                            related_to,
                            snapshot_id,
                            sent_id,
                            internet_message_id,
                        )
                    })
                    .await;
            }
            Cmd::SaveDraft {
                account_id,
                compose_id,
                replace_id,
                mail,
                autosave,
            } => {
                global
                    .spawn_on(&account_id, |acc| {
                        send::save_draft(acc, compose_id, replace_id, mail, autosave)
                    })
                    .await;
            }
            Cmd::FetchInlineImage {
                editor_id,
                cid,
                url,
            } => {
                tokio::spawn(fetch_inline_image_for_editor(
                    global.clone(),
                    editor_id,
                    cid,
                    url,
                ));
            }
        }
    }
    global.languagetool.stop().await;
}

/// Shared client for every provider call.
///
/// Neither timeout is optional in practice: a stalled connection would hold one
/// of the account's `mailbox_gate` permits, and — for a send — the
/// `operation_drain` lock, freezing the whole outbox with no error surfaced.
/// `read_timeout` bounds the silence *between* chunks rather than the total
/// transfer, so a slow but progressing 30 MB attachment still completes.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|error| {
            log::warn!("falling back to an untimed HTTP client: {error:#}");
            reqwest::Client::new()
        })
}

/// Fetches one pasted image and reports it back to the editor that asked.
async fn fetch_inline_image_for_editor(
    global: Arc<BgGlobal>,
    editor_id: String,
    cid: String,
    url: String,
) {
    match fetch_inline_image(&global.http, &url).await {
        Ok((bytes, mime)) => global.emit(Evt::InlineImageFetched {
            editor_id,
            cid,
            bytes,
            mime,
        }),
        Err(e) => global.emit(Evt::InlineImageFetchError {
            editor_id,
            cid,
            error: format!("{e:#}"),
        }),
    }
}

/// Fetch the bytes of an external image referenced by a pasted `<img src=...>`
/// tag. Mime is read from `Content-Type`, falling back to `image/png`
/// (recipients tolerate the wrong subtype better than no type).
///
/// The size cap is enforced *while* streaming: buffering the whole response
/// first would let a hostile source exhaust memory before the check ever ran.
async fn fetch_inline_image(http: &reqwest::Client, url: &str) -> Result<(Vec<u8>, String)> {
    use futures::StreamExt as _;

    const MAX_BYTES: usize = 16 * 1024 * 1024;
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase())
        .filter(|s| s.starts_with("image/"))
        .unwrap_or_else(|| "image/png".to_string());

    // An advertised length above the cap fails before a single chunk is read.
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        anyhow::bail!(tr!("runtime-error-inline-image-too-large", {
            max: MAX_BYTES / (1024 * 1024)
        }));
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > MAX_BYTES {
            anyhow::bail!(tr!("runtime-error-inline-image-too-large", {
                max: MAX_BYTES / (1024 * 1024)
            }));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, mime))
}
