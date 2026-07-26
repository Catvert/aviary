//! Reader faithful mode: renders original HTML with Blitz (Stylo + Taffy +
//! parley), rasterized on the CPU by vello_cpu on a background thread and then
//! displayed as an image in gpui.
//!
//! The Blitz document remains live after rendering (`LiveDoc`): gpui mouse
//! events are relayed to it (`UiEvent`), enabling rich text/image selection
//! (Ctrl+C also retains the HTML fragment and images in Aviary; Escape clears)
//! and clickable links (anchor clicks leave through the `NavigationProvider`
//! to the system browser). The cursor (I-beam or pointer) comes from
//! `get_cursor()` after each event batch.
//!
//! Every interaction passes through an asynchronous pump: events are queued on
//! the UI side (`Entry.pending`) and sent in batches to the document actor
//! thread. `HtmlDocument` is not `Send`, so it is created and destroyed on its
//! own OS thread, communicating through an mpsc channel and oneshots.
//! Re-rasterization occurs only for batches that change selection (click, drag,
//! release); hover costs only a hit test.
//!
//! The document is rebuilt (new `LiveDoc`, losing selection) when available
//! width or its effective email theme changes, measured by a panel-constrained
//! `canvas` probe. Pure zoom changes reuse the live document and its loaded
//! resources, then rerun scale-sensitive layout and full rasterization. The
//! application theme is passed to Stylo by default; users can instead force a
//! browser-like light canvas from Appearance settings.
//! Resources are served by a custom `NetProvider`:
//! - `cid:` from the message's `InlineImage` values;
//! - `data:` decoded locally;
//! - `http(s):` downloaded through the application's Tokio runtime only when
//!   remote images are enabled (the setting is part of the cache key, and
//!   toggling it triggers a new render).
//!
//! Optional font-family and font-size normalization is also part of this key
//! and overrides sender styles.
//!
//! Rasterization is band-based and viewport-driven: the document is laid out
//! in full, but only [`TILE_ROWS`]-row bands around the visible region (probe
//! origin vs window viewport) are painted, each converted RGBA -> BGRA into
//! its own `RenderImage` (which also keeps textures below GPU size limits).
//! Missing bands are requested from the live actor by a band pump as the user
//! scrolls; bands far offscreen are evicted, and a byte budget bounds the
//! whole cache. Preparation (Outlook repairs, cache key, image set) is
//! memoized per location in `PrepCache` because `element()` runs on every
//! view render.
//!
//! This module holds the state all of that shares — `Entry`, the `BlitzCache`
//! it lives in, the cache keys, the `Job` a prepared render is made of, and the
//! command and outcome types crossing the thread boundary. The work itself is
//! split by role:
//!
//! - [`element`] — the gpui element, its entry points, links and zoom;
//! - [`bands`] — which bands to rasterize and when, plus the two pumps;
//! - [`events`] — mouse and keyboard interaction, and copying a selection;
//! - [`actor`] — the document's own OS thread;
//! - [`paint`] — layout and rasterization, on that thread;
//! - [`net`], [`outlook`], [`selection`], [`theme`] — resources, sender
//!   quirks, selection extraction, and stylesheets.

mod actor;
mod bands;
mod element;
mod events;
mod net;
mod outlook;
mod paint;
mod selection;
#[cfg(test)]
mod tests;
mod theme;

pub(crate) use self::element::{
    element, fragment_element, html_element, preview_html_element, preview_html_fragment_element,
};
pub(crate) use outlook::{repair_fragmented_outlook_cids, repair_outlook_html};

use self::events::text_to_html;

#[cfg(test)]
use self::{net::decode_data_uri, selection::selected_html};
use self::{
    net::{open_link, safe_link, MailNet, NetPending, OpenLinks},
    selection::{selected_content, selected_image_nodes},
    theme::{
        adapt_dark_colors, uniform_typography_ua_css, MailTheme, FRAGMENT_UA_CSS, MAIL_UA_CSS,
    },
};
use super::settings::MailBodyOptions;
use super::util;
use crate::model::{BodyFormat, InlineImage, Message};
use anyrender::{render_to_buffer, PaintScene as _};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{node::RasterImageData, util::Color, Document as _, DocumentConfig, FontContext};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, MouseEventButton, MouseEventButtons, Point as EventPoint,
    PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::NavigationProvider;
