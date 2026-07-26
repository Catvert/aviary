use super::{BgAccount, BgGlobal, Evt, UnifiedAccountPage};
use crate::model::{AccountId, MessageHeader, Provider};
use crate::providers::MessagePage;
use anyhow::{Context, Result};
use futures::future::join_all;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;

struct UnifiedStream {
    account_id: AccountId,
    chunk_size: usize,
    buffer: VecDeque<MessageHeader>,
    next: Option<String>,
    exhausted: bool,
}

pub(super) struct UnifiedPagination {
    request_id: u64,
    page_size: usize,
    streams: Vec<UnifiedStream>,
    emitted: HashSet<(AccountId, String)>,
}

impl UnifiedPagination {
    fn has_more(&self) -> bool {
        self.streams
            .iter()
            .any(|stream| !stream.buffer.is_empty() || !stream.exhausted)
    }

    async fn fill_empty_streams(&mut self, global: &Arc<BgGlobal>) {
        let pending: Vec<_> = self
            .streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| stream.buffer.is_empty() && !stream.exhausted)
            .map(|(index, stream)| {
                (
                    index,
                    stream.account_id.clone(),
                    stream.chunk_size,
                    stream.next.clone(),
                )
            })
            .collect();
        if pending.is_empty() {
            return;
        }

        let pages = join_all(
            pending
                .into_iter()
                .map(|(index, account_id, chunk_size, cursor)| {
                    let global = global.clone();
                    async move {
                        let page =
                            fetch_unified_page(global, account_id.clone(), chunk_size, cursor)
                                .await;
                        (index, account_id, page)
                    }
                }),
        )
        .await;

        for (index, account_id, result) in pages {
            let stream = &mut self.streams[index];
            match result {
                Ok(page) => {
                    stream.next = page.next;
                    stream.buffer = page.messages.into();
                    stream.exhausted = stream.buffer.is_empty() || stream.next.is_none();
                }
                Err(error) => {
                    log::warn!("unified pagination for account {account_id}: {error:#}");
                    stream.next = None;
                    stream.exhausted = true;
                }
            }
        }
    }

    async fn take_page(&mut self, global: &Arc<BgGlobal>) -> Vec<MessageHeader> {
        let mut page = Vec::with_capacity(self.page_size);
        while page.len() < self.page_size {
            self.fill_empty_streams(global).await;
            let next_stream = self
                .streams
                .iter()
                .enumerate()
                .filter_map(|(index, stream)| stream.buffer.front().map(|message| (index, message)))
                .max_by(|(left_index, left), (right_index, right)| {
                    left.received.cmp(&right.received).then_with(|| {
                        self.streams[*right_index]
                            .account_id
                            .0
                            .cmp(&self.streams[*left_index].account_id.0)
                    })
                })
                .map(|(index, _)| index);
            let Some(index) = next_stream else { break };
            let message = self.streams[index]
                .buffer
                .pop_front()
                .expect("the selected unified stream has a head");
            let key = (message.account_id.clone(), message.id.clone());
            if self.emitted.insert(key) {
                page.push(message);
            }
        }
        page
    }
}

