//! Submission and retry of durable mail operations.

use super::operation_store::{OperationKind, StoredOperation, UnreadableOperation};
use super::{
    mailbox, send, BgAccount, BgGlobal, Cmd, Evt, MessageMutationKind, QuickActionExecution,
    QuickActionStep,
};
use crate::model::AccountId;
use std::sync::Arc;

const MAX_MUTATION_ATTEMPTS: u32 = 8;

pub(super) async fn schedule_quick_action(
    global: Arc<BgGlobal>,
    account_id: AccountId,
    execution: QuickActionExecution,
    delay_secs: u32,
) {
    let execute_at = chrono::Utc::now().timestamp() + i64::from(delay_secs);
    let kind = OperationKind::QuickAction {
        execution: execution.clone(),
        next_step: 0,
    };
    if let Err(error) = global
        .operations
        .enqueue_at(account_id.clone(), kind, execute_at)
        .await
    {
        global.emit(Evt::QuickActionFailed {
            account_id,
            remaining: execution,
            completed_steps: 0,
            error: tr!("runtime-error-operation-store", {
                error: format!("{error:#}")
            })
            .to_string(),
        });
        return;
    }
    if let Some(account) = global.account(&account_id).await {
        tokio::spawn(async move {
            if delay_secs > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs.into())).await;
            }
            drain_account(account).await;
        });
    }
}

pub(super) async fn cancel_quick_action(
    global: Arc<BgGlobal>,
    account_id: AccountId,
    execution_id: u64,
) {
    match global
        .operations
        .cancel_quick_action(account_id.clone(), execution_id)
        .await
    {
        Ok(Some(action_name)) => global.emit(Evt::QuickActionCancelled {
            account_id,
            execution_id,
            action_name,
        }),
        Ok(None) => {}
        Err(error) => log::warn!("cancelling quick action {execution_id}: {error:#}"),
    }
}

/// The durable form of a command the UI held behind an undo window, or `None`
/// for a command the outbox does not carry. Kept in step with
/// `Cmd::is_durable_operation`, which the UI consults before scheduling.
fn durable_operation(command: Cmd) -> Option<(AccountId, OperationKind)> {
    match command {
        Cmd::DeleteMessage { account_id, id } => Some((account_id, OperationKind::Delete { id })),
        Cmd::MoveMessage {
            account_id,
            message_id,
            source_folder_id,
            target_folder_id,
        } => Some((
            account_id,
            OperationKind::Move {
                message_id,
                source_folder_id,
                target_folder_id,
            },
        )),
        Cmd::SetFlag {
            account_id,
            id,
            flagged,
        } => Some((account_id, OperationKind::SetFlag { id, flagged })),
        Cmd::MarkRead {
            account_id,
            id,
            read,
        } => Some((account_id, OperationKind::MarkRead { id, read })),
        Cmd::AddTag {
            account_id,
            message_id,
            tag_id,
        } => Some((
            account_id,
            OperationKind::SetTag {
                message_id,
                tag_id,
                added: true,
            },
        )),
        Cmd::RemoveTag {
            account_id,
            message_id,
            tag_id,
        } => Some((
            account_id,
            OperationKind::SetTag {
                message_id,
                tag_id,
                added: false,
            },
        )),
        Cmd::SendMail {
            account_id,
            compose_id,
            reply_to,
            reply_all,
            forward_of,
            draft_id,
            mail,
        } => Some((
            account_id,
            OperationKind::Send {
                compose_id,
                reply_to,
                reply_all,
                forward_of,
                draft_id,
                mail,
            },
        )),
        _ => None,
    }
}

/// Grace between the end of the UI's undo window and the moment the outbox
/// may run the operations. The UI stops offering "cancel" when its own timer
/// fires; this margin guarantees that a cancel clicked just before that still
/// reaches the store while the rows are not due, rather than racing a drain.
const SCHEDULE_GRACE_SECS: i64 = 1;

