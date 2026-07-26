//! Registers inline images used by block-editor previews.
//!
//! Bytes are stored in an in-memory registry under an `aviary-cid/{hash}` path.
//!
//! gpui image rendering stores the URL as a `SharedUri`, whose conversion to
//! `ImageSource` always produces
//! `Resource::Uri`, never `Resource::Embedded`, so everything is sent
//! to the application's HTTP client without consulting the `AssetSource`.
//! Registry paths are therefore served at two levels:
//! - [`CidHttpClient`], installed by `ui::run`, intercepts `get()` for
//!   `aviary-cid/...` paths;
//! - the `AssetSource` in `ui/mod.rs` also serves them for any `img()` element
//!   built from a non-URI string.

use futures::future::BoxFuture;
use gpui::http_client::{self, AsyncBody, HttpClient, Response, Url};
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

/// Maximum number of images retained in memory (FIFO eviction). gpui keeps its
/// own render cache per path, so eviction affects only bodies not yet displayed.
const CAP: usize = 256;

/// Byte ceiling on the same registry.
///
/// The count alone bounds nothing useful: a photo pasted into a composer can be
/// several megabytes, so 256 entries could mean anything up to a gigabyte. The
/// two caps together are what make the registry's footprint predictable —
/// whichever is reached first evicts.
const CAP_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Registry {
    bytes: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
    total_bytes: usize,
}

impl Registry {
    fn evict_oldest(&mut self) -> bool {
        let Some(old) = self.order.pop_front() else {
            return false;
        };
        if let Some(bytes) = self.bytes.remove(&old) {
            self.total_bytes -= bytes.len();
        }
        true
    }
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Queried by the `AssetSource`: bytes for a registered image, if `path`
/// est un chemin `aviary-cid/…` connu.
pub(crate) fn load(path: &str) -> Option<Vec<u8>> {
    if !path.starts_with("aviary-cid/") {
        return None;
    }
    registry()
        .lock()
        .expect("registre inline_images")
        .bytes
        .get(path)
        .cloned()
}

/// Registered images and the bytes they hold, for the diagnostics report.
pub(crate) fn stats() -> (usize, usize) {
    let reg = registry().lock().expect("registre inline_images");
    (reg.bytes.len(), reg.total_bytes)
}

/// Stable path within this process for an image from a given message.
fn asset_path(message_id: &str, cid: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (message_id, cid).hash(&mut h);
    format!("aviary-cid/{:016x}", h.finish())
}

/// Registers arbitrary bytes under a scope (block editor, etc.) and returns the
/// `aviary-cid/...` path to pass to `img()` or `TextView`.
pub(crate) fn register_bytes(scope: &str, cid: &str, bytes: &[u8]) -> String {
    let path = asset_path(scope, cid);
    let mut reg = registry().lock().expect("registre inline_images");
    if !reg.bytes.contains_key(&path) {
        while reg.order.len() >= CAP && reg.evict_oldest() {}
        // Keep at least the image being registered, however large it is: an
        // editor that cannot show what was just pasted is worse than one over
        // budget for a moment.
        while !reg.order.is_empty() && reg.total_bytes + bytes.len() > CAP_BYTES {
            reg.evict_oldest();
        }
        reg.total_bytes += bytes.len();
        reg.bytes.insert(path.clone(), bytes.to_vec());
        reg.order.push_back(path.clone());
    }
    path
}

/// Application HTTP client: serves `aviary-cid/...` paths from the
/// registry and delegates everything else to gpui's default client.
pub(crate) struct CidHttpClient {
    inner: Arc<dyn HttpClient>,
}

impl CidHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self { inner }
    }
}

impl HttpClient for CidHttpClient {
    fn type_name(&self) -> &'static str {
        "CidHttpClient"
    }

    fn user_agent(&self) -> Option<&http_client::http::HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(
        &self,
        req: http_client::Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.inner.send(req)
    }

    // `get` receives the raw string before parsing as `http::Uri`; this is the
    // only place where an `aviary-cid/...` path, which is not a valid URI, can
    // be intercepted.
    fn get(
        &self,
        uri: &str,
        body: AsyncBody,
        follow_redirects: bool,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        if let Some(bytes) = load(uri) {
            return Box::pin(async move {
                Response::builder()
                    .status(200)
                    .body(AsyncBody::from(bytes))
                    .map_err(anyhow::Error::from)
            });
        }
        self.inner.get(uri, body, follow_redirects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is a process-wide singleton, so the byte cap is exercised
    /// on a local `Registry` rather than through `register_bytes`.
    fn register(reg: &mut Registry, path: &str, len: usize) {
        while reg.order.len() >= CAP && reg.evict_oldest() {}
        while !reg.order.is_empty() && reg.total_bytes + len > CAP_BYTES {
            reg.evict_oldest();
        }
        reg.total_bytes += len;
        reg.bytes.insert(path.to_string(), vec![0; len]);
        reg.order.push_back(path.to_string());
    }

    #[test]
    fn byte_cap_evicts_before_the_count_cap_is_reached() {
        let mut reg = Registry::default();
        let chunk = CAP_BYTES / 4;
        for ix in 0..10 {
            register(&mut reg, &format!("aviary-cid/{ix:04x}"), chunk);
        }

        assert!(reg.order.len() < 10, "large images must evict each other");
        assert!(reg.total_bytes <= CAP_BYTES);
        assert_eq!(
            reg.total_bytes,
            reg.bytes.values().map(Vec::len).sum::<usize>(),
            "the running total must match what is actually held"
        );
    }

    #[test]
    fn an_image_larger_than_the_budget_is_still_registered() {
        let mut reg = Registry::default();
        register(&mut reg, "aviary-cid/small", 1024);
        register(&mut reg, "aviary-cid/huge", CAP_BYTES * 2);

        assert!(reg.bytes.contains_key("aviary-cid/huge"));
        assert!(!reg.bytes.contains_key("aviary-cid/small"));
    }
}