async fn fetch_unified_page(
    global: Arc<BgGlobal>,
    account_id: AccountId,
    page_size: usize,
    cursor: Option<String>,
) -> Result<MessagePage> {
    let account = global
        .account(&account_id)
        .await
        .context("unified mailbox account disappeared")?;
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(error) => {
            account.emit(Evt::SyncStateChanged {
                account_id,
                online: false,
                error: Some(error.to_string()),
            });
            return Err(error);
        }
    };
    let permit = account.mailbox_permit().await;
    let result = if let Some(cursor) = cursor {
        account
            .session(&auth)
            .fetch_messages_page(&cursor)
            .await
            .map(|(messages, next)| MessagePage { messages, next })
    } else {
        account
            .session(&auth)
            .list_folder_messages_page(None, page_size)
            .await
    };
    drop(permit);

    let mut page = match result {
        Ok(page) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account_id.clone(),
                online: true,
                error: None,
            });
            page
        }
        Err(error) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account_id.clone(),
                online: false,
                error: Some(error.to_string()),
            });
            return Err(error);
        }
    };
    for message in &mut page.messages {
        message.account_id = account_id.clone();
    }
    page.messages.sort_by(|left, right| {
        right
            .received
            .cmp(&left.received)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut seen = account.seen_ids.lock().await;
    for message in &page.messages {
        seen.insert(message.id.clone());
    }
    drop(seen);
    account
        .global
        .cache
        .store_headers(account_id, None, page.messages.clone());
    emit_conversation_totals(&account, None).await;
    Ok(page)
}

pub(super) async fn refresh_unified(
    global: Arc<BgGlobal>,
    request_id: u64,
    accounts: Vec<UnifiedAccountPage>,
    page_size: usize,
) {
    let cached = join_all(accounts.iter().map(|account| {
        global
            .cache
            .load_headers(account.account_id.clone(), None, page_size, 0)
    }))
    .await;
    let mut cached_by_account: HashMap<_, _> = accounts
        .iter()
        .zip(cached)
        .filter_map(|(account, result)| {
            result
                .ok()
                .map(|messages| (account.account_id.clone(), messages))
        })
        .collect();
    let mut cached_messages: Vec<_> = cached_by_account.values().flatten().cloned().collect();
    cached_messages.sort_by(|left, right| {
        right.received.cmp(&left.received).then_with(|| {
            left.account_id
                .0
                .cmp(&right.account_id.0)
                .then_with(|| left.id.cmp(&right.id))
        })
    });
    let mut cached_seen = HashSet::new();
    cached_messages
        .retain(|message| cached_seen.insert((message.account_id.clone(), message.id.clone())));
    cached_messages.truncate(page_size);
    if !cached_messages.is_empty()
        && global.unified_request_id.load(Ordering::Acquire) == request_id
    {
        global.emit(Evt::UnifiedCachedMessages {
            request_id,
            messages: cached_messages,
        });
    }

    let initial = join_all(accounts.iter().map(|account| {
        fetch_unified_page(
            global.clone(),
            account.account_id.clone(),
            account.page_size,
            None,
        )
    }))
    .await;
    let mut streams = Vec::with_capacity(accounts.len());
    for (account, result) in accounts.iter().zip(initial) {
        match result {
            Ok(page) => {
                let exhausted = page.messages.is_empty() || page.next.is_none();
                streams.push(UnifiedStream {
                    account_id: account.account_id.clone(),
                    chunk_size: account.page_size,
                    buffer: page.messages.into(),
                    next: page.next,
                    exhausted,
                });
            }
            Err(error) => {
                log::warn!(
                    "initial unified load for account {}: {error:#}",
                    account.account_id
                );
                streams.push(UnifiedStream {
                    account_id: account.account_id.clone(),
                    chunk_size: account.page_size,
                    buffer: cached_by_account
                        .remove(&account.account_id)
                        .unwrap_or_default()
                        .into(),
                    next: None,
                    exhausted: true,
                });
            }
        }
    }

    let mut pagination = UnifiedPagination {
        request_id,
        page_size: page_size.max(1),
        streams,
        emitted: HashSet::new(),
    };
    let messages = pagination.take_page(&global).await;
    let has_more = pagination.has_more();
    if global.unified_request_id.load(Ordering::Acquire) != request_id {
        return;
    }
    let mut slot = global.unified_mailbox.lock().await;
    if global.unified_request_id.load(Ordering::Acquire) != request_id {
        return;
    }
    *slot = Some(pagination);
    drop(slot);
    global.emit(Evt::UnifiedMessages {
        request_id,
        messages,
        has_more,
    });

    for account in accounts {
        let Some(account) = global.account(&account.account_id).await else {
            continue;
        };
        tokio::spawn(async move {
            let Ok(auth) = account.ensure_auth().await else {
                return;
            };
            sync_cached_folder(account.clone(), &auth, None).await;
        });
    }
}

pub(super) async fn load_more_unified(global: Arc<BgGlobal>, request_id: u64) {
    if global.unified_request_id.load(Ordering::Acquire) != request_id {
        return;
    }
    let Some(mut pagination) = global.unified_mailbox.lock().await.take() else {
        return;
    };
    if pagination.request_id != request_id {
        return;
    }
    let messages = pagination.take_page(&global).await;
    let has_more = pagination.has_more();
    if global.unified_request_id.load(Ordering::Acquire) != request_id {
        return;
    }
    let mut slot = global.unified_mailbox.lock().await;
    if global.unified_request_id.load(Ordering::Acquire) != request_id {
        return;
    }
    *slot = Some(pagination);
    drop(slot);
    global.emit(Evt::UnifiedMoreMessages {
        request_id,
        messages,
        has_more,
    });
}

/// Ships the cache's per-thread counts for a folder that was just listed.
///
/// Ordering is what makes this correct: `store_headers` and this query travel
/// the same actor channel, so the counts always observe the page that was
/// stored just before. Failures stay silent — a missing count degrades a
/// group's counter to what the list holds, which is never worse than not
/// grouping at all.
async fn emit_conversation_totals(account: &Arc<BgAccount>, folder_id: Option<String>) {
    match account
        .global
        .cache
        .conversation_totals(account.id.clone(), folder_id.clone())
        .await
    {
        Ok(totals) => account.emit(Evt::ConversationTotals {
            account_id: account.id.clone(),
            folder_id,
            totals,
        }),
        Err(e) => log::warn!("conversation totals unavailable: {e:#}"),
    }
}

pub(super) async fn refresh_inbox(
    account: Arc<BgAccount>,
    folder_id: Option<String>,
    limit: usize,
) {
    if let Ok(cached) = account
        .global
        .cache
        .load_headers(account.id.clone(), folder_id.clone(), limit, 0)
        .await
    {
        if !cached.is_empty() {
            account.emit(Evt::CachedMessages {
                account_id: account.id.clone(),
                folder_id: folder_id.clone(),
                messages: cached,
            });
            emit_conversation_totals(&account, folder_id.clone()).await;
        }
    }
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(e) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: false,
                error: Some(e.to_string()),
            });
            return;
        }
    };
    // Gmail requires a historyId from before the initial load to avoid losing
    // a change made between listing messages and initializing the history
    // journal. Capture it before the mailbox GET.
    if matches!(account.provider, Provider::Google)
        && account
            .global
            .cache
            .load_cursor(account.id.clone(), folder_id.clone())
            .await
            .unwrap_or(None)
            .is_none()
    {
        let permit = account.mailbox_permit().await;
        let seed = account
            .session(&auth)
            .sync_folder_messages(folder_id.as_deref(), None)
            .await;
        drop(permit);
        if let Ok(seed) = seed {
            if let Some(cursor) = seed.cursor {
                account.global.cache.store_cursor(
                    account.id.clone(),
                    folder_id.clone(),
                    "Google".to_string(),
                    cursor,
                );
            }
        }
    }
    let permit = account.mailbox_permit().await;
    account.emit(Evt::Status(tr!("status-loading-messages").to_string()));
    let response = account
        .session(&auth)
        .list_folder_messages(folder_id.as_deref(), limit, 0)
        .await;
    drop(permit);
    match response {
        Ok(mut msgs) => {
            for m in &mut msgs {
                m.account_id = account.id.clone();
            }
            let mut seen = account.seen_ids.lock().await;
            *seen = msgs.iter().map(|h| h.id.clone()).collect();
            drop(seen);
            account
                .global
                .cache
                .store_headers(account.id.clone(), folder_id.clone(), msgs.clone());
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: true,
                error: None,
            });
            account.emit(Evt::Messages {
                account_id: account.id.clone(),
                messages: msgs,
            });
            emit_conversation_totals(&account, folder_id.clone()).await;
            sync_cached_folder(account.clone(), &auth, folder_id).await;
            super::operations::drain_account(account).await;
        }
        Err(e) => account.emit(Evt::SyncStateChanged {
            account_id: account.id.clone(),
            online: false,
            error: Some(e.to_string()),
        }),
    }
}