/// Persists the commands of an undo window in the outbox, due when the window
/// closes. Undo is then a row deletion (`cancel_scheduled_operations`), and
/// closing Aviary inside the window no longer loses the action: the rows are
/// on disk and run at the next drain, at startup at the latest.
pub(super) async fn schedule_operations(
    global: Arc<BgGlobal>,
    schedule_id: u64,
    delay_secs: u32,
    commands: Vec<Cmd>,
) {
    let items: Vec<_> = commands
        .into_iter()
        .filter_map(|command| {
            let item = durable_operation(command);
            if item.is_none() {
                log::error!("a command the outbox cannot carry was scheduled; dropped");
            }
            item
        })
        .collect();
    if items.is_empty() {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let execute_at = now_ms.div_euclid(1000) + i64::from(delay_secs) + SCHEDULE_GRACE_SECS;
    let mut accounts: Vec<AccountId> = Vec::new();
    for (account_id, _) in &items {
        if !accounts.contains(account_id) {
            accounts.push(account_id.clone());
        }
    }
    // What a refusal has to report, taken before the store consumes the
    // operations — cloning them would copy every attachment of a send.
    let failed: Vec<_> = items
        .iter()
        .map(|(account_id, kind)| (account_id.clone(), Unqueued::of(kind)))
        .collect();
    if let Err(error) = global
        .operations
        .enqueue_scheduled(schedule_id, items, execute_at)
        .await
    {
        let error = tr!("runtime-error-operation-store", {
            error: format!("{error:#}")
        })
        .to_string();
        for (account_id, unqueued) in failed {
            report_unqueued(&global, account_id, unqueued, error.clone()).await;
        }
        return;
    }
    let wait_ms = (execute_at * 1000 - now_ms).max(0) as u64;
    for account_id in accounts {
        // An account not restored yet drains its outbox once it is.
        let Some(account) = global.account(&account_id).await else {
            continue;
        };
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            spawn_drain(account);
        });
    }
}

/// Undo: removes what `schedule_operations` stored under `schedule_id`, as
/// long as it has not come due, and tells the UI whether everything was taken
/// back.
pub(super) async fn cancel_scheduled_operations(global: Arc<BgGlobal>, schedule_id: u64) {
    match global.operations.cancel_scheduled(schedule_id).await {
        Ok((cancelled, expected)) => global.emit(Evt::ScheduledOperationsCancelled {
            schedule_id,
            cancelled,
            expected,
        }),
        Err(error) => {
            log::warn!("cancelling scheduled operations {schedule_id}: {error:#}");
            global.emit(Evt::ScheduledOperationsCancelled {
                schedule_id,
                cancelled: 0,
                expected: 1,
            });
        }
    }
}

/// What the UI must hear about an operation the store refused.
enum Unqueued {
    Send(u64),
    Tag {
        message_id: String,
        tag_id: String,
        added: bool,
    },
    Mutation {
        message_id: String,
        kind: MessageMutationKind,
    },
    Nothing,
}

impl Unqueued {
    fn of(kind: &OperationKind) -> Self {
        if let Some(compose_id) = kind.compose_id() {
            return Self::Send(compose_id);
        }
        if let OperationKind::SetTag {
            message_id,
            tag_id,
            added,
        } = kind
        {
            return Self::Tag {
                message_id: message_id.clone(),
                tag_id: tag_id.clone(),
                added: *added,
            };
        }
        match (kind.message_id(), kind.message_mutation_kind()) {
            (Some(message_id), Some(kind)) => Self::Mutation {
                message_id: message_id.to_string(),
                kind,
            },
            _ => Self::Nothing,
        }
    }
}

/// Reports an operation the store refused, through the event its command
/// would have failed with, so the UI rolls back what it applied optimistically.
async fn report_unqueued(
    global: &BgGlobal,
    account_id: AccountId,
    unqueued: Unqueued,
    error: String,
) {
    match unqueued {
        Unqueued::Send(compose_id) => global.emit(Evt::MailSendError {
            account_id,
            compose_id,
            error,
        }),
        Unqueued::Tag {
            message_id,
            tag_id,
            added,
        } => global.emit(Evt::TagApplyError {
            account_id,
            message_id,
            tag_id,
            added,
            error,
        }),
        Unqueued::Mutation { message_id, kind } => {
            let header = global
                .cache
                .load_header(account_id.clone(), message_id.clone())
                .await
                .unwrap_or_else(|cache_error| {
                    log::warn!("loading rollback header: {cache_error:#}");
                    None
                });
            global.emit(Evt::MutationFailed {
                account_id,
                operation_id: 0,
                message_id,
                kind,
                header,
                error,
            });
        }
        Unqueued::Nothing => {}
    }
}

pub(super) async fn submit(global: Arc<BgGlobal>, account_id: AccountId, kind: OperationKind) {
    let compose_id = kind.compose_id();
    let message_id = kind.message_id().map(str::to_string);
    let mutation_kind = kind.message_mutation_kind();
    let quick_action = kind.quick_action().map(|(execution, next_step)| {
        let mut remaining = execution.clone();
        remaining.steps = remaining.steps.into_iter().skip(next_step).collect();
        (remaining, next_step)
    });
    let operation = match global.operations.enqueue(account_id.clone(), kind).await {
        Ok(operation) => operation,
        Err(error) => {
            let error = tr!("runtime-error-operation-store", {
                error: format!("{error:#}")
            });
            if let Some((remaining, completed_steps)) = quick_action {
                global.emit(Evt::QuickActionFailed {
                    account_id,
                    remaining,
                    completed_steps,
                    error: error.to_string(),
                });
            } else if let Some(compose_id) = compose_id {
                global.emit(Evt::MailSendError {
                    account_id,
                    compose_id,
                    error: error.to_string(),
                });
            } else if let (Some(message_id), Some(kind)) = (message_id, mutation_kind) {
                let header = match global
                    .cache
                    .load_header(account_id.clone(), message_id.clone())
                    .await
                {
                    Ok(header) => header,
                    Err(cache_error) => {
                        log::warn!("loading rollback header: {cache_error:#}");
                        None
                    }
                };
                global.emit(Evt::MutationFailed {
                    account_id,
                    operation_id: 0,
                    message_id,
                    kind,
                    header,
                    error: error.to_string(),
                });
            }
            return;
        }
    };

    let Some(account) = global.account(&account_id).await else {
        emit_deferred(&global, &operation);
        return;
    };
    spawn_drain(account);
}

