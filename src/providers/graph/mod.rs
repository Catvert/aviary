use anyhow::Result;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const BASE: &str = "https://graph.microsoft.com/v1.0";
const MAX_THROTTLE_RETRIES: u32 = 3;

/// Longest server-directed pause served by retrying inline. Beyond this the
/// 429 is returned to the caller — the account-wide cooldown paces every
/// later request, and the runtime reschedules the read itself. Must match
/// `runtime::retry::MAX_RETRY_AFTER`, or one layer sleeps out a pause the
/// other refuses (locked by a test there).
pub(crate) const MAX_INLINE_RETRY_AFTER: Duration = Duration::from_secs(15);

/// Upper bound on the account-wide cooldown a single `Retry-After` may
/// impose: a malformed or hostile header must not silence an account for an
/// afternoon. The runtime applies the same cap to the pause it reports to
/// the UI and to its rescheduled sync.
pub(crate) const MAX_COOLDOWN: Duration = Duration::from_secs(300);

/// Per-account request gate: the concurrency permits plus the cooldown a 429
/// imposes on the whole mailbox. It lives at the transport so every HTTP
/// attempt — including fan-outs hidden inside one provider operation — both
/// honors and feeds it; without the shared cooldown, each concurrent task
/// would only learn about the throttle by burning a request into it.
pub struct RequestGate {
    permits: Arc<Semaphore>,
    cooldown_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl RequestGate {
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            cooldown_until: std::sync::Mutex::new(None),
        }
    }

    /// Records a server-directed pause. Extends the current cooldown, never
    /// shortens it: two 429s in flight must not let the smaller pause win.
    fn set_cooldown(&self, pause: Duration) {
        let target = tokio::time::Instant::now() + pause.min(MAX_COOLDOWN);
        let mut slot = self.cooldown_until.lock().expect("cooldown lock poisoned");
        if slot.is_none_or(|current| current < target) {
            *slot = Some(target);
        }
    }

    /// Sleeps until the cooldown has elapsed. Re-reads the deadline after
    /// each sleep — another request may have received a fresh 429 meanwhile.
    async fn wait_cooldown(&self) {
        loop {
            let deadline = *self.cooldown_until.lock().expect("cooldown lock poisoned");
            match deadline {
                Some(deadline) if deadline > tokio::time::Instant::now() => {
                    tokio::time::sleep_until(deadline).await;
                }
                _ => return,
            }
        }
    }
}

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
    request_gate: Option<&'a Arc<RequestGate>>,
}

impl<'a> Client<'a> {
    pub(crate) fn new(
        http: &'a reqwest::Client,
        access_token: &'a str,
        request_gate: &'a Arc<RequestGate>,
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
        let permit = gate
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("Graph request semaphore is never closed");
        // Waiting *with* the permit is deliberate: during a cooldown no
        // request may go out anyway, so releasing it would only let more
        // tasks pile up at the wall.
        gate.wait_cooldown().await;
        Some(permit)
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
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Ok(response);
            }

            let delay = retry_delay(response.headers().get(RETRY_AFTER), retry);
            if let Some(gate) = self.client.request_gate {
                // The whole account backs off, not just this request: Graph
                // throttles per mailbox, so every sibling request would only
                // deepen the penalty.
                gate.set_cooldown(delay);
            }
            if retry == MAX_THROTTLE_RETRIES || delay > MAX_INLINE_RETRY_AFTER {
                // Return the 429 as-is: `http_error` preserves its
                // `Retry-After`, the caller decides (and the cooldown above
                // already paces whatever runs next).
                return Ok(response);
            }
            log::warn!(
                "Graph request throttled; retry {}/{} in {:.1}s",
                retry + 1,
                MAX_THROTTLE_RETRIES,
                delay.as_secs_f32()
            );
            drop(response);
            drop(permit);
            if self.client.request_gate.is_none() {
                // With a gate, the next `acquire` sleeps the cooldown out.
                tokio::time::sleep(delay).await;
            }
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
    fn retry_after_seconds_are_honoured() {
        assert_eq!(
            retry_delay(Some(&HeaderValue::from_static("7")), 0),
            Duration::from_secs(7)
        );
        // No cap here: a long pause is returned as-is so the send loop can
        // decide to give up instead of sleeping it out inline.
        assert_eq!(
            retry_delay(Some(&HeaderValue::from_static("120")), 0),
            Duration::from_secs(120)
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

    #[tokio::test(start_paused = true)]
    async fn the_cooldown_extends_but_never_shrinks_and_is_capped() {
        let gate = RequestGate::new(3);
        gate.set_cooldown(Duration::from_secs(10));
        gate.set_cooldown(Duration::from_secs(2));
        let deadline = gate
            .cooldown_until
            .lock()
            .unwrap()
            .expect("a cooldown was recorded");
        assert_eq!(
            deadline - tokio::time::Instant::now(),
            Duration::from_secs(10)
        );
        gate.set_cooldown(Duration::from_secs(3_600));
        let deadline = gate.cooldown_until.lock().unwrap().unwrap();
        assert_eq!(deadline - tokio::time::Instant::now(), MAX_COOLDOWN);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_out_the_cooldown_observes_extensions() {
        let gate = Arc::new(RequestGate::new(1));
        gate.set_cooldown(Duration::from_secs(5));
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.wait_cooldown().await;
                tokio::time::Instant::now()
            }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        gate.set_cooldown(Duration::from_secs(8));
        let start = tokio::time::Instant::now() - Duration::from_secs(2);
        let released = waiter.await.unwrap();
        assert_eq!(released - start, Duration::from_secs(10));
    }
}