use blitz_traits::shell::Viewport;
use cursor_icon::CursorIcon;
use fontique::{Blob, GenericFamily};
use futures::channel::oneshot;
use gpui::{
    anchored, canvas, deferred, div, img, point, prelude::*, px, App, Corner, CursorStyle,
    EntityId, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, Pixels, Point, RenderImage, ScrollDelta, ScrollWheelEvent, WeakEntity, Window,
};
use gpui_component::{
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    v_flex, ActiveTheme, StyledExt,
};
use peniko::{kurbo::Rect, Fill};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Tile height in physical pixels. GPU textures are bounded (often 8192 or
/// 16384); 2048 works everywhere.
const TILE_ROWS: u32 = 2048;
/// Maximum physical render height. Bands are rasterized lazily around the
/// viewport, so this only bounds layout bookkeeping (tile table size), not
/// pixel memory; it exists to keep pathological documents in check.
const MAX_PHYS_HEIGHT: u32 = 1_048_576;
/// Number of renders retained in the LRU cache.
const CACHE_CAP: usize = 6;
/// How long a cached render keeps its live document after leaving the screen.
///
/// Measured on a real mailbox, a live document costs around 20 MiB — Stylo's
/// styled tree, Taffy's layout, parley's shaped text and the images Blitz
/// decoded — against roughly 5 MiB for the tiles of the same message. Six of
/// them is most of Aviary's resident growth, and only the one being read is
/// ever used. The delay is what keeps flipping between two messages from
/// rebuilding either of them.
const DOCUMENT_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Lower bound between two sweeps for idle documents. `element()` runs on every
/// frame; the sweep walks the whole cache and has no reason to.
const DOCUMENT_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// Byte budget for materialized tiles across all cache entries. Oldest entries
/// are evicted beyond it (the most recent one is always kept).
const CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
/// Network-resource budget for one render's remote images.
const NET_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the resource pump asks the actor whether late remote resources
/// landed. Short enough that images feel like they load progressively, long
/// enough that an idle poll stays cheap.
const RESOURCE_POLL: Duration = Duration::from_millis(200);

/// Upper bound on how long the resource pump keeps polling after the first
/// paint. Slightly above the per-request HTTP timeout, so a request that dies
/// on the timeout still gets its (empty) delivery applied.
const RESOURCE_PUMP_BUDGET: Duration = Duration::from_secs(10);
/// Email messages have no trustworthy document origin. A valid inert origin
/// still lets Blitz resolve scheme-relative resources (`//host/path`) without
/// panicking on its non-hierarchical default `data:` URL.
const MAIL_DOCUMENT_BASE_URL: &str = "https://example.invalid/";
/// Measurements and zoom can change on every frame or wheel tick. Briefly
/// waiting for the target to stabilize avoids redundant Blitz work.
const RENDER_DEBOUNCE: Duration = Duration::from_millis(75);
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const ZOOM_LINE_STEP: f32 = 0.1;
const ZOOM_BADGE_DURATION: Duration = Duration::from_secs(2);

#[derive(Default)]
struct LinkHandler {
    app: Option<WeakEntity<super::app::AviaryApp>>,
}

impl gpui::Global for LinkHandler {}

pub(crate) fn install_link_handler(app: WeakEntity<super::app::AviaryApp>, cx: &mut App) {
    cx.default_global::<LinkHandler>().app = Some(app);
}

#[derive(Clone, Copy)]
struct RasterScale {
    /// Scale used for Blitz layout and painting.
    render: f64,
    /// Actual window density used to recover logical gpui tile dimensions after
    /// rasterization.
    display: f64,
}

/// Cooperative signal shared by the UI cache and render thread.
///
/// Blitz provides no cancellation primitive for rasterization already inside
/// the engine, but surrounding stages (resource resolution, layout, and tile
/// splitting) can stop quickly. This primarily prevents messages traversed with
/// j/k from accumulating during the possible eight-second remote-resource wait.
#[derive(Default)]
struct RenderCancellation {
    cancelled: AtomicBool,
}

impl RenderCancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(render_cancelled_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

fn render_cancelled_error() -> String {
    tr!("blitz-render-canceled").to_string()
}

fn render_interrupted_error() -> String {
    tr!("blitz-render-interrupted").to_string()
}

fn next_render_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ----------------------------------------------------------------------
// Global render cache
// ----------------------------------------------------------------------

/// One vertical band of the render. Bands are rasterized lazily: `image` is
/// `None` for a band that has not been painted yet (or was evicted after
/// scrolling far away); its logical `height` is always known so layout stays
/// stable.
#[derive(Clone)]
struct Tile {
    image: Option<Arc<RenderImage>>,
    /// Logical height of the band.
    height: f32,
}

type Tiles = Vec<Tile>;
/// Band repaint produced by the actor: (band index, BGRA image, logical height).
type BandTile = (usize, Arc<RenderImage>, f32);
type RenderTarget = (f32, f32, f32);

struct Rendered {
    /// Logical width at which the document was rendered.
    width: f32,
    /// Physical density of the render, to convert logical scroll positions
    /// into band indices.
    display: f32,
    tiles: Tiles,
    truncated: bool,
    /// Remote resources still downloading when these tiles were painted. The
    /// body is displayed without waiting for them; while this holds, the
    /// resource pump keeps asking the actor to fold in what has arrived.
    resources_pending: bool,
}

impl Rendered {
    fn image_bytes(&self) -> usize {
        self.tiles
            .iter()
            .filter_map(|tile| tile.image.as_deref())
            .map(tile_image_bytes)
            .sum()
    }
}

fn tile_image_bytes(image: &RenderImage) -> usize {
    let size = image.size(0);
    size.width.0.max(0) as usize * size.height.0.max(0) as usize * 4
}

/// Handle to the actor thread that owns the non-`Send` `HtmlDocument`. When the
/// final handle is dropped (cache eviction or replacement after a resize), the
/// channel closes and the thread terminates, releasing the document.
struct LiveDoc {
    tx: mpsc::Sender<DocCmd>,
}

/// Commands sent to the document actor thread.
enum DocCmd {
    Batch(Vec<DocOp>, oneshot::Sender<(RenderTarget, BatchOutcome)>),
    SelectedContent(oneshot::Sender<Option<SelectedContent>>),
    /// Rasterizes the requested bands (viewport-driven lazy tiles).
    PaintBands {
        bands: Vec<usize>,
        out: oneshot::Sender<(RenderTarget, Vec<BandTile>)>,
    },
    /// Applies remote resources that arrived after the first paint and, if any
    /// did, re-rasterizes. Answers `None` when nothing landed, so a poll that
    /// finds no news costs one channel round trip and no rasterization.
    ApplyResources {
        visible: (f32, f32),
        out: oneshot::Sender<Option<(RenderTarget, Rendered)>>,
    },
    Rerender {
        logical_width: f32,
        device_scale: f32,
        zoom: f32,
        /// Logical vertical range (relative to the document top) to rasterize
        /// immediately; the band pump fills the rest on scroll.
        visible: (f32, f32),
        cancellation: Arc<RenderCancellation>,
        out: oneshot::Sender<Result<Rendered, String>>,
    },
}

struct SelectedContent {
    text: String,
    html: String,
    images: Vec<InlineImage>,
}

/// Pending operation for the live document.
enum DocOp {
    Ui(UiEvent),
    ClearSelection,
}

struct Entry {
    rendered: Option<Arc<Rendered>>,
    error: Option<String>,
    /// Latest width and physical density for which a render was started.
    /// Deduplicates consecutive frames without reusing a texture blurred by a
    /// display change.
    last_target: Option<RenderTarget>,
    /// Target currently installed in `live`. Unlike `last_target`, this is
    /// updated only after a render is accepted by the UI generation guard.
    live_target: Option<RenderTarget>,
    /// CSS zoom specific to this document. Physical window density remains
    /// separate to keep tiles crisp at every scale.
    zoom: f32,
    /// Transient HUD displayed after wheel zoom. Its generation makes older
    /// hide timers harmless when wheel events arrive in quick succession.
    zoom_badge_visible: bool,
    zoom_badge_generation: u64,
    /// Identifies the latest requested target. An older render may finish after
    /// the newest one; its result must never replace the
    /// bonnes tuiles.
    target_generation: u64,
    in_flight: bool,
    /// Cancels the render or zoom rerender associated with `target_generation`.
    /// Completed documents remain live in the cache.
    render_cancellation: Option<Arc<RenderCancellation>>,
    /// Only the main reader body is replaced during j/k navigation. Editor
    /// quotes and translations use other locations and must not be canceled
    /// with it.
    reader: bool,
    live: Option<Arc<LiveDoc>>,
    /// Events waiting for processing by the pump.
    pending: Vec<DocOp>,
    pump_running: bool,
    /// Missing bands (visible + margin) queued for the band pump.
    pending_bands: Vec<usize>,
    /// Latest desired band range, used to drop stale requests when scrolling
    /// fast (a queued band may already be far from the viewport).
    desired_bands: Option<(usize, usize)>,
    band_pump_running: bool,
    /// Logical vertical range last painted, reused by the resource pump so a
    /// late repaint materializes the bands the reader is actually looking at.
    last_paint_range: Option<(f32, f32)>,
    resource_pump_running: bool,
    /// Content origin in window coordinates at the latest prepaint, used to
    /// convert gpui mouse positions into document coordinates.
    origin: Option<Point<Pixels>>,
    /// Last prepaint of this location. An entry no longer on screen stops being
    /// refreshed, which is what lets the sweep tell a cached render apart from
    /// a displayed one.
    last_seen: Option<Instant>,
    cursor: Option<CursorStyle>,
    /// Body keyboard focus for Ctrl+C and Escape.
    focus: Option<FocusHandle>,
    /// gpui view that built this element. The Blitz cache is a `Global`:
    /// mutating it does not automatically dirty any view.
    owner: Option<EntityId>,
    /// Full bitmap under the pointer. The faithful body itself is one large
    /// raster, so this enables a context menu only over actual DOM images.
    context_image: Option<Arc<RenderImage>>,
    /// Safe external link under the pointer, if any.
    context_link: Option<String>,
    /// `mailto:` clicks waiting to be dispatched on the UI thread.
    pending_mailto: Vec<String>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            rendered: None,
            error: None,
            last_target: None,
            live_target: None,
            zoom: 1.0,
            zoom_badge_visible: false,
            zoom_badge_generation: 0,
            target_generation: 0,
            in_flight: false,
            render_cancellation: None,
            reader: false,
            live: None,
            pending: Vec::new(),
            pump_running: false,
            pending_bands: Vec::new(),
            desired_bands: None,
            band_pump_running: false,
            last_seen: None,
            last_paint_range: None,
            resource_pump_running: false,
            origin: None,
            cursor: None,
            focus: None,
            owner: None,
            context_image: None,
            context_link: None,
            pending_mailto: Vec::new(),
        }
    }
}