/// Runs a drain on a task of its own whose handle nobody keeps, so nothing can
/// abort it.
///
/// Callers that live in abortable tasks — the auto-refresh loop, a throttled
/// refresh, the retry timer — must go through here rather than awaiting
/// `drain_account` inline: aborting them would otherwise cancel whatever the
/// drain was executing, a send included, halfway through. Concurrent drains do
/// not duplicate work, `operation_drain` serializes them per account.
pub(super) fn spawn_drain(account: Arc<BgAccount>) {
    tokio::spawn(drain_account(account));
}

pub(super) async fn drain_account(account: Arc<BgAccount>) {
    let _serial = account.operation_drain.lock().await;
    match account
        .global
        .operations
        .take_interrupted(account.id.clone())
        .await
    {
        Ok(interrupted) => {
            report_unreadable(&account, &interrupted.unreadable);
            for operation in interrupted.ready {
                if let Some((execution, next_step)) = operation.kind.quick_action() {
                    let mut remaining = execution.clone();
                    remaining.steps = remaining
                        .steps
                        .into_iter()
                        .skip(next_step.saturating_add(1))
                        .collect();
                    account.emit(Evt::QuickActionSendUncertain {
                        account_id: account.id.clone(),
                        remaining,
                    });
                } else if let Some(compose_id) = operation.kind.compose_id() {
                    account.emit(Evt::MailSendError {
                        account_id: account.id.clone(),
                        compose_id,
                        error: tr!("outbox-delivery-uncertain").to_string(),
                    });
                } else {
                    log::warn!("unexpected interrupted non-send operation {}", operation.id);
                }
            }
        }
        Err(error) => {
            log::warn!("loading interrupted durable operations failed: {error:#}");
        }
    }
    match account.global.operations.load_due(account.id.clone()).await {
        Ok(loaded) => {
            report_unreadable(&account, &loaded.unreadable);
            for operation in loaded.ready {
                execute(account.clone(), operation).await;
            }
        }
        // Still arm the timer below: a transient store failure must not leave
        // the outbox waiting for the next manual refresh.
        Err(error) => log::warn!("loading durable operations failed: {error:#}"),
    }
    drop(_serial);
    arm_retry_timer(account).await;
}

/// Tells the user about rows the store could not decode and has set aside. A
/// send is the one that matters — silently dropping it would lose a message
/// the user believes is on its way — so it goes back to its composer when the
/// row still names one.
fn report_unreadable(account: &BgAccount, unreadable: &[UnreadableOperation]) {
    for operation in unreadable {
        let error = tr!("runtime-error-operation-store", {
            error: format!("unreadable durable operation {}: {}", operation.id, operation.error)
        })
        .to_string();
        match operation.compose_id {
            Some(compose_id) => account.emit(Evt::MailSendError {
                account_id: account.id.clone(),
                compose_id,
                error,
            }),
            None => account.emit(Evt::Error(error)),
        }
    }
}

/// Schedules the next drain on the earliest deadline `handle_failure` wrote to
/// the store.
///
/// The exponential backoff is otherwise inert: nothing else polls the outbox, so
/// a send that failed on a dropped connection would sit there until the user
/// happened to refresh — and never at all with auto-refresh off.
///
/// The boxed return type keeps the future types finite: the timer leads back
/// to `drain_account` (through `spawn_drain`), which calls back here.
fn arm_retry_timer(
    account: Arc<BgAccount>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        // The deadline is read under the same lock that arms the timer. Read
        // before it, two drains finishing together could interleave so that
        // the one holding the older answer armed last, replacing the fresh
        // timer with a stale deadline — or with none at all.
        let mut guard = account.operation_retry.lock().await;
        let next = match account
            .global
            .operations
            .next_attempt_at(account.id.clone())
            .await
        {
            Ok(next) => next,
            Err(error) => {
                log::warn!("reading the next outbox deadline failed: {error:#}");
                return;
            }
        };

        // Aborting only ever cancels a sleep: the timer hands the drain to a
        // detached task, so a later re-arm cannot cut an operation in flight.
        if let Some(handle) = guard.take() {
            handle.abort();
        }
        let Some(next) = next else {
            return;
        };

        let delay = next.saturating_sub(chrono::Utc::now().timestamp()).max(0);
        let waiting = account.clone();
        *guard = Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay as u64)).await;
            spawn_drain(waiting);
        }));
    })
}