async fn sync_cached_folder(
    account: Arc<BgAccount>,
    auth: &super::AuthOwned,
    folder_id: Option<String>,
) {
    let _sync = account.mail_sync.lock().await;
    let stored_cursor = account
        .global
        .cache
        .load_cursor(account.id.clone(), folder_id.clone())
        .await
        .unwrap_or(None);
    let incremental = stored_cursor.is_some();
    let mut cursor_or_page = stored_cursor.clone();
    loop {
        let permit = account.mailbox_permit().await;
        let result = account
            .session(auth)
            .sync_folder_messages(folder_id.as_deref(), cursor_or_page.as_deref())
            .await;
        drop(permit);
        let page = match result {
            Ok(page) => page,
            Err(e) => {
                let text = e.to_string();
                if stored_cursor.is_some() && (text.contains("(404") || text.contains("(410")) {
                    account
                        .global
                        .cache
                        .purge_folder(account.id.clone(), folder_id.clone());
                }
                log::warn!("incremental synchronization failed: {e:#}");
                return;
            }
        };
        let crate::providers::MailSyncPage {
            mut upserts,
            deleted,
            removed_from_folder,
            next,
            cursor,
        } = page;
        for header in &mut upserts {
            header.account_id = account.id.clone();
        }
        if !upserts.is_empty() {
            account.global.cache.store_headers(
                account.id.clone(),
                folder_id.clone(),
                upserts.clone(),
            );
        }
        for id in &deleted {
            account
                .global
                .cache
                .remove_message(account.id.clone(), id.clone());
        }
        for id in &removed_from_folder {
            account.global.cache.remove_from_folder(
                account.id.clone(),
                folder_id.clone(),
                id.clone(),
            );
        }
        let mut removed = deleted;
        removed.extend(removed_from_folder);
        if incremental && (!upserts.is_empty() || !removed.is_empty()) {
            account.emit(Evt::MessageChanges {
                account_id: account.id.clone(),
                folder_id: folder_id.clone(),
                upserts,
                deleted: removed,
            });
        }
        if let Some(cursor) = cursor {
            account.global.cache.store_cursor(
                account.id.clone(),
                folder_id.clone(),
                format!("{:?}", account.provider),
                cursor,
            );
            return;
        }
        let Some(next) = next else { return };
        cursor_or_page = Some(next);
        tokio::task::yield_now().await;
    }
}

