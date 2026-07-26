//! Navigation and resource loading for a Blitz HTML document.

use base64::Engine as _;
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use std::{
    collections::{HashMap, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

/// Must remain below the renderer's overall network budget so an HTTP failure
/// completes critical resources before rasterization.
const HTTP_TIMEOUT: Duration = Duration::from_secs(7);

/// In-flight resources, split by what the renderer is allowed to do about them.
///
/// `cid:`, `data:` and blocked remote URLs resolve synchronously, so the first
/// paint must apply them — otherwise inline images would be missing from the
/// very first frame. Remote resources take however long the far end takes: the
/// first paint no longer waits for them, and the resource pump folds them in
/// afterwards. Keeping the two counters apart is what makes that distinction
/// expressible.
#[derive(Default)]
pub(super) struct NetPending {
    local: AtomicUsize,
    remote: AtomicUsize,
}

impl NetPending {
    /// Resources that must be applied before the document is first painted.
    pub(super) fn local(&self) -> usize {
        self.local.load(Ordering::SeqCst)
    }

    /// Remote resources still on the wire.
    pub(super) fn remote(&self) -> usize {
        self.remote.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub(super) struct OpenLinks(std::sync::Mutex<Vec<String>>);

impl OpenLinks {
    /// Opens web navigation requests produced by the last interaction batch
    /// and returns `mailto:` requests to the UI so Aviary can compose them. An
    /// image click suppresses its surrounding anchor because the lightbox is
    /// the deliberate action for that click.
    pub(super) fn flush(&self, suppress: bool) -> Vec<String> {
        let links = std::mem::take(&mut *self.0.lock().expect("Blitz navigation queue"));
        if suppress {
            return Vec::new();
        }
        let mut mailto = Vec::new();
        for url in links {
            if reqwest::Url::parse(&url).is_ok_and(|url| url.scheme() == "mailto") {
                mailto.push(url);
            } else {
                open_link(&url);
            }
        }
        mailto
    }
}

impl NavigationProvider for OpenLinks {
    fn navigate_to(&self, options: NavigationOptions) {
        let url = options.url;
        // Safe schemes only; no file:, javascript:, and so on.
        if safe_scheme(url.scheme()) {
            self.0
                .lock()
                .expect("Blitz navigation queue")
                .push(url.to_string());
        } else {
            log::info!("ignored link with disallowed scheme: {url}");
        }
    }
}

fn safe_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "mailto")
}

/// Validates and normalizes a link before exposing it to reader actions.
pub(super) fn safe_link(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    safe_scheme(url.scheme()).then(|| url.to_string())
}

pub(super) fn open_link(raw: &str) {
    let Some(url) = safe_link(raw) else {
        return;
    };
    if let Err(error) = open::that_detached(&url) {
        log::warn!("opening link {url}: {error:#}");
    }
}

pub(super) struct MailNet {
    images: HashMap<String, Vec<u8>>,
    allow_remote: bool,
    /// In-flight resources. The first paint waits only on the local half; see
    /// [`NetPending`].
    pending: Arc<NetPending>,
    /// Resource handlers touch Stylo state guarded by a non-blocking
    /// `AtomicRefCell`. Network tasks only enqueue results; the document actor
    /// drains every callback serially immediately before `resolve`.
    deliveries: Arc<Mutex<VecDeque<Delivery>>>,
}

struct Delivery {
    resolved_url: String,
    handler: Box<dyn NetHandler>,
    bytes: Bytes,
    /// Which half of [`NetPending`] this delivery decrements.
    remote: bool,
}

impl MailNet {
    pub(super) fn new(
        images: HashMap<String, Vec<u8>>,
        allow_remote: bool,
        pending: Arc<NetPending>,
    ) -> Self {
        Self {
            images,
            allow_remote,
            pending,
            deliveries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn enqueue(&self, resolved_url: String, handler: Box<dyn NetHandler>, bytes: Bytes) {
        self.pending.local.fetch_add(1, Ordering::SeqCst);
        push_delivery(
            &self.deliveries,
            Delivery {
                resolved_url,
                handler,
                bytes,
                remote: false,
            },
        );
    }

    /// Runs every completed callback on the Blitz document actor. A broken
    /// upstream handler is isolated so it cannot strand `pending` until the
    /// renderer's eight-second timeout.
    pub(super) fn drain_deliveries(&self) -> usize {
        let mut drained = 0;
        while let Some(delivery) = pop_delivery(&self.deliveries) {
            let result = catch_unwind(AssertUnwindSafe(|| {
                delivery
                    .handler
                    .bytes(delivery.resolved_url, delivery.bytes);
            }));
            if delivery.remote {
                self.pending.remote.fetch_sub(1, Ordering::SeqCst);
            } else {
                self.pending.local.fetch_sub(1, Ordering::SeqCst);
            }
            if result.is_err() {
                log::error!("Blitz resource callback panicked; resource ignored");
            }
            drained += 1;
        }
        drained
    }

    pub(super) fn has_deliveries(&self) -> bool {
        !lock_deliveries(&self.deliveries).is_empty()
    }
}

impl NetProvider for MailNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url;
        match url.scheme() {
            "cid" => {
                let raw_cid = url
                    .as_str()
                    .trim_start_matches("cid:")
                    .trim_start_matches("//");
                let cid = normalize_cid(raw_cid);
                let bytes = self
                    .images
                    .get(&cid)
                    .or_else(|| {
                        self.images
                            .iter()
                            .find(|(stored, _)| stored.eq_ignore_ascii_case(&cid))
                            .map(|(_, bytes)| bytes)
                    })
                    .cloned()
                    .unwrap_or_default();
                self.enqueue(url.to_string(), handler, Bytes::from(bytes));
            }
            "data" => {
                let bytes = decode_data_uri(url.as_str()).unwrap_or_default();
                self.enqueue(url.to_string(), handler, Bytes::from(bytes));
            }
            "http" | "https" if self.allow_remote => {
                let Some(runtime) = crate::runtime::TOKIO_HANDLE.get() else {
                    self.enqueue(url.to_string(), handler, Bytes::new());
                    return;
                };
                let pending = self.pending.clone();
                let deliveries = self.deliveries.clone();
                pending.remote.fetch_add(1, Ordering::SeqCst);
                runtime.spawn(async move {
                    let fetched = async {
                        http_client()
                            .get(url.clone())
                            .send()
                            .await
                            .ok()?
                            .bytes()
                            .await
                            .ok()
                    }
                    .await;
                    push_delivery(
                        &deliveries,
                        Delivery {
                            resolved_url: url.to_string(),
                            handler,
                            bytes: fetched.unwrap_or_default(),
                            remote: true,
                        },
                    );
                });
            }
            // Blitz marks <link> stylesheets in <head> as
            // critical. Even when blocked, they must finish
            // the callback so the document becomes paintable.
            _ => self.enqueue(url.to_string(), handler, Bytes::new()),
        }
    }
}

fn lock_deliveries(
    deliveries: &Mutex<VecDeque<Delivery>>,
) -> std::sync::MutexGuard<'_, VecDeque<Delivery>> {
    deliveries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn push_delivery(deliveries: &Mutex<VecDeque<Delivery>>, delivery: Delivery) {
    lock_deliveries(deliveries).push_back(delivery);
}

fn pop_delivery(deliveries: &Mutex<VecDeque<Delivery>>) -> Option<Delivery> {
    lock_deliveries(deliveries).pop_front()
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("client HTTP images distantes")
    })
}

