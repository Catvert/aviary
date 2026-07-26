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

/// A non-success HTTP response from a provider.
#[derive(Debug)]
pub struct ProviderError {
    /// Status the provider replied with, when the failure came from a response
    /// rather than from the transport.
    pub status: Option<StatusCode>,
    message: String,
}

impl ProviderError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
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
    let body = resp.text().await.unwrap_or_default();
    ProviderError::new(status, format!("{label} failed ({status}): {body}")).into()
}

/// Status carried by any `ProviderError` in the chain.
pub(crate) fn status_of(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderError>())
        .and_then(|provider| provider.status)
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
}