pub(super) async fn load_more(
    account: Arc<BgAccount>,
    folder_id: Option<String>,
    skip: usize,
    limit: usize,
) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account
        .session(&auth)
        .list_folder_messages(folder_id.as_deref(), limit, skip)
        .await
    {
        Ok(mut msgs) => {
            for m in &mut msgs {
                m.account_id = account.id.clone();
            }
            let returned = msgs.len();
            let mut seen = account.seen_ids.lock().await;
            for m in &msgs {
                seen.insert(m.id.clone());
            }
            drop(seen);
            account
                .global
                .cache
                .store_headers(account.id.clone(), folder_id.clone(), msgs.clone());
            account.emit(Evt::MoreMessages {
                account_id: account.id.clone(),
                messages: msgs,
                has_more: returned >= limit,
            });
            emit_conversation_totals(&account, folder_id).await;
        }
        Err(e) => account.emit(Evt::Error(e.to_string())),
    }
}

async fn auto_refresh_check(account: Arc<BgAccount>, limit: usize) -> bool {
    let folder_id = None;
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: false,
                error: Some(e.to_string()),
            });
            return false;
        }
    };
    let permit = account.mailbox_permit().await;
    let response = account
        .session(&auth)
        .list_folder_messages(folder_id.as_deref(), limit, 0)
        .await;
    drop(permit);
    match response {
        Ok(mut msgs) => {
            for m in &mut msgs {
                m.account_id = account.id.clone();
            }
            let mut seen = account.seen_ids.lock().await;
            let was_seeded = !seen.is_empty();
            let new_msgs: Vec<MessageHeader> = if was_seeded {
                msgs.iter()
                    .filter(|h| !seen.contains(&h.id))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            for m in &msgs {
                seen.insert(m.id.clone());
            }
            drop(seen);
            account
                .global
                .cache
                .store_headers(account.id.clone(), folder_id.clone(), msgs.clone());
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: true,
                error: None,
            });
            if was_seeded {
                if !new_msgs.is_empty() {
                    account.emit(Evt::NewMessages {
                        account_id: account.id.clone(),
                        messages: new_msgs,
                    });
                }
            } else {
                account.emit(Evt::Messages {
                    account_id: account.id.clone(),
                    messages: msgs,
                });
            }
            emit_conversation_totals(&account, folder_id.clone()).await;
            sync_cached_folder(account.clone(), &auth, folder_id).await;
            super::operations::drain_account(account).await;
            true
        }
        Err(e) => {
            log::warn!("auto-refresh failed: {e:#}");
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: false,
                error: Some(e.to_string()),
            });
            false
        }
    }
}

