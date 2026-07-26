use anyhow::Result;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const BASE: &str = "https://graph.microsoft.com/v1.0";
const MAX_THROTTLE_RETRIES: u32 = 3;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Authenticated Graph transport shared by every provider operation.
///
/// Graph's Outlook APIs enforce their concurrency budget per app *and*
/// mailbox. Limiting runtime operations is not sufficient because one
/// operation can fan out into several HTTP requests (inline attachments,
/// sender history or category migrations). The semaphore therefore lives
/// here, at the actual request boundary.
pub struct Client<'a> {
    http: &'a reqwest::Client,
    access_token: &'a str,
    request_gate: Option<&'a Arc<Semaphore>>,
}

impl<'a> Client<'a> {
    pub(crate) fn new(
        http: &'a reqwest::Client,
        access_token: &'a str,
        request_gate: &'a Arc<Semaphore>,
    ) -> Self {
        Self {
            http,
            access_token,
            request_gate: Some(request_gate),
        }
    }

    /// Used only while identifying an account, before its per-mailbox
    /// runtime state and semaphore exist.
    pub(crate) fn without_gate(http: &'a reqwest::Client, access_token: &'a str) -> Self {
        Self {
            http,
            access_token,
            request_gate: None,
        }
    }

    fn get(&self, url: impl reqwest::IntoUrl) -> RequestBuilder<'_> {
        self.request(self.http.get(url))
    }

    fn post(&self, url: impl reqwest::IntoUrl) -> RequestBuilder<'_> {
        self.request(self.http.post(url))
    }

    fn patch(&self, url: impl reqwest::IntoUrl) -> RequestBuilder<'_> {
        self.request(self.http.patch(url))
    }

    fn delete(&self, url: impl reqwest::IntoUrl) -> RequestBuilder<'_> {
        self.request(self.http.delete(url))
    }

    fn request(&self, request: reqwest::RequestBuilder) -> RequestBuilder<'_> {
        RequestBuilder {
            client: self,
            request: request.bearer_auth(self.access_token),
        }
    }

    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        let gate = self.request_gate?;
        Some(
            gate.clone()
                .acquire_owned()
                .await
                .expect("Graph request semaphore is never closed"),
        )
    }
}

struct RequestBuilder<'a> {
    client: &'a Client<'a>,
    request: reqwest::RequestBuilder,
}

impl RequestBuilder<'_> {
    fn query<T: Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.request = self.request.query(query);
        self
    }

    fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.request = self.request.json(body);
        self
    }

    fn header(mut self, key: &'static str, value: &'static str) -> Self {
        self.request = self.request.header(key, value);
        self
    }

    async fn send(self) -> reqwest::Result<reqwest::Response> {
        // Streaming bodies cannot be cloned. Graph requests in Aviary are
        // JSON or body-less today, but retain a safe single-attempt fallback.
        if self.request.try_clone().is_none() {
            let _permit = self.client.acquire().await;
            return self.request.send().await;
        }

        for retry in 0..=MAX_THROTTLE_RETRIES {
            let request = self
                .request
                .try_clone()
                .expect("request cloneability was checked above");
            let permit = self.client.acquire().await;
            let response = request.send().await?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS
                || retry == MAX_THROTTLE_RETRIES
            {
                return Ok(response);
            }

            let delay = retry_delay(response.headers().get(RETRY_AFTER), retry);
            log::warn!(
                "Graph request throttled; retry {}/{} in {:.1}s",
                retry + 1,
                MAX_THROTTLE_RETRIES,
                delay.as_secs_f32()
            );
            drop(response);
            drop(permit);
            tokio::time::sleep(delay).await;
        }
        unreachable!("the retry loop always returns on its final attempt")
    }
}

fn retry_delay(retry_after: Option<&reqwest::header::HeaderValue>, retry: u32) -> Duration {
    retry_after
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(1_u64 << retry.min(5)))
        .min(MAX_RETRY_DELAY)
}

#[derive(Deserialize)]
pub(super) struct GraphList<T> {
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GraphRecipient {
    #[serde(rename = "emailAddress")]
    pub email_address: GraphEmailAddress,
}

#[derive(Deserialize)]
pub(super) struct GraphEmailAddress {
    pub name: Option<String>,
    pub address: Option<String>,
}

pub(super) fn from_label(r: Option<GraphRecipient>) -> String {
    let Some(r) = r else {
        return String::new();
    };
    let name = r.email_address.name.unwrap_or_default();
    let addr = r.email_address.address.unwrap_or_default();
    if name.is_empty() {
        addr
    } else if addr.is_empty() {
        name
    } else {
        format!("{name} <{addr}>")
    }
}

pub(super) async fn post_json(
    client: &Client<'_>,
    url: &str,
    payload: &serde_json::Value,
    label: &str,
) -> Result<()> {
    let resp = client.post(url).json(payload).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, &format!("graph {label} failed")).await);
    }
    Ok(())
}

pub(super) async fn patch_json(
    client: &Client<'_>,
    url: &str,
    payload: &serde_json::Value,
    label: &str,
) -> Result<()> {
    let resp = client.patch(url).json(payload).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, &format!("graph {label} failed")).await);
    }
    Ok(())
}

mod accounts;
mod attachments;
mod calendar;
mod folders;
mod messages;
mod people;
mod tags;

pub use accounts::get_me;
pub use attachments::fetch_attachment;
pub use calendar::{
    create_event, delete_event, list_events, move_event, respond_to_invitation, update_event,
};
pub use folders::{create_folder, delete_folder, list_folders, rename_folder};
pub use messages::{
    delete_message, delete_message as delete_draft, fetch_messages_page, find_sent_copy_id,
    get_message, list_folder_messages, list_folder_messages_page, list_from_sender, list_thread,
    mark_read, move_message, note_last_action, save_draft, search, send_mail, send_mail_tracked,
    send_reply, set_flag, sync_folder_messages, OutgoingMessage,
};
pub use people::list_people;
pub use tags::{
    add_tag_to_message, create_tag, delete_tag, list_messages_tagged, list_tags,
    remove_tag_from_message, rename_tag, set_tag_color, PRESET_PALETTE,
};

#[cfg(test)]
mod transport_tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn retry_after_seconds_are_honoured_and_capped() {
        assert_eq!(
            retry_delay(Some(&HeaderValue::from_static("7")), 0),
            Duration::from_secs(7)
        );
        assert_eq!(
            retry_delay(Some(&HeaderValue::from_static("120")), 0),
            MAX_RETRY_DELAY
        );
    }

    #[test]
    fn invalid_retry_after_uses_exponential_fallback() {
        assert_eq!(
            retry_delay(Some(&HeaderValue::from_static("later")), 2),
            Duration::from_secs(4)
        );
        assert_eq!(retry_delay(None, 0), Duration::from_secs(1));
    }
}
