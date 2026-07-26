//! Memory diagnostics surfaced in the Logs view.
//!
//! Aviary's resident size is dominated by things a stack trace never shows: the
//! reader's rasterized tiles, the Blitz documents kept live behind them, and the
//! message bodies the mail view holds so a click never waits on the provider.
//! Each of those is capped, but by different units — a count here, a byte
//! budget there — so "is it growing or is it just full?" is not a question the
//! code can be read for. This module answers it with numbers.
//!
//! `AviaryApp` logs a [`Report`] periodically (see `MEMORY_REPORT_INTERVAL`),
//! so reproducing a memory concern only takes using the app and reading the
//! Logs tab afterwards.

use super::app::AviaryApp;
use super::state::ThreadBodyState;
use crate::model::Message;
use gpui::App;

/// Resident set size of the process, in bytes.
///
/// Read from `VmRSS` in `/proc/self/status`, which is already in kB — no page
/// size to look up, so no libc dependency. This is what a system monitor
/// reports, and therefore the number a user comparing against their task
/// manager expects: it includes memory Rust has freed but the allocator has not
/// returned to the kernel.
#[cfg(target_os = "linux")]
pub(crate) fn process_rss_bytes() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .strip_prefix("VmRSS:")?;
    let kib: usize = line.split_whitespace().next()?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_rss_bytes() -> Option<usize> {
    None
}

/// Splits resident memory into its anonymous and file-backed halves, in bytes.
///
/// This is the measurement that says where non-heap growth comes from.
/// Anonymous pages are the heap, thread stacks and GPU mappings; file-backed
/// pages are executables, shared libraries and — the interesting one here —
/// font files, which fontique memory-maps and its shared `SourceCache` never
/// releases. A counting allocator cannot see either.
#[cfg(target_os = "linux")]
fn resident_split() -> Option<(usize, usize)> {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    let field = |name: &str| -> Option<usize> {
        rollup
            .lines()
            .find(|line| line.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<usize>()
            .ok()
            .map(|kib| kib * 1024)
    };
    let rss = field("Rss:")?;
    let anonymous = field("Anonymous:")?;
    Some((anonymous, rss.saturating_sub(anonymous)))
}

#[cfg(not(target_os = "linux"))]
fn resident_split() -> Option<(usize, usize)> {
    None
}

/// Bytes of font files currently mapped into the address space.
///
/// Every Blitz document clones a `FontContext`; the clones share one
/// `SourceCache`, so a face is loaded once — but never unloaded. A mail asking
/// for a system family fontconfig resolves to a large face (CJK ones run to
/// tens of megabytes) leaves it mapped for the life of the process.
#[cfg(target_os = "linux")]
fn mapped_font_bytes() -> Option<usize> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut total = 0;
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let range = fields.next()?;
        let path = match line.split_whitespace().nth(5) {
            Some(path) if path.starts_with('/') => path,
            _ => continue,
        };
        let is_font = [".ttf", ".otf", ".ttc", ".pfb", ".woff", ".woff2"]
            .iter()
            .any(|ext| path.to_ascii_lowercase().ends_with(ext));
        if !is_font || !seen.insert(path) {
            continue;
        }
        let (start, end) = range.split_once('-')?;
        let start = usize::from_str_radix(start, 16).ok()?;
        let end = usize::from_str_radix(end, 16).ok()?;
        total += end.saturating_sub(start);
    }
    Some(total)
}

#[cfg(not(target_os = "linux"))]
fn mapped_font_bytes() -> Option<usize> {
    None
}

/// Number of OS threads in the process.
///
/// The reader gives every live Blitz document its own thread, so this is how a
/// document that outlives its cache entry shows up: the count climbs with the
/// number of messages opened instead of settling near the cache cap.
#[cfg(target_os = "linux")]
fn thread_count() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with("Threads:"))?
        .strip_prefix("Threads:")?
        .trim()
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> Option<usize> {
    None
}

/// Bytes a message occupies beyond its metadata: body, original payload, inline
/// images and any attachment already downloaded.
///
/// Attachment bytes are the surprising term — a received attachment fetched
/// once stays on its `Message`, so a mail with a slide deck is heavier in the
/// reader than its text ever suggests.
fn message_bytes(message: &Message) -> usize {
    message.body.len()
        + message.raw_body.as_ref().map_or(0, String::len)
        + message
            .inline_images
            .iter()
            .map(|image| image.bytes.len())
            .sum::<usize>()
        + message
            .attachments
            .iter()
            .filter_map(|attachment| attachment.bytes.as_ref())
            .map(Vec::len)
            .sum::<usize>()
}