pub(super) async fn set_auto_refresh(
    account: Arc<BgAccount>,
    _folder_id: Option<String>,
    secs: u32,
    limit: usize,
) {
    let mut guard = account.auto_refresh.lock().await;
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    if secs > 0 {
        let acc = account.clone();
        let handle = tokio::spawn(async move {
            let base = std::time::Duration::from_secs(u64::from(secs.max(15)));
            let mut delay = base;
            loop {
                tokio::time::sleep(delay).await;
                delay = if auto_refresh_check(acc.clone(), limit).await {
                    base
                } else {
                    (delay * 2).min(std::time::Duration::from_secs(15 * 60))
                };
            }
        });
        *guard = Some(handle);
    }
}

pub(super) async fn open_message(account: Arc<BgAccount>, id: String) {
    let started = std::time::Instant::now();
    let cache_started = std::time::Instant::now();
    let had_cached = match account
        .global
        .cache
        .load_message(account.id.clone(), id.clone())
        .await
    {
        Ok(Some(message)) => {
            account
                .global
                .cache
                .set_read(account.id.clone(), id.clone(), true);
            account.emit(Evt::CachedMessageOpened {
                account_id: account.id.clone(),
                message: Box::new(message),
            });
            true
        }
        Ok(None) => false,
        Err(e) => {
            log::warn!("reading cached body: {e:#}");
            false
        }
    };
    let cache_elapsed = cache_started.elapsed();
    let auth_started = std::time::Instant::now();
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(e) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: false,
                error: Some(e.to_string()),
            });
            if !had_cached {
                account.emit(Evt::Error(e.to_string()));
            }
            return;
        }
    };
    let auth_elapsed = auth_started.elapsed();
    let gate_started = std::time::Instant::now();
    let permit = account.mailbox_permit().await;
    let gate_elapsed = gate_started.elapsed();
    let fetch_started = std::time::Instant::now();
    let fetched = account.session(&auth).get_message(&id).await;
    let fetch_elapsed = fetch_started.elapsed();
    match fetched {
        Ok(mut m) => {
            m.header.account_id = account.id.clone();
            let mark_read_deferred = !m.header.is_read;
            // Opening a message is optimistic: every viewer path already
            // treats it as read locally. Persist and display that state before
            // the provider round-trip so Gmail's label update is not on the
            // critical path.
            m.header.is_read = true;
            account
                .global
                .cache
                .store_message(account.id.clone(), m.clone());
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: true,
                error: None,
            });
            let inline_image_count = m.inline_images.len();
            let attachment_count = m.attachments.len();
            account.emit(Evt::MessageOpened {
                account_id: account.id.clone(),
                message: Box::new(m),
            });
            log::debug!(
                "message opened in {} ms \
                 (provider={:?}, cache_hit={}, cache_lookup_ms={}, auth_ms={}, \
                 mailbox_wait_ms={}, fetch_ms={}, mark_read_deferred={}, \
                 inline_images={}, files={})",
                started.elapsed().as_millis(),
                account.provider,
                had_cached,
                cache_elapsed.as_millis(),
                auth_elapsed.as_millis(),
                gate_elapsed.as_millis(),
                fetch_elapsed.as_millis(),
                mark_read_deferred,
                inline_image_count,
                attachment_count
            );
            drop(permit);
            if mark_read_deferred {
                // This task must outlive `open_message`: selecting another row
                // aborts the current open task, but must not cancel a read
                // receipt that the UI has already applied optimistically.
                tokio::spawn(async move {
                    let mark_read_started = std::time::Instant::now();
                    let _serial = account
                        .deferred_read_gate
                        .clone()
                        .acquire_owned()
                        .await
                        .expect("deferred read semaphore is never closed");
                    let queue_elapsed = mark_read_started.elapsed();
                    let auth = match account.ensure_auth().await {
                        Ok(auth) => auth,
                        Err(e) => {
                            log::warn!("deferred mark_read authentication failed: {e:#}");
                            return;
                        }
                    };
                    let mailbox_started = std::time::Instant::now();
                    let _permit = account.mailbox_permit().await;
                    let mailbox_elapsed = mailbox_started.elapsed();
                    let provider_started = std::time::Instant::now();
                    match account.session(&auth).mark_read(&id, true).await {
                        Ok(()) => log::debug!(
                            "deferred mark_read completed in {} ms \
                             (provider={:?}, queue_ms={}, mailbox_wait_ms={}, provider_ms={})",
                            mark_read_started.elapsed().as_millis(),
                            account.provider,
                            queue_elapsed.as_millis(),
                            mailbox_elapsed.as_millis(),
                            provider_started.elapsed().as_millis()
                        ),
                        Err(e) => log::warn!("deferred mark_read failed: {e:#}"),
                    }
                });
            }
        }
        Err(e) => {
            account.emit(Evt::SyncStateChanged {
                account_id: account.id.clone(),
                online: false,
                error: Some(e.to_string()),
            });
            if !had_cached {
                account.emit(Evt::Error(e.to_string()));
            }
        }
    }
}

