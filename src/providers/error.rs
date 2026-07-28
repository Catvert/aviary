//! Provider failures that carry their HTTP status.
//!
//! The retry policy in `runtime::operations` has to tell "the network dropped,
//! replay it" from "the server rejected it, don't". Reading that back out of a
//! formatted message means matching on `(429)` inside a string that also
//! contains the *response body* — a body mentioning `(500)` would then make a
//! non-retryable operation replay, and a provider wording its errors slightly
//! differently would silently stop retrying at all. Keeping the status as data
//! removes the guesswork; the rendered message is unchanged.

use reqwest::StatusCode;
use std::time::Duration;

/// A non-success HTTP response from a provider.
#[derive(Debug)]
pub struct ProviderError {
    /// Status the provider replied with, when the failure came from a response
    /// rather than from the transport.
    pub status: Option<StatusCode>,
    /// Server-directed pause from a `Retry-After` header, when the response
    /// carried one (throttling replies do).
    pub retry_after: Option<Duration>,
    message: String,
}

impl ProviderError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            retry_after: None,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Consumes a failed response into an error that keeps its status.
///
/// `label` names the operation; the rendered text stays
/// `"<label> failed (<status>): <body>"`, the shape the UI already displays.
pub(crate) async fn http_error(resp: reqwest::Response, label: &str) -> anyhow::Error {
    let status = resp.status();
    let retry_after = parse_retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    let mut error = ProviderError::new(status, format!("{label} failed ({status}): {body}"));
    error.retry_after = retry_after;
    error.into()
}

/// Seconds form of `Retry-After` only: Graph and Google both use it, and the
/// HTTP-date form is not worth a date parser here.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Status carried by any `ProviderError` in the chain.
pub(crate) fn status_of(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderError>())
        .and_then(|provider| provider.status)
}

/// Server-directed pause carried by any `ProviderError` in the chain.
pub(crate) fn retry_after_of(error: &anyhow::Error) -> Option<Duration> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderError>())
        .and_then(|provider| provider.retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_survives_added_context() {
        let error: anyhow::Error = ProviderError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "graph list failed (429): slow",
        )
        .into();
        let wrapped = error.context("while refreshing the inbox");

        assert_eq!(status_of(&wrapped), Some(StatusCode::TOO_MANY_REQUESTS));
    }

    /// A body quoting a status must not be mistaken for the response's own.
    #[test]
    fn a_status_quoted_in_the_body_is_not_the_response_status() {
        let error: anyhow::Error = ProviderError::new(
            StatusCode::FORBIDDEN,
            "graph send failed (403): upstream reported (500) earlier",
        )
        .into();

        assert_eq!(status_of(&error), Some(StatusCode::FORBIDDEN));
    }

    #[test]
    fn an_unrelated_error_carries_no_status() {
        assert_eq!(status_of(&anyhow::anyhow!("keyring unavailable")), None);
    }

    #[test]
    fn retry_after_reads_the_seconds_form_and_ignores_the_date_form() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn the_retry_after_survives_added_context() {
        let mut provider = ProviderError::new(StatusCode::TOO_MANY_REQUESTS, "slow down");
        provider.retry_after = Some(Duration::from_secs(7));
        let wrapped = anyhow::Error::from(provider).context("while loading the calendar");

        assert_eq!(retry_after_of(&wrapped), Some(Duration::from_secs(7)));
    }
}