impl Entry {
    fn update_zoom(&mut self, zoom: f32) -> Option<u64> {
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return None;
        }
        self.zoom = zoom;
        self.zoom_badge_visible = true;
        self.zoom_badge_generation = self.zoom_badge_generation.wrapping_add(1);
        Some(self.zoom_badge_generation)
    }

    fn hide_zoom_badge(&mut self, generation: u64) -> bool {
        if self.zoom_badge_generation != generation || !self.zoom_badge_visible {
            return false;
        }
        self.zoom_badge_visible = false;
        true
    }

    fn can_rerender_live(&self, width: f32, scale: f32) -> bool {
        self.live.is_some()
            && self.rendered.is_some()
            && self.live_target.is_some_and(|(live_width, live_scale, _)| {
                live_width == width && (live_scale - scale).abs() < 0.001
            })
    }

    fn request_target(&mut self, width: f32, scale: f32, zoom: f32) -> Option<u64> {
        if self
            .last_target
            .is_some_and(|target| render_targets_match(target, (width, scale, zoom)))
        {
            return None;
        }
        if let Some(previous) = self.render_cancellation.take() {
            previous.cancel();
        }
        self.last_target = Some((width, scale, zoom));
        // Globally unique, even if the FIFO entry is evicted and recreated
        // while an older thread finishes (avoids ABA).
        self.target_generation = next_render_generation();
        self.in_flight = true;
        self.error = None;
        self.render_cancellation = Some(Arc::new(RenderCancellation::default()));
        Some(self.target_generation)
    }

    /// Invalidates the generation before signaling its thread: even if it was
    /// already inside the rasterizer's non-interruptible call, its result
    /// can no longer replace current content.
    fn cancel_render(&mut self) -> bool {
        if !self.in_flight {
            return false;
        }
        let cancellation = self.render_cancellation.take();
        self.target_generation = next_render_generation();
        self.last_target = None;
        self.in_flight = false;
        self.error = None;
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
        }
        true
    }
}