async fn execute(account: Arc<BgAccount>, operation: StoredOperation) {
    if matches!(operation.kind, OperationKind::QuickAction { .. }) {
        execute_quick_action(account, operation).await;
        return;
    }
    if operation.kind.is_send() {
        if let Err(error) = account.global.operations.mark_executing(operation.id).await {
            handle_failure(account, operation, error).await;
            return;
        }
    }
    let result = match operation.kind.clone() {
        OperationKind::Delete { id } => mailbox::perform_delete(account.clone(), id).await,
        OperationKind::Move {
            message_id,
            source_folder_id,
            target_folder_id,
        } => {
            mailbox::perform_move(
                account.clone(),
                message_id,
                source_folder_id,
                target_folder_id,
            )
            .await
        }
        OperationKind::SetFlag { id, flagged } => {
            mailbox::perform_set_flag(account.clone(), id, flagged).await
        }
        OperationKind::MarkRead { id, read } => {
            mailbox::perform_mark_read(account.clone(), id, read).await
        }
        OperationKind::Send {
            compose_id,
            reply_to,
            reply_all,
            forward_of,
            draft_id,
            mail,
        } => {
            send::perform_send(
                account.clone(),
                compose_id,
                reply_to,
                reply_all,
                forward_of,
                draft_id,
                mail,
            )
            .await
        }
        OperationKind::SetTag {
            message_id,
            tag_id,
            added,
        } => perform_quick_tag(account.clone(), &message_id, tag_id, added).await,
        OperationKind::QuickAction { .. } => unreachable!("handled above"),
    };

    match result {
        Ok(()) => {
            settle_completed(&account, operation.id).await;
            if let (Some(message_id), Some(kind)) = (
                operation.kind.message_id(),
                operation.kind.message_mutation_kind(),
            ) {
                account.emit(Evt::MutationSucceeded {
                    account_id: account.id.clone(),
                    operation_id: operation.id,
                    message_id: message_id.to_string(),
                    kind,
                });
            }
        }
        Err(error) => handle_failure(account, operation, error).await,
    }
}

async fn execute_quick_action(account: Arc<BgAccount>, mut operation: StoredOperation) {
    if let Some((execution, 0)) = operation.kind.quick_action() {
        account.emit(Evt::QuickActionStarted {
            account_id: account.id.clone(),
            execution_id: execution.execution_id,
            action_name: execution.action_name.clone(),
        });
    }
    loop {
        let OperationKind::QuickAction {
            execution,
            next_step,
        } = operation.kind.clone()
        else {
            return;
        };
        let Some(step) = execution.steps.get(next_step).cloned() else {
            settle_completed(&account, operation.id).await;
            account.emit(Evt::QuickActionCompleted {
                account_id: account.id.clone(),
                execution_id: execution.execution_id,
                action_name: execution.action_name,
                message_id: execution.message_id,
            });
            return;
        };

        if is_quick_send_step(&step) {
            if let Err(error) = account.global.operations.mark_executing(operation.id).await {
                quick_action_failed(account, operation, error).await;
                return;
            }
        }

        let result = perform_quick_action_step(
            account.clone(),
            execution.execution_id,
            &execution.message_id,
            step,
        )
        .await;
        if let Err(error) = result {
            quick_action_failed(account, operation, error).await;
            return;
        }

        let next_kind = OperationKind::QuickAction {
            execution,
            next_step: next_step + 1,
        };
        if let Err(error) = checkpoint(&account, operation.id, &next_kind).await {
            // A successful send followed by a failed checkpoint cannot resume:
            // replaying the row would deliver a duplicate. The remaining steps
            // go back to the user, and the row is settled as delivered so
            // `take_interrupted` does not raise the same doubt a second time.
            if matches!(
                operation.kind.quick_action(),
                Some((execution, index))
                    if execution
                        .steps
                        .get(index)
                        .is_some_and(is_quick_send_step)
            ) {
                let (execution, _) = operation.kind.quick_action().expect("checked");
                let mut remaining = execution.clone();
                remaining.steps = remaining
                    .steps
                    .into_iter()
                    .skip(next_step.saturating_add(1))
                    .collect();
                log::warn!("quick action checkpoint after a send failed: {error:#}");
                settle_completed(&account, operation.id).await;
                account.emit(Evt::QuickActionSendUncertain {
                    account_id: account.id.clone(),
                    remaining,
                });
                return;
            }
            quick_action_failed(account, operation, error).await;
            return;
        }
        operation.kind = next_kind;
        operation.attempts = 0;
    }
}