pub(super) async fn load_quick_action_message(
    account: Arc<BgAccount>,
    request_id: u64,
    id: String,
) {
    if let Ok(Some(message)) = account
        .global
        .cache
        .load_message(account.id.clone(), id.clone())
        .await
    {
        if message
            .attachments
            .iter()
            .all(|attachment| attachment.bytes.is_some())
        {
            account.emit(Evt::QuickActionMessageLoaded {
                request_id,
                account_id: account.id.clone(),
                message: Box::new(message),
            });
            return;
        }
    }
    let result = async {
        let auth = account.ensure_auth().await?;
        let _permit = account.mailbox_permit().await;
        let session = account.session(&auth);
        let mut message = session.get_message(&id).await?;
        for attachment in &mut message.attachments {
            if attachment.bytes.is_none() && !attachment.id.is_empty() {
                attachment.bytes = Some(session.fetch_attachment(&id, &attachment.id).await?);
            }
        }
        message.header.account_id = account.id.clone();
        account
            .global
            .cache
            .store_message(account.id.clone(), message.clone());
        anyhow::Ok(message)
    }
    .await;
    match result {
        Ok(message) => account.emit(Evt::QuickActionMessageLoaded {
            request_id,
            account_id: account.id.clone(),
            message: Box::new(message),
        }),
        Err(error) => account.emit(Evt::QuickActionMessageError {
            request_id,
            account_id: account.id.clone(),
            error: format!("{error:#}"),
        }),
    }
}