fn render_targets_match(a: RenderTarget, b: RenderTarget) -> bool {
    a.0 == b.0 && (a.1 - b.1).abs() < 0.001 && (a.2 - b.2).abs() < 0.001
}

#[derive(Default)]
pub(crate) struct BlitzCache {
    entries: HashMap<String, Entry>,
    /// Least-recently-used first.
    order: VecDeque<String>,
    /// Images released by evictions, waiting for a context with `cx` to call
    /// `cx.drop_image` (the GPU texture is not freed by the `Arc` drop alone).
    orphaned: Vec<Arc<RenderImage>>,
    last_sweep: Option<Instant>,
}

impl gpui::Global for BlitzCache {}

impl BlitzCache {
    fn entry_mut(&mut self, key: &str) -> &mut Entry {
        self.touch(key);
        if !self.entries.contains_key(key) {
            while self.order.len() >= CACHE_CAP {
                self.evict_oldest();
            }
            self.entries.insert(key.to_string(), Entry::default());
            self.order.push_back(key.to_string());
        }
        self.entries.get_mut(key).expect("entry inserted above")
    }

    /// Marks the key as most recently used.
    fn touch(&mut self, key: &str) {
        if let Some(ix) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(ix).expect("index from position");
            self.order.push_back(k);
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(old) = self.order.pop_front() {
            if let Some(mut entry) = self.entries.remove(&old) {
                entry.cancel_render();
                if let Some(rendered) = &entry.rendered {
                    self.orphaned
                        .extend(rendered.tiles.iter().filter_map(|t| t.image.clone()));
                }
                if let Some(image) = entry.context_image.take() {
                    self.orphaned.push(image);
                }
            }
        }
    }