/// How many times a store write that follows a confirmed provider call is
/// tried, and the pause before the first retry (doubled each time). Short on
/// purpose: this runs under the account's drain lock.
const SETTLE_ATTEMPTS: u32 = 3;
const SETTLE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Retries `attempt` a few times on a short backoff, returning the last error.
async fn retry_store_write<F, Fut>(mut attempt: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut delay = SETTLE_BACKOFF;
    let mut tries = 1;
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) if tries >= SETTLE_ATTEMPTS => return Err(error),
            Err(error) => {
                log::debug!("durable operation store write failed, retrying: {error:#}");
                tokio::time::sleep(delay).await;
                delay *= 2;
                tries += 1;
            }
        }
    }
}

/// Takes the row of an operation the provider confirmed out of the queue.
///
/// A row that outlives its operation is worse than noise: a send left
/// `executing` comes back at the next drain as "delivery uncertain" for a mail
/// `MailSent` already confirmed, and any other row is replayed. The delete is
/// retried; failing that, the row is marked delivered — a terminal state no
/// drain replays or reports, which holds in memory for the session even when
/// the disk refuses that write too.
async fn settle_completed(account: &BgAccount, id: i64) {
    let operations = &account.global.operations;
    let Err(error) = retry_store_write(|| operations.remove(id)).await else {
        return;
    };
    log::warn!("removing completed durable operation {id}: {error:#}");
    if let Err(error) = operations.mark_delivered(id).await {
        log::error!(
            "durable operation {id} completed but could not be settled on disk; \
             it stays out of the queue for this session: {error:#}"
        );
    }
}

/// Records a quick action's progress, retried like `settle_completed`: a
/// checkpoint lost after a send is what turns a delivered mail into an
/// uncertain one.
async fn checkpoint(account: &BgAccount, id: i64, next_kind: &OperationKind) -> anyhow::Result<()> {
    let operations = &account.global.operations;
    retry_store_write(|| operations.replace_kind(id, next_kind.clone())).await
}

async fn perform_quick_action_step(
    account: Arc<BgAccount>,
    execution_id: u64,
    message_id: &str,
    step: QuickActionStep,
) -> anyhow::Result<()> {
    // A step run on a collapsed conversation may name another member.
    let target = step.target(message_id).to_string();
    match step {
        QuickActionStep::Forward { mail } => {
            send::perform_send(
                account,
                execution_id,
                None,
                false,
                Some(message_id.to_string()),
                None,
                mail,
            )
            .await
        }
        QuickActionStep::Reply { mail, reply_all } => {
            send::perform_send(
                account,
                execution_id,
                Some(message_id.to_string()),
                reply_all,
                None,
                None,
                mail,
            )
            .await
        }
        QuickActionStep::RemoveTag { tag_id } => {
            perform_quick_tag(account, message_id, tag_id, false).await
        }
        QuickActionStep::AddTag { tag_id } => {
            perform_quick_tag(account, message_id, tag_id, true).await
        }
        QuickActionStep::MarkRead { read, .. } => {
            mailbox::perform_mark_read(account.clone(), target.clone(), read).await?;
            account.emit(Evt::QuickActionMessageState {
                account_id: account.id.clone(),
                message_id: target,
                read: Some(read),
                flagged: None,
            });
            Ok(())
        }
        QuickActionStep::SetFlag { flagged } => {
            mailbox::perform_set_flag(account.clone(), message_id.to_string(), flagged).await?;
            account.emit(Evt::QuickActionMessageState {
                account_id: account.id.clone(),
                message_id: message_id.to_string(),
                read: None,
                flagged: Some(flagged),
            });
            Ok(())
        }
        QuickActionStep::Move {
            source_folder_id,
            target_folder_id,
            ..
        } => mailbox::perform_move(account, target, source_folder_id, target_folder_id).await,
    }
}

fn is_quick_send_step(step: &QuickActionStep) -> bool {
    matches!(
        step,
        QuickActionStep::Forward { .. } | QuickActionStep::Reply { .. }
    )
}

async fn perform_quick_tag(
    account: Arc<BgAccount>,
    message_id: &str,
    tag_id: String,
    added: bool,
) -> anyhow::Result<()> {
    let auth = account.ensure_auth().await?;
    let _permit = account.mailbox_permit().await;
    if added {
        account
            .session(&auth)
            .add_tag_to_message(message_id, &tag_id)
            .await?;
    } else {
        account
            .session(&auth)
            .remove_tag_from_message(message_id, &tag_id)
            .await?;
    }
    account.global.cache.set_tag(
        account.id.clone(),
        message_id.to_string(),
        tag_id.clone(),
        added,
    );
    account.emit(Evt::TagApplied {
        account_id: account.id.clone(),
        message_id: message_id.to_string(),
        tag_id,
        added,
    });
    Ok(())
}

