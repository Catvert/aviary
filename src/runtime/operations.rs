//! Submission and retry of durable mail operations.

use super::operation_store::{OperationKind, StoredOperation};
use super::{mailbox, send, BgAccount, BgGlobal, Evt, QuickActionExecution, QuickActionStep};
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
    tokio::spawn(async move {
        drain_account(account).await;
    });
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
            for operation in interrupted {
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
    let operations = match account.global.operations.load_due(account.id.clone()).await {
        Ok(operations) => operations,
        Err(error) => {
            log::warn!("loading durable operations failed: {error:#}");
            return;
        }
    };
    for operation in operations {
        execute(account.clone(), operation).await;
    }
    drop(_serial);
    arm_retry_timer(account).await;
}

/// Schedules the next drain on the earliest deadline `handle_failure` wrote to
/// the store.
///
/// The exponential backoff is otherwise inert: nothing else polls the outbox, so
/// a send that failed on a dropped connection would sit there until the user
/// happened to refresh — and never at all with auto-refresh off.
///
/// The boxed return type is what makes this compile: the timer calls
/// `drain_account`, which calls back here, and an `async fn` would leave the
/// compiler chasing an infinitely nested future type.
fn arm_retry_timer(
    account: Arc<BgAccount>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
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

        let mut guard = account.operation_retry.lock().await;
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
            drain_account(waiting).await;
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
        OperationKind::QuickAction { .. } => unreachable!("handled above"),
    };

    match result {
        Ok(()) => {
            if let Err(error) = account.global.operations.remove(operation.id).await {
                log::warn!("removing completed durable operation: {error:#}");
            }
            if let Some(message_id) = operation.kind.message_id() {
                account.emit(Evt::MutationSucceeded {
                    account_id: account.id.clone(),
                    operation_id: operation.id,
                    message_id: message_id.to_string(),
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
            let _ = account.global.operations.remove(operation.id).await;
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
        if let Err(error) = account
            .global
            .operations
            .replace_kind(operation.id, next_kind.clone())
            .await
        {
            // A successful send followed by a failed checkpoint is uncertain:
            // retrying it could deliver a duplicate.
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

async fn perform_quick_action_step(
    account: Arc<BgAccount>,
    execution_id: u64,
    message_id: &str,
    step: QuickActionStep,
) -> anyhow::Result<()> {
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
        QuickActionStep::MarkRead { read } => {
            mailbox::perform_mark_read(account.clone(), message_id.to_string(), read).await?;
            account.emit(Evt::QuickActionMessageState {
                account_id: account.id.clone(),
                message_id: message_id.to_string(),
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
        } => {
            mailbox::perform_move(
                account,
                message_id.to_string(),
                source_folder_id,
                target_folder_id,
            )
            .await
        }
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

fn send_retry_is_safe(error: &anyhow::Error) -> bool {
    if request_error(error).is_some_and(reqwest::Error::is_connect) {
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

    /// IMAP and SMTP have no HTTP status; their text form must still work.
    #[test]
    fn unstructured_backends_fall_back_to_the_message() {
        let error = anyhow::anyhow!("imap append failed (503): try again");
        assert!(mutation_is_retryable(&error));
    }
}