    /// Evicts the oldest entries beyond the tile byte budget; the most recent
    /// entry always survives.
    fn enforce_budget(&mut self) {
        self.enforce_budget_of(CACHE_BUDGET_BYTES);
    }

    fn enforce_budget_of(&mut self, budget: usize) {
        while self.order.len() > 1 && self.total_bytes() > budget {
            self.evict_oldest();
        }
    }

    fn total_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| {
                entry
                    .rendered
                    .as_ref()
                    .map_or(0, |rendered| rendered.image_bytes())
                    + entry.context_image.as_deref().map_or(0, tile_image_bytes)
            })
            .sum()
    }

    fn take_orphaned(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.orphaned)
    }

    /// Releases the live document of every entry off screen for longer than
    /// `idle`, keeping their tiles.
    ///
    /// Dropping the last `Arc<LiveDoc>` closes the actor's channel, so its
    /// thread returns and the whole document goes with it. The tiles survive,
    /// which is what matters on screen: coming back to the message still shows
    /// it immediately while a fresh document renders underneath.
    ///
    /// `last_target` has to be cleared alongside, otherwise `request_target`
    /// would consider the entry already rendered and never rebuild the
    /// document — leaving bands the reader scrolls into as permanent
    /// placeholders, since rasterization is lazy.
    fn release_idle_documents(&mut self, idle: Duration, now: Instant) -> usize {
        let mut released = 0;
        for entry in self.entries.values_mut() {
            if entry.live.is_none() {
                continue;
            }
            let idle_for = entry
                .last_seen
                .map_or(idle, |seen| now.saturating_duration_since(seen));
            if idle_for < idle {
                continue;
            }
            entry.cancel_render();
            entry.live = None;
            entry.last_target = None;
            entry.live_target = None;
            released += 1;
        }
        released
    }

    /// Runs [`Self::release_idle_documents`] at most once per
    /// [`DOCUMENT_SWEEP_INTERVAL`].
    fn sweep_idle_documents(&mut self) {
        let now = Instant::now();
        if self
            .last_sweep
            .is_some_and(|last| now.saturating_duration_since(last) < DOCUMENT_SWEEP_INTERVAL)
        {
            return;
        }
        self.last_sweep = Some(now);
        let released = self.release_idle_documents(DOCUMENT_IDLE_TIMEOUT, now);
        if released > 0 {
            log::debug!("blitz: released {released} idle document(s)");
        }
    }

    /// Cached renders, rasterized tile bytes, and how many documents are still
    /// live on an actor thread.
    ///
    /// A live document is the part the byte budget does *not* see: its tiles
    /// may all have been evicted while Stylo's styled tree, Taffy's layout and
    /// parley's shaped text stay resident so selection and clicks keep working.
    fn stats(&self) -> (usize, usize, usize, usize) {
        (
            self.entries.len(),
            self.total_bytes(),
            self.entries
                .values()
                .filter(|entry| entry.live.is_some())
                .count(),
            self.orphaned.len(),
        )
    }

    fn cancel_pending_readers_except(&mut self, keep: Option<&str>) {
        for (key, entry) in &mut self.entries {
            if entry.reader && keep != Some(key.as_str()) {
                entry.cancel_render();
            }
        }
    }
}