async fn quick_action_failed(
    account: Arc<BgAccount>,
    operation: StoredOperation,
    error: anyhow::Error,
) {
    let Some((execution, next_step)) = operation.kind.quick_action() else {
        return;
    };
    let execution = execution.clone();
    let retryable = if operation.kind.is_send() {
        send_retry_is_safe(&error)
    } else {
        mutation_is_retryable(&error)
    };
    let attempts = operation.attempts.saturating_add(1);
    if retryable && attempts < MAX_MUTATION_ATTEMPTS {
        let next = chrono::Utc::now().timestamp() + retry_delay(attempts).as_secs() as i64;
        if account
            .global
            .operations
            .defer(operation.id, attempts, next, format!("{error:#}"))
            .await
            .is_ok()
        {
            account.emit(super::sync_failure_evt(account.id.clone(), &error));
            return;
        }
    }
    let _ = account.global.operations.remove(operation.id).await;
    let mut remaining = execution.clone();
    remaining.steps = remaining.steps.into_iter().skip(next_step).collect();
    account.emit(Evt::QuickActionFailed {
        account_id: account.id.clone(),
        remaining,
        completed_steps: next_step,
        error: format!("{error:#}"),
    });
}

async fn handle_failure(account: Arc<BgAccount>, operation: StoredOperation, error: anyhow::Error) {
    let attempts = operation.attempts.saturating_add(1);
    let raw_error = format!("{error:#}");
    let display_error = operation_error(&operation.kind, &raw_error);
    let retryable = if operation.kind.is_send() {
        send_retry_is_safe(&error)
    } else {
        mutation_is_retryable(&error)
    };

    if retryable && attempts < MAX_MUTATION_ATTEMPTS {
        let delay = retry_delay(attempts);
        let next = chrono::Utc::now().timestamp() + delay.as_secs() as i64;
        if let Err(store_error) = account
            .global
            .operations
            .defer(operation.id, attempts, next, raw_error.clone())
            .await
        {
            log::warn!("deferring durable operation: {store_error:#}");
        }
        if operation.attempts == 0 {
            emit_deferred(&account.global, &operation);
        }
        account.emit(super::sync_failure_evt(account.id.clone(), &error));
        return;
    }

    if let Some(compose_id) = operation.kind.compose_id() {
        // The editor session remains the recoverable source for a send that
        // cannot be retried safely. Removing the operation avoids a duplicate
        // delivery if the user edits and sends it again.
        if let Err(store_error) = account.global.operations.remove(operation.id).await {
            log::warn!("removing failed outbox operation: {store_error:#}");
        }
        account.emit(Evt::MailSendError {
            account_id: account.id.clone(),
            compose_id,
            error: display_error,
        });
    } else if let (Some(message_id), Some(kind)) = (
        operation.kind.message_id(),
        operation.kind.message_mutation_kind(),
    ) {
        let header = match account
            .global
            .cache
            .load_header(account.id.clone(), message_id.to_string())
            .await
        {
            Ok(header) => header,
            Err(cache_error) => {
                log::warn!("loading rollback header: {cache_error:#}");
                None
            }
        };
        if let Err(store_error) = account.global.operations.remove(operation.id).await {
            log::warn!("removing failed mutation: {store_error:#}");
        }
        account.emit(Evt::MutationFailed {
            account_id: account.id.clone(),
            operation_id: operation.id,
            message_id: message_id.to_string(),
            kind,
            header,
            error: display_error,
        });
    } else if let OperationKind::SetTag {
        message_id,
        tag_id,
        added,
    } = &operation.kind
    {
        if let Err(store_error) = account.global.operations.remove(operation.id).await {
            log::warn!("removing failed tag change: {store_error:#}");
        }
        account.emit(Evt::TagApplyError {
            account_id: account.id.clone(),
            message_id: message_id.clone(),
            tag_id: tag_id.clone(),
            added: *added,
            error: display_error,
        });
    }
}

fn operation_error(kind: &OperationKind, error: &str) -> String {
    match kind {
        OperationKind::Delete { .. } => {
            tr!("runtime-error-delete-message", { error: error }).to_string()
        }
        OperationKind::Move { .. } => {
            tr!("runtime-error-move-message", { error: error }).to_string()
        }
        OperationKind::SetFlag { .. } => tr!("runtime-error-flag", { error: error }).to_string(),
        OperationKind::MarkRead { .. } => {
            tr!("runtime-error-read-state", { error: error }).to_string()
        }
        OperationKind::SetTag { .. } => {
            tr!("runtime-error-update-tag", { error: error }).to_string()
        }
        OperationKind::Send { .. } => error.to_string(),
        OperationKind::QuickAction { .. } => error.to_string(),
    }
}