pub(super) async fn fetch_attachment(
    account: Arc<BgAccount>,
    message_id: String,
    attachment_id: String,
) {
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(error) => {
            account.emit(Evt::AttachmentFetchError {
                account_id: account.id.clone(),
                message_id,
                attachment_id,
                error: format!("{error:#}"),
            });
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match account
        .session(&auth)
        .fetch_attachment(&message_id, &attachment_id)
        .await
    {
        Ok(bytes) => {
            account.global.cache.store_attachment(
                account.id.clone(),
                message_id.clone(),
                attachment_id.clone(),
                bytes.clone(),
            );
            account.emit(Evt::AttachmentFetched {
                account_id: account.id.clone(),
                message_id,
                attachment_id,
                bytes,
            });
        }
        Err(error) => account.emit(Evt::AttachmentFetchError {
            account_id: account.id.clone(),
            message_id,
            attachment_id,
            error: format!("{error:#}"),
        }),
    }
}

/// Lazy-loader for thread entries the user expanded inline. Mirrors
/// `open_message` (full body fetch + auto mark-read) but emits a
/// thread-scoped event so the UI's `mailbox.selected` keeps pointing at
/// the primary message the user is actively viewing.
pub(super) async fn load_thread_message(account: Arc<BgAccount>, id: String) {
    let had_cached = match account
        .global
        .cache
        .load_message(account.id.clone(), id.clone())
        .await
    {
        Ok(Some(message)) => {
            account.emit(Evt::ThreadMessageLoaded {
                account_id: account.id.clone(),
                id: id.clone(),
                message: Box::new(message),
            });
            true
        }
        _ => false,
    };
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            if !had_cached {
                account.emit(Evt::ThreadMessageError {
                    account_id: account.id.clone(),
                    id,
                    error: e.to_string(),
                });
            }
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).get_message(&id).await {
        Ok(mut m) => {
            m.header.account_id = account.id.clone();
            if !m.header.is_read {
                if let Err(e) = account.session(&auth).mark_read(&id, true).await {
                    log::warn!("mark_read (thread) failed: {e:#}");
                } else {
                    m.header.is_read = true;
                }
            }
            account
                .global
                .cache
                .store_message(account.id.clone(), m.clone());
            account.emit(Evt::ThreadMessageLoaded {
                account_id: account.id.clone(),
                id,
                message: Box::new(m),
            });
        }
        Err(e) => {
            if !had_cached {
                account.emit(Evt::ThreadMessageError {
                    account_id: account.id.clone(),
                    id,
                    error: e.to_string(),
                });
            }
        }
    }
}

/// Restores session UI from SQLite before provider accounts finish resuming.
pub(super) async fn load_cached_message(global: Arc<BgGlobal>, account_id: AccountId, id: String) {
    if let Ok(Some(mut message)) = global
        .cache
        .load_message(account_id.clone(), id.clone())
        .await
    {
        message.header.account_id = account_id.clone();
        global.emit(Evt::ThreadMessageLoaded {
            account_id,
            id,
            message: Box::new(message),
        });
    }
}

pub(super) async fn perform_delete(account: Arc<BgAccount>, id: String) -> Result<()> {
    let auth = account.ensure_auth().await?;
    let _permit = account.mailbox_permit().await;
    account.session(&auth).delete_message(&id).await?;
    account.seen_ids.lock().await.remove(&id);
    account
        .global
        .cache
        .remove_message(account.id.clone(), id.clone());
    account.emit(Evt::MessageDeleted {
        account_id: account.id.clone(),
        id,
    });
    Ok(())
}

pub(super) async fn perform_set_flag(
    account: Arc<BgAccount>,
    id: String,
    flagged: bool,
) -> Result<()> {
    let auth = account.ensure_auth().await?;
    let _permit = account.mailbox_permit().await;
    account.session(&auth).set_flag(&id, flagged).await?;
    account
        .global
        .cache
        .set_flag(account.id.clone(), id, flagged);
    Ok(())
}

/// Toggle the read/unread state of a message without opening it. The UI
/// updates its row optimistically before the cmd is dispatched, so this is
/// fire-and-forget — only errors flow back through `Evt::Error`.
pub(super) async fn perform_mark_read(
    account: Arc<BgAccount>,
    id: String,
    read: bool,
) -> Result<()> {
    let auth = account.ensure_auth().await?;
    let _permit = account.mailbox_permit().await;
    account.session(&auth).mark_read(&id, read).await?;
    account.global.cache.set_read(account.id.clone(), id, read);
    Ok(())
}