/// Snapshot of what the renderer holds in memory, for the diagnostics report.
///
/// Returns cached renders, tile bytes, live documents, tiles awaiting
/// `cx.drop_image`, memoized preparations and the bytes their HTML and inline
/// images occupy.
pub(crate) fn memory_stats(cx: &mut App) -> (usize, usize, usize, usize, usize, usize) {
    let (entries, tile_bytes, live, orphaned) = cx.default_global::<BlitzCache>().stats();
    let (prep_entries, prep_bytes) = cx.default_global::<PrepCache>().stats();
    (
        entries,
        tile_bytes,
        live,
        orphaned,
        prep_entries,
        prep_bytes,
    )
}

/// Frees GPU textures of evicted tiles. Call from any place holding `cx`
/// after cache mutations.
fn drop_orphaned_images(cx: &mut App) {
    for image in cx.default_global::<BlitzCache>().take_orphaned() {
        cx.drop_image(image, None);
    }
}

/// Immediately stops rendering for a message leaving the reader. Its completed
/// tile cache is untouched, so returning to the message can still reuse it.
pub(crate) fn cancel_pending_reader(cx: &mut App) {
    cx.default_global::<BlitzCache>()
        .cancel_pending_readers_except(None);
}

#[cfg(test)]
fn cache_key(
    instance: &str,
    html: &str,
    images: &[InlineImage],
    options: MailBodyOptions,
    theme: MailTheme,
) -> String {
    cache_key_from_html_hash(
        instance,
        hash_html(html, PrepSource::Html),
        images_signature(images),
        options,
        theme,
    )
}

fn hash_html(html: &str, kind: PrepSource) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut h);
    kind.hash(&mut h);
    h.finish()
}

fn cache_key_from_html_hash(
    instance: &str,
    html_hash: u64,
    images_sig: (usize, usize),
    options: MailBodyOptions,
    theme: MailTheme,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    instance.hash(&mut h);
    html_hash.hash(&mut h);
    images_sig.hash(&mut h);
    options.show_remote_images.hash(&mut h);
    options.force_uniform_font_family.hash(&mut h);
    options.force_uniform_font_size.hash(&mut h);
    if options.force_uniform_font_size {
        options.font_size.to_bits().hash(&mut h);
    }
    theme.hash(&mut h);
    format!("{:016x}", h.finish())
}

// ----------------------------------------------------------------------
// Reader element
// ----------------------------------------------------------------------

struct Job {
    html: Arc<str>,
    /// Message inline images retained with MIME data so they can be
    /// carried during rich HTML copy.
    images: Arc<[InlineImage]>,
    allow_remote: bool,
    force_uniform_font_family: bool,
    force_uniform_font_size: bool,
    uniform_font_size: f32,
    theme: MailTheme,
    /// Whether the root canvas follows the fragment's contents rather than
    /// filling the synthetic browser viewport.
    intrinsic_height: bool,
}

/// Raw input handed to [`prepared_job`].
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum PrepSource {
    /// Already HTML; only the Outlook repairs apply.
    Html,
    /// HTML embedded inside another editor/view and sized to its contents.
    HtmlFragment,
    /// Plain text, wrapped through [`text_to_html`].
    Text,
    /// Plain text embedded inside another view and sized to its contents.
    TextFragment,
}

/// Memoized preparation of one render location. `element()` runs on every view
/// render (each smooth-scroll tick during animations): without this cache each
/// call would re-clone the body, re-parse it through the `scraper`-based
/// Outlook repairs, re-hash it for the render key and deep-copy every inline
/// image. A hit costs a length check plus one `memcmp` of the source. The
/// repaired HTML and images are reference-counted separately from the themed
/// job so a palette switch does not parse or copy them again on the UI thread.
struct PrepEntry {
    source: Arc<str>,
    kind: PrepSource,
    html: Arc<str>,
    html_hash: u64,
    /// Cheap signature of the inline images (count, total byte length): the
    /// render key deliberately ignores image bytes, but a late-arriving image
    /// set must still invalidate the memo.
    images_sig: (usize, usize),
    show_remote: bool,
    force_uniform_font_family: bool,
    force_uniform_font_size: bool,
    font_size_bits: u32,
    theme: MailTheme,
    images: Arc<[InlineImage]>,
    key: String,
    job: Arc<Job>,
}