fn emit_deferred(global: &BgGlobal, operation: &StoredOperation) {
    if let Some(compose_id) = operation.kind.compose_id() {
        global.emit(Evt::OutboxQueued {
            account_id: operation.account_id.clone(),
            operation_id: operation.id,
            compose_id,
        });
    } else if let (Some(message_id), Some(kind)) = (
        operation.kind.message_id(),
        operation.kind.message_mutation_kind(),
    ) {
        global.emit(Evt::MutationDeferred {
            account_id: operation.account_id.clone(),
            operation_id: operation.id,
            message_id: message_id.to_string(),
            kind,
        });
    }
}

fn retry_delay(attempts: u32) -> std::time::Duration {
    let seconds = 5u64.saturating_mul(1u64 << attempts.saturating_sub(1).min(6));
    std::time::Duration::from_secs(seconds.min(300))
}

fn request_error(error: &anyhow::Error) -> Option<&reqwest::Error> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
}

/// Status the provider actually replied with, preferring the one carried as
/// data over anything parsed out of the message.
fn response_status(error: &anyhow::Error) -> Option<u16> {
    crate::providers::status_of(error)
        .or_else(|| request_error(error).and_then(reqwest::Error::status))
        .map(|status| status.as_u16())
}

fn io_error_kinds(error: &anyhow::Error) -> impl Iterator<Item = std::io::ErrorKind> + '_ {
    error.chain().filter_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            // `imap::Error` only exposes its cause through the deprecated
            // `cause()`, which `chain()` does not follow.
            .or_else(|| match cause.downcast_ref::<imap::Error>() {
                Some(imap::Error::Io(io)) => Some(io),
                _ => None,
            })
            .map(std::io::Error::kind)
    })
}

fn smtp_error(error: &anyhow::Error) -> Option<&lettre::transport::smtp::Error> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<lettre::transport::smtp::Error>())
}

/// The connection was never established, so nothing reached the server: safe
/// to replay even a send.
///
/// Limited to failures that can only happen while connecting. A reset, a
/// broken pipe or a timeout can also strike after SMTP's final `.` went out,
/// when the server may already have accepted the message.
fn failed_before_connecting(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    let refused = io_error_kinds(error).any(|kind| {
        matches!(
            kind,
            ErrorKind::ConnectionRefused
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::NetworkDown
                | ErrorKind::AddrNotAvailable
        )
    });
    // lettre only raises its `Connection` kind while resolving and opening
    // the socket, and exposes it through its display text alone.
    refused
        || smtp_error(error).is_some_and(|smtp| smtp.to_string().starts_with("Connection error"))
}

/// A 4xx SMTP reply, at whatever stage: the server declined the transaction
/// and took no responsibility for the message (RFC 5321 §4.2.1 — a 4xx after
/// the final `.` means it was *not* accepted), so replaying cannot duplicate.
fn smtp_transient(error: &anyhow::Error) -> bool {
    smtp_error(error).is_some_and(lettre::transport::smtp::Error::is_transient)
}

/// The connection dropped or timed out midway. Fine to replay a mutation, the
/// same way an HTTP timeout is — never a send.
fn connection_dropped(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    let imap_lost = error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<imap::Error>(),
            Some(imap::Error::Io(_) | imap::Error::ConnectionLost | imap::Error::Bye(_))
        )
    });
    imap_lost
        || smtp_error(error).is_some_and(lettre::transport::smtp::Error::is_timeout)
        || io_error_kinds(error).any(|kind| {
            matches!(
                kind,
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::NotConnected
            )
        })
}

fn send_retry_is_safe(error: &anyhow::Error) -> bool {
    if request_error(error).is_some_and(reqwest::Error::is_connect) {
        return true;
    }
    if failed_before_connecting(error) || smtp_transient(error) {
        return true;
    }
    match response_status(error) {
        // A send that reached the provider is never replayed on its own: a
        // duplicate delivery is worse than a failed one.
        Some(status) => matches!(status, 408 | 429),
        None => error_text_has_status(error, 408) || error_text_has_status(error, 429),
    }
}

fn mutation_is_retryable(error: &anyhow::Error) -> bool {
    if request_error(error).is_some_and(|request| request.is_connect() || request.is_timeout()) {
        return true;
    }
    if failed_before_connecting(error) || smtp_transient(error) || connection_dropped(error) {
        return true;
    }
    match response_status(error) {
        Some(status) => matches!(status, 408 | 429) || (500..600).contains(&status),
        None => {
            error_text_has_status(error, 408)
                || error_text_has_status(error, 429)
                || (500..600).any(|status| error_text_has_status(error, status))
        }
    }
}