/// One snapshot of where Aviary's memory sits.
pub(crate) struct Report {
    rss: Option<usize>,
    /// Bytes held by live Rust allocations. The gap between this and `rss` is
    /// what the allocator is keeping rather than what Aviary is holding.
    live_heap: usize,
    /// Resident memory split into anonymous and file-backed pages.
    resident_split: Option<(usize, usize)>,
    /// Font files mapped into the address space.
    font_bytes: Option<usize>,
    threads: Option<usize>,
    /// Reader renders: cached entries, rasterized tile bytes, documents still
    /// live on an actor thread, tiles awaiting `cx.drop_image`.
    blitz: (usize, usize, usize, usize),
    /// Memoized render preparations: entries and retained bytes.
    prep: (usize, usize),
    /// Block-editor image registry: entries and bytes.
    inline_images: (usize, usize),
    /// Message headers held by the list, and search results on top of them.
    headers: usize,
    search_results: usize,
    /// Bodies the reader holds: selection, pinned tabs, expanded thread
    /// messages — with the bytes behind them.
    bodies: usize,
    body_bytes: usize,
    /// Reply/forward snapshots accumulated this session.
    sent_snapshots: usize,
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

impl Report {
    pub(crate) fn collect(app: &AviaryApp, cx: &mut App) -> Self {
        let (entries, tile_bytes, live, orphaned, prep_entries, prep_bytes) =
            super::blitz_body::memory_stats(cx);

        let mut bodies = 0;
        let mut body_bytes = 0;
        let mut account = |message: &Message| {
            bodies += 1;
            body_bytes += message_bytes(message);
        };
        if let Some(selected) = app.mailbox.selected.as_deref() {
            account(selected);
        }
        for tab in &app.mailbox.open_tabs {
            if let Some(message) = tab.message() {
                account(message);
            }
        }
        for state in app.mailbox.thread_bodies.values() {
            if let ThreadBodyState::Loaded(message) = state {
                account(message);
            }
        }

        Self {
            rss: process_rss_bytes(),
            live_heap: crate::LIVE_HEAP_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            resident_split: resident_split(),
            font_bytes: mapped_font_bytes(),
            threads: thread_count(),
            blitz: (entries, tile_bytes, live, orphaned),
            prep: (prep_entries, prep_bytes),
            inline_images: super::inline_images::stats(),
            headers: app.mailbox.messages.len(),
            search_results: app.mailbox.search.results.as_ref().map_or(0, Vec::len),
            bodies,
            body_bytes,
            sent_snapshots: app.mailbox.sent_messages.values().map(Vec::len).sum(),
        }
    }

    /// One log line per snapshot, so successive reports can be compared by eye
    /// in the Logs tab.
    pub(crate) fn log(&self) {
        let (entries, tile_bytes, live, orphaned) = self.blitz;
        let (prep_entries, prep_bytes) = self.prep;
        let (image_entries, image_bytes) = self.inline_images;
        let rss = match self.rss {
            Some(bytes) => format!("{:.0} MiB", mib(bytes)),
            None => "n/a".to_string(),
        };
        let threads = match self.threads {
            Some(count) => count.to_string(),
            None => "n/a".to_string(),
        };
        let split = match self.resident_split {
            Some((anonymous, file)) => {
                format!("anon={:.0} MiB file={:.0} MiB", mib(anonymous), mib(file))
            }
            None => "anon=n/a file=n/a".to_string(),
        };
        let fonts = match self.font_bytes {
            Some(bytes) => format!("{:.0} MiB", mib(bytes)),
            None => "n/a".to_string(),
        };
        log::info!(
            "memory: rss={rss} heap={:.0} MiB {split} fonts={fonts} threads={threads} | \
             blitz={entries} renders, \
             {:.1} MiB tiles, {live} live docs, {orphaned} orphaned | \
             prep={prep_entries} entries, {:.1} MiB | \
             editor-images={image_entries}, {:.1} MiB | headers={}(+{} results) | \
             bodies={}, {:.1} MiB | sent-snapshots={}",
            mib(self.live_heap),
            mib(tile_bytes),
            mib(prep_bytes),
            mib(image_bytes),
            self.headers,
            self.search_results,
            self.bodies,
            mib(self.body_bytes),
            self.sent_snapshots,
        );
    }
}