#[derive(Default)]
struct PrepCache {
    entries: HashMap<String, PrepEntry>,
    order: VecDeque<String>,
}

impl gpui::Global for PrepCache {}

impl PrepCache {
    /// Entries and the bytes their retained source, repaired HTML and inline
    /// images add up to. The cap is a count, so a handful of image-heavy
    /// newsletters is what actually decides this number.
    fn stats(&self) -> (usize, usize) {
        let bytes = self
            .entries
            .values()
            .map(|entry| {
                entry.source.len()
                    + entry.html.len()
                    + entry
                        .images
                        .iter()
                        .map(|image| image.bytes.len())
                        .sum::<usize>()
            })
            .sum();
        (self.entries.len(), bytes)
    }
}

/// Memoized prepared-job cap; entries hold the source and repaired HTML.
const PREP_CAP: usize = 8;

fn images_signature(images: &[InlineImage]) -> (usize, usize) {
    (
        images.len(),
        images.iter().map(|image| image.bytes.len()).sum(),
    )
}

/// Returns the render key and shared [`Job`] for this location, reusing the
/// previous preparation when source and options are unchanged.
fn prepared_job(
    instance: &str,
    source: &str,
    kind: PrepSource,
    images: &[InlineImage],
    options: MailBodyOptions,
    theme: MailTheme,
    cx: &mut App,
) -> (String, Arc<Job>) {
    let images_sig = images_signature(images);
    let font_size_bits = options.font_size.to_bits();
    let (cached_html, cached_images) = {
        let cache = cx.default_global::<PrepCache>();
        if let Some(entry) = cache.entries.get(instance) {
            if entry.kind == kind
                && entry.show_remote == options.show_remote_images
                && entry.force_uniform_font_family == options.force_uniform_font_family
                && entry.force_uniform_font_size == options.force_uniform_font_size
                && entry.font_size_bits == font_size_bits
                && entry.theme == theme
                && entry.images_sig == images_sig
                && entry.source.as_ref() == source
            {
                return (entry.key.clone(), entry.job.clone());
            }

            let same_source = entry.kind == kind && entry.source.as_ref() == source;
            let html =
                same_source.then(|| (entry.source.clone(), entry.html.clone(), entry.html_hash));
            let images =
                (same_source && entry.images_sig == images_sig).then(|| entry.images.clone());
            (html, images)
        } else {
            (None, None)
        }
    };

    let (source, html, html_hash) = match cached_html {
        Some(cached) => cached,
        None => {
            let html: Arc<str> = match kind {
                PrepSource::Html | PrepSource::HtmlFragment => repair_outlook_html(source).into(),
                PrepSource::Text | PrepSource::TextFragment => {
                    repair_outlook_html(&text_to_html(source)).into()
                }
            };
            (Arc::from(source), html.clone(), hash_html(&html, kind))
        }
    };
    let images = cached_images.unwrap_or_else(|| Arc::from(images.to_vec()));
    let key = cache_key_from_html_hash(instance, html_hash, images_sig, options, theme);
    let job = Arc::new(Job {
        html: html.clone(),
        images: images.clone(),
        allow_remote: options.show_remote_images,
        force_uniform_font_family: options.force_uniform_font_family,
        force_uniform_font_size: options.force_uniform_font_size,
        uniform_font_size: options.font_size,
        theme,
        intrinsic_height: matches!(kind, PrepSource::HtmlFragment | PrepSource::TextFragment),
    });

    let cache = cx.default_global::<PrepCache>();
    if !cache.entries.contains_key(instance) {
        while cache.order.len() >= PREP_CAP {
            if let Some(old) = cache.order.pop_front() {
                cache.entries.remove(&old);
            }
        }
        cache.order.push_back(instance.to_string());
    }
    cache.entries.insert(
        instance.to_string(),
        PrepEntry {
            source,
            kind,
            html,
            html_hash,
            images_sig,
            show_remote: options.show_remote_images,
            force_uniform_font_family: options.force_uniform_font_family,
            force_uniform_font_size: options.force_uniform_font_size,
            font_size_bits,
            theme,
            images,
            key: key.clone(),
            job: job.clone(),
        },
    );
    (key, job)
}