/// Last-resort parsing for backends that report failures as plain text — IMAP
/// and SMTP have no HTTP status to carry. Only consulted when nothing in the
/// chain knows the real status, since the message embeds the response body and
/// a body quoting `(500)` would otherwise look like a server error.
fn error_text_has_status(error: &anyhow::Error, status: u16) -> bool {
    let text = error.to_string();
    text.contains(&format!("({status})"))
        || text.contains(&format!("({status}:"))
        || text.contains(&format!("status {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_bounded() {
        assert_eq!(retry_delay(1), std::time::Duration::from_secs(5));
        assert_eq!(retry_delay(20), std::time::Duration::from_secs(300));
    }

    #[test]
    fn status_parser_recognizes_provider_error_shapes() {
        let error = anyhow::anyhow!("provider failed (429): synthetic");
        assert!(error_text_has_status(&error, 429));
    }

    fn provider_error(status: u16, message: &str) -> anyhow::Error {
        crate::providers::error::ProviderError::new(
            reqwest::StatusCode::from_u16(status).expect("valid status"),
            message,
        )
        .into()
    }

    /// The response body is part of the message. Before the status travelled as
    /// data, a body quoting `(500)` made a rejected mutation look retryable.
    #[test]
    fn a_rejection_quoting_a_server_error_is_not_retried() {
        let error = provider_error(403, "graph move failed (403): upstream said (500) earlier");

        assert!(!mutation_is_retryable(&error));
        assert!(!send_retry_is_safe(&error));
    }

    #[test]
    fn throttling_and_server_errors_stay_retryable() {
        assert!(mutation_is_retryable(&provider_error(
            429,
            "graph move failed (429): throttled"
        )));
        assert!(mutation_is_retryable(&provider_error(
            503,
            "graph move failed (503): unavailable"
        )));
        assert!(send_retry_is_safe(&provider_error(
            429,
            "graph send failed (429): throttled"
        )));
    }

    /// A send the provider accepted and then rejected must never replay on its
    /// own — a duplicate delivery is worse than a failed one.
    #[test]
    fn a_server_error_does_not_replay_a_send() {
        assert!(!send_retry_is_safe(&provider_error(
            500,
            "graph send failed (500): internal"
        )));
    }

    fn io_error(kind: std::io::ErrorKind) -> anyhow::Error {
        anyhow::Error::new(std::io::Error::new(kind, "synthetic")).context("imap connect failed")
    }

    #[test]
    fn an_unreachable_server_replays_sends_and_mutations() {
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NetworkUnreachable,
        ] {
            assert!(send_retry_is_safe(&io_error(kind)));
            assert!(mutation_is_retryable(&io_error(kind)));
        }
    }

    /// A reset may have happened after the server accepted the message.
    #[test]
    fn a_dropped_connection_replays_mutations_but_not_sends() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::BrokenPipe,
        ] {
            assert!(mutation_is_retryable(&io_error(kind)));
            assert!(!send_retry_is_safe(&io_error(kind)));
        }
    }

    /// `imap::Error` hides its io cause from `chain()`; it must still count.
    #[test]
    fn imap_connection_errors_are_transient() {
        let lost = anyhow::Error::new(imap::Error::ConnectionLost).context("UID STORE flag");
        assert!(mutation_is_retryable(&lost));
        assert!(!send_retry_is_safe(&lost));

        let refused = anyhow::Error::new(imap::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "synthetic",
        )))
        .context("imap connect failed");
        assert!(send_retry_is_safe(&refused));
    }

    #[test]
    fn protocol_refusals_are_not_retried() {
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "synthetic",
        ));
        assert!(!mutation_is_retryable(&error));
        assert!(!send_retry_is_safe(&error));
    }

    /// IMAP and SMTP have no HTTP status; their text form must still work.
    #[test]
    fn unstructured_backends_fall_back_to_the_message() {
        let error = anyhow::anyhow!("imap append failed (503): try again");
        assert!(mutation_is_retryable(&error));
    }

    /// A store write after a confirmed provider call is retried a bounded
    /// number of times, then gives up with the last error.
    #[tokio::test(start_paused = true)]
    async fn store_writes_after_delivery_are_retried_then_given_up() {
        let calls = std::cell::Cell::new(0);
        let result = retry_store_write(|| {
            calls.set(calls.get() + 1);
            let succeed = calls.get() == 2;
            async move {
                if succeed {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("synthetic"))
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.get(), 2);

        calls.set(0);
        let result = retry_store_write(|| {
            calls.set(calls.get() + 1);
            async { Err(anyhow::anyhow!("synthetic")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), SETTLE_ATTEMPTS);
    }
}
