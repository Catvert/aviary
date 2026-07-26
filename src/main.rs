#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Keep the catalogs as explicit crate inputs: the procedural macro reads them
// at compile time, but Cargo does not otherwise know that they invalidate the
// crate when only a translation changes.
const _: &str = include_str!("../assets/i18n/en.json");
const _: &str = include_str!("../assets/i18n/fr.json");
rust_i18n::i18n!("assets/i18n", fallback = "en");

/// Bytes currently held by live Rust allocations, maintained by
/// [`CountingAllocator`].
pub static LIVE_HEAP_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Global allocator that tracks how much the process actually holds.
///
/// Resident size alone cannot answer "is memory leaking?": a freed allocation
/// stays resident until the allocator returns it to the kernel, and glibc keeps
/// per-thread arenas it is in no hurry to trim. Aviary rasterizes mail bodies
/// into multi-megabyte buffers on short-lived threads, which is exactly the
/// pattern that inflates RSS without anything being retained. Comparing this
/// counter against `VmRSS` separates the two, and costs one relaxed atomic per
/// allocation — noise next to `malloc` itself.
struct CountingAllocator;

unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = unsafe { std::alloc::System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_HEAP_BYTES.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // Delegated rather than left to the default implementation: the tile
        // buffers are zero-initialized and large, and `calloc` can hand back
        // pages the kernel already zeroed.
        let ptr = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE_HEAP_BYTES.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        LIVE_HEAP_BYTES.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { std::alloc::System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE_HEAP_BYTES.fetch_add(new_size, std::sync::atomic::Ordering::Relaxed);
            LIVE_HEAP_BYTES.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Turns a catalog lookup into a [`gpui::SharedString`] without copying when
/// the entry needs no interpolation.
///
/// `rust-i18n` compiles the catalogs into `Cow::Borrowed(&'static str)`, and a
/// `SharedString` can hold that borrow as-is. Producing a `String` — what the
/// `tr!` facade used to do — cost one allocation for the copy and a second for
/// the `Arc` gpui puts behind every label, on every frame that renders it.
pub fn i18n_shared(value: std::borrow::Cow<'static, str>) -> gpui::SharedString {
    match value {
        std::borrow::Cow::Borrowed(text) => gpui::SharedString::new_static(text),
        std::borrow::Cow::Owned(text) => gpui::SharedString::from(text),
    }
}

/// Translation facade used throughout the application.
///
/// Yields a `SharedString`: it derefs to `str`, formats and compares like one,
/// and the UI layer consumes it without a further copy. Call sites that need an
/// owned `String` say so with `.to_string()`.
#[macro_export]
macro_rules! tr {
    ($key:expr) => {
        $crate::i18n_shared(rust_i18n::t!($key))
    };
    ($key:expr, { $($name:ident : $value:expr),* $(,)? }) => {
        $crate::i18n_shared(rust_i18n::t!($key, $($name = $value),*))
    };
}

mod ai;
mod auth;
mod blocks;
mod dictionaries;
mod logging;
mod mailto;
mod model;
mod notify;
mod proofreading;
mod providers;
mod runtime;
mod search_query;
mod single_instance;
#[cfg(target_os = "linux")]
mod tray;
mod ui;

/// Stops glibc from turning the reader's tile buffers into memory it keeps.
///
/// glibc serves allocations above `M_MMAP_THRESHOLD` with `mmap`, and returns
/// them to the kernel on `free`. That threshold is adaptive: each time a
/// mmap'd block is freed, glibc raises it — up to 32 MiB — assuming the size
/// will recur and that recycling it through an arena is cheaper. For a
/// workload of one-off allocations that assumption is exactly wrong. Aviary
/// rasterizes each mail body into buffers of `width x 2048 x 4` bytes (over
/// 10 MiB on a HiDPI display), on a fresh thread per message, so after a few
/// messages the threshold has climbed past them: the buffers start coming from
/// per-thread arenas and stay resident for the life of the process, even
/// though nothing holds them. Resident size then grows with every message
/// opened while the live heap stays flat.
///
/// Pinning the threshold keeps those buffers on the `mmap` path, where `free`
/// really does hand the pages back. `M_TRIM_THRESHOLD` does the same for what
/// the main arena accumulates.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_allocator() {
    // Comfortably below one tile band, comfortably above the small allocations
    // that would suffer from a syscall each.
    const MMAP_THRESHOLD: libc::c_int = 1024 * 1024;
    const TRIM_THRESHOLD: libc::c_int = 4 * 1024 * 1024;
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD);
        libc::mallopt(libc::M_TRIM_THRESHOLD, TRIM_THRESHOLD);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_allocator() {}

fn main() {
    tune_allocator();
    logging::init();

    // Claim the session before anything is opened or written. Aviary is the
    // registered `mailto:` handler, so the desktop starts a fresh process on
    // every click; all but the first must hand their URL to the instance
    // already running and leave, rather than become a second writer of the
    // session file and the two SQLite databases.
    #[cfg(unix)]
    let (external_requests, _socket_guard) =
        match single_instance::acquire(single_instance::ExternalRequest::from_args()) {
            single_instance::Acquisition::HandedOver => return,
            // The guard is bound here, not inside the match arm, so the socket
            // file is unlinked when `main` returns rather than immediately.
            single_instance::Acquisition::Primary {
                requests,
                _listener,
            } => (requests, _listener),
        };
    #[cfg(not(unix))]
    let external_requests = {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        rx
    };

    log::info!("starting Aviary");
    ui::run(external_requests);
}