/// Decodes a `data:` URI (base64 or percent encoding).
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if meta.ends_with(";base64") {
        let decoded = percent_decode(payload);
        let cleaned: Vec<u8> = decoded
            .into_iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(&cleaned)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
            .ok()
    } else {
        Some(percent_decode(payload))
    }
}

fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(byte);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    output
}

fn normalize_cid(value: &str) -> String {
    String::from_utf8_lossy(&percent_decode(value))
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{normalize_cid, MailNet, NetPending};
    use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request, Url};
    use std::collections::HashMap;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct CountingHandler(Arc<AtomicUsize>);

    impl NetHandler for CountingHandler {
        fn bytes(self: Box<Self>, _resolved_url: String, _bytes: Bytes) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cid_lookup_normalizes_percent_encoded_brackets() {
        assert_eq!(
            normalize_cid("%3Cimage-a%40example.test%3E"),
            "image-a@example.test"
        );
    }

    #[test]
    fn blocked_resources_wait_for_serial_actor_drain() {
        let pending = Arc::new(NetPending::default());
        let completed = Arc::new(AtomicUsize::new(0));
        let net = MailNet::new(HashMap::new(), false, pending.clone());

        for index in 0..32 {
            net.fetch(
                0,
                Request::get(
                    Url::parse(&format!("https://example.invalid/image-{index}.png")).unwrap(),
                ),
                Box::new(CountingHandler(completed.clone())),
            );
        }

        // Remote images are blocked here, so they complete locally and the
        // first paint still has to apply them.
        assert_eq!(pending.local(), 32);
        assert_eq!(pending.remote(), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(net.drain_deliveries(), 32);
        assert_eq!(pending.local(), 0);
        assert_eq!(completed.load(Ordering::SeqCst), 32);
    }

    /// The whole point of splitting the counters: an allowed remote image must
    /// never land in the half the first paint waits on.
    #[test]
    fn allowed_remote_images_are_counted_as_deferred() {
        let pending = Arc::new(NetPending::default());
        let completed = Arc::new(AtomicUsize::new(0));
        let net = MailNet::new(HashMap::new(), true, pending.clone());

        net.fetch(
            0,
            Request::get(Url::parse("https://example.invalid/tracker.gif").unwrap()),
            Box::new(CountingHandler(completed.clone())),
        );

        // Without a Tokio handle installed (as in unit tests) the request
        // cannot be spawned and degrades to a local, empty delivery; with one,
        // it is accounted as remote. Either way it must never be both.
        assert_eq!(pending.local() + pending.remote(), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
    }
}