/// Cache-first search: the local full-text index answers first — instantly and
/// without a network round-trip — then the provider reconciles.
///
/// Both phases emit `Evt::SearchResults`; the UI deduplicates and merges them,
/// so the provider adds whatever the cache had not seen instead of replacing
/// what is already on screen. The local hits come back ranked by relevance
/// (`bm25`), which decides *which* ones survive the `limit`; the view then
/// sorts everything chronologically, as it does for a folder listing.
pub(super) async fn search_messages(
    account: Arc<BgAccount>,
    query: String,
    scope: crate::runtime::protocol::SearchScope,
    limit: usize,
) {
    // The field is parsed once here; the cache and the provider each render
    // the result into their own dialect, so `de:alice` means the same thing
    // whichever answers.
    let parsed = crate::search_query::SearchQuery::parse(&query);
    if parsed.is_empty() {
        return;
    }
    let cached = account
        .global
        .cache
        .search(
            parsed.clone(),
            Some(account.id.clone()),
            scope.clone(),
            limit,
        )
        .await;
    let mut answered_locally = false;
    match cached {
        Ok(messages) if !messages.is_empty() => {
            answered_locally = true;
            account.emit(Evt::SearchResults {
                account_id: account.id.clone(),
                query: query.clone(),
                messages,
            });
        }
        Ok(_) => {}
        // A degraded index must not take the provider search down with it.
        Err(e) => log::warn!("local search failed: {e:#}"),
    }

    // Not `auth_or_report`: offline, having already shown local results, an
    // error toast would contradict what the user is looking at.
    let auth = match account.ensure_auth().await {
        Ok(auth) => auth,
        Err(e) => {
            if !answered_locally {
                account.emit(Evt::Error(e.to_string()));
            } else {
                log::info!("search served from cache only: {e:#}");
            }
            return;
        }
    };
    let _permit = account.mailbox_permit().await;
    account.emit(Evt::Status(
        tr!("status-searching", { query: query.clone() }).to_string(),
    ));
    match account
        .session(&auth)
        .search(&parsed, scope.folder(), limit)
        .await
    {
        Ok(mut messages) => {
            // Backends drop what their dialect cannot express — Graph cannot
            // combine `$search` with a date filter, IMAP has no attachment
            // predicate — so the query is re-applied to what came back. Without
            // this, `avec:pj` would quietly return messages without one.
            messages.retain(|message| parsed.matches(message));
            for m in &mut messages {
                m.account_id = account.id.clone();
            }
            account.emit(Evt::SearchResults {
                account_id: account.id.clone(),
                query,
                messages,
            });
        }
        Err(e) => account.emit(Evt::Error(e.to_string())),
    }
}

pub(super) async fn load_thread(account: Arc<BgAccount>, conversation_id: String) {
    let started = std::time::Instant::now();
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    let _permit = account.mailbox_permit().await;
    match account.session(&auth).list_thread(&conversation_id).await {
        Ok(mut messages) => {
            let message_count = messages.len();
            for m in &mut messages {
                m.account_id = account.id.clone();
            }
            account.emit(Evt::Thread {
                account_id: account.id.clone(),
                conversation_id,
                messages,
            });
            log::debug!(
                "conversation headers loaded in {} ms (messages={})",
                started.elapsed().as_millis(),
                message_count
            );
        }
        Err(e) => log::warn!("thread load failed: {e:#}"),
    }
}

pub(super) async fn perform_move(
    account: Arc<BgAccount>,
    message_id: String,
    source_folder_id: Option<String>,
    target_folder_id: String,
) -> Result<()> {
    let auth = account.ensure_auth().await?;
    let _permit = account.mailbox_permit().await;
    let new_id = account
        .session(&auth)
        .move_message(&message_id, source_folder_id.as_deref(), &target_folder_id)
        .await?;
    account.seen_ids.lock().await.remove(&message_id);
    account.global.cache.move_message(
        account.id.clone(),
        message_id.clone(),
        source_folder_id.clone(),
        target_folder_id.clone(),
        new_id.clone(),
    );
    account.emit(Evt::MessageMoved {
        account_id: account.id.clone(),
        message_id,
        source_folder_id,
        target_folder_id,
        new_id,
    });
    Ok(())
}
