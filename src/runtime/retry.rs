//! Bounded retry for idempotent provider reads.
//!
//! The durable outbox replays *mutations* with its own policy
//! (`operations.rs`); this helper covers listing and loading calls, which are
//! safe to repeat. Graph in particular answers a busy mailbox with anonymous
//! 504s that succeed seconds later — surfacing those as an error toast (and,
//! for the calendar, months that stay empty) punishes the user for a failure
//! that was about to heal itself.
//!
//! Only failures that name the server as the cause are retried: timeouts,
//! 408/429 and 5xx. A connection failure is *not* — it usually means offline,
//! and the read paths are cache-first precisely so that offline fails fast
//! into the cached view instead of stalling behind sleeps.

use anyhow::Result;
use std::future::Future;
use std::time::Duration;

/// Pauses before the second and third attempt. A 429/503 carrying
/// `Retry-After` sleeps what the server asked instead.
const BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// Longest server-directed pause honored. Beyond this the user is better
/// served by the error (and the calendar's own cooldown) than by a silent
/// half-minute hang holding a mailbox permit.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(15);

/// Runs `attempt` until it succeeds, fails permanently, or exhausts the
/// backoff schedule. Callers keep their mailbox permit across the pauses on
/// purpose: a throttled account should not spend the wait firing other
/// requests.
pub(super) async fn retry_read<T, Fut>(attempt: impl FnMut() -> Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    retry_read_with(&BACKOFF, attempt).await
}

async fn retry_read_with<T, Fut>(
    backoff: &[Duration],
    mut attempt: impl FnMut() -> Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    let mut pauses = backoff.iter();
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(pause) = pauses.next() else {
            return Err(error);
        };
        if !is_transient(&error) {
            return Err(error);
        }
        let pause = match crate::providers::retry_after_of(&error) {
            Some(server) if server > MAX_RETRY_AFTER => return Err(error),
            Some(server) => server,
            None => *pause,
        };
        log::debug!("transient provider failure, retrying in {pause:?}: {error:#}");
        tokio::time::sleep(pause).await;
    }
}

fn is_transient(error: &anyhow::Error) -> bool {
    if error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .is_some_and(reqwest::Error::is_timeout)
    {
        return true;
    }
    match crate::providers::status_of(error) {
        Some(status) => matches!(status.as_u16(), 408 | 429) || status.is_server_error(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::error::ProviderError;
    use reqwest::StatusCode;
    use std::sync::atomic::{AtomicU32, Ordering};

    const NO_PAUSE: [Duration; 2] = [Duration::ZERO, Duration::ZERO];

    fn gateway_timeout() -> anyhow::Error {
        ProviderError::new(StatusCode::GATEWAY_TIMEOUT, "graph calendar failed (504)").into()
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_succeeds() {
        let calls = AtomicU32::new(0);
        let result: Result<u32> = retry_read_with(&NO_PAUSE, || async {
            match calls.fetch_add(1, Ordering::Relaxed) {
                0 => Err(gateway_timeout()),
                n => Ok(n),
            }
        })
        .await;

        assert_eq!(result.unwrap(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn the_schedule_bounds_the_attempts() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_read_with(&NO_PAUSE, || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(gateway_timeout())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn a_rejection_is_not_replayed() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_read_with(&NO_PAUSE, || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(ProviderError::new(StatusCode::NOT_FOUND, "graph open failed (404)").into())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    /// Offline must fail fast into the cached view, not stall behind sleeps.
    #[tokio::test]
    async fn a_plain_error_without_status_is_not_replayed() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_read_with(&NO_PAUSE, || async {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow::anyhow!("connection refused"))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_long_server_directed_pause_surfaces_the_error_instead() {
        let calls = AtomicU32::new(0);
        let result: Result<()> = retry_read_with(&NO_PAUSE, || async {
            calls.fetch_add(1, Ordering::Relaxed);
            let mut error =
                ProviderError::new(StatusCode::TOO_MANY_REQUESTS, "graph list failed (429)");
            error.retry_after = Some(MAX_RETRY_AFTER + Duration::from_secs(1));
            Err(error.into())
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