enum RenderAttempt {
    Replace(Result<(Rendered, LiveDoc), String>),
    Reuse {
        live: Arc<LiveDoc>,
        result: Result<Rendered, String>,
    },
}

// ----------------------------------------------------------------------
// Mouse/keyboard events -> live document
// ----------------------------------------------------------------------

enum PointerPhase {
    Down,
    Move,
    Up,
}

struct BatchOutcome {
    cursor: CursorStyle,
    /// Bands repainted after a selection change (materialized bands only).
    update: Option<Vec<BandTile>>,
    /// Whether an image was clicked without dragging. This prevents the same
    /// click from activating a link while leaving image actions to right-click.
    clicked_image: bool,
    /// `None` means unchanged; `Some(None)` means the pointer left an image.
    hovered_image_change: Option<Option<Arc<RenderImage>>>,
    /// `None` means unchanged; `Some(None)` means the pointer left a link.
    hovered_link_change: Option<Option<String>>,
    /// `mailto:` navigations emitted by this batch.
    mailto_links: Vec<String>,
}

/// Margin added around the dirty region in CSS pixels, covering line heights
/// and double-click word selections.
const DIRTY_MARGIN_CSS: f32 = 150.0;

/// Actor-thread paint state: tracks the vertical region to repaint (pointer path
/// plus current selection extent) so only affected tiles are rasterized again.
struct PaintState {
    /// Physical height of the latest full render.
    render_h: u32,
    /// Vertical extent in CSS pixels of the current selection since the latest
    /// press, used to erase the old highlight.
    sel_extent: Option<(f32, f32)>,
    /// Region to repaint during the next paint.
    dirty: Option<(f32, f32)>,
    /// Latest vertical pointer position during a drag.
    focus_y: Option<f32>,
    /// Whether a non-empty selection was visible after the previous batch.
    had_visible: bool,
    /// DOM nodes touched at the start and end of a rich drag. They augment
    /// Blitz's text selection to include images.
    rich_anchor: Option<usize>,
    rich_focus: Option<usize>,
    rich_anchor_point: Option<(f32, f32)>,
    rich_focus_point: Option<(f32, f32)>,
    rich_dragged: bool,
    /// Canvas color from the application theme.
    background: Color,
    /// Bands delivered to the UI (initial render, band pump, repaints).
    /// Selection repaints are limited to them: a band the UI never received
    /// (or evicted far offscreen) has nothing stale to refresh.
    materialized: Vec<bool>,
    /// Only the received-message reader opens images. Editor quotes and
    /// composer previews keep their existing link behavior.
    preview_images: bool,
    hovered_image_node: Option<usize>,
    hovered_image: Option<Arc<RenderImage>>,
    hovered_link: Option<String>,
}

impl PaintState {
    fn new(render_h: u32, background: Color) -> Self {
        let bands = render_h.div_ceil(TILE_ROWS).max(1) as usize;
        Self {
            render_h,
            sel_extent: None,
            dirty: None,
            focus_y: None,
            had_visible: false,
            rich_anchor: None,
            rich_focus: None,
            rich_anchor_point: None,
            rich_focus_point: None,
            rich_dragged: false,
            background,
            materialized: vec![true; bands],
            preview_images: false,
            hovered_image_node: None,
            hovered_image: None,
            hovered_link: None,
        }
    }

    /// Aligns the materialized set with the tiles actually delivered.
    fn set_materialized_from(&mut self, tiles: &Tiles) {
        self.materialized = tiles.iter().map(|tile| tile.image.is_some()).collect();
        if self.materialized.is_empty() {
            self.materialized.push(true);
        }
    }

    fn mark_materialized(&mut self, ix: usize) {
        if let Some(slot) = self.materialized.get_mut(ix) {
            *slot = true;
        }
    }

    fn mark_dirty(&mut self, (lo, hi): (f32, f32)) {
        let merged = match self.dirty {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        };
        self.dirty = Some(merged);
    }
}

const LIGHTBOX_MAX_DIMENSION: u32 = 4096;

// ----------------------------------------------------------------------
// Rendering (background thread)
// ----------------------------------------------------------------------

/// First render of a document, plus everything the actor needs to keep serving
/// it: the live document, and the resource plumbing so late remote images can
/// still be applied after the body is already on screen.
struct DocRender {
    rendered: Rendered,
    doc: HtmlDocument,
    render_h: u32,
    provider: Arc<MailNet>,
    pending: Arc<NetPending>,
}
