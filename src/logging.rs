//! Application log shared between console output and the gpui view.
//!
//! `env_logger` retains `RUST_LOG` filtering and terminal formatting;
//! the same entries are kept in a bounded buffer for the UI log.

use chrono::Local;
use log::{Level, Log, Metadata, Record};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_ENTRIES: usize = 2_000;

#[derive(Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: Level,
    pub target: String,
    pub message: String,
}

#[derive(Default)]
struct LogStore {
    entries: Mutex<VecDeque<LogEntry>>,
    generation: AtomicU64,
}

impl LogStore {
    fn push(&self, entry: LogEntry) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() == MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(entry);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

struct AviaryLogger {
    console: env_logger::Logger,
    store: Arc<LogStore>,
}

impl Log for AviaryLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.console.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.console.matches(record) {
            return;
        }

        self.console.log(record);
        self.store.push(LogEntry {
            timestamp: Local::now().format("%H:%M:%S%.3f").to_string(),
            level: record.level(),
            target: record.target().to_string(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {
        self.console.flush();
    }
}

static STORE: OnceLock<Arc<LogStore>> = OnceLock::new();

/// Installs the console and in-memory logger. By default, dependencies are
/// limited to `warn`, while Aviary retains its entries down to `debug`.
pub fn init() {
    let console = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,aviary=debug"),
    )
    .build();
    let max_level = console.filter();
    let store = Arc::new(LogStore::default());
    let _ = STORE.set(store.clone());

    if log::set_boxed_logger(Box::new(AviaryLogger { console, store })).is_ok() {
        log::set_max_level(max_level);
    }
}

pub fn entries() -> Vec<LogEntry> {
    STORE.get().map(|s| s.snapshot()).unwrap_or_default()
}

pub fn generation() -> u64 {
    STORE
        .get()
        .map(|s| s.generation.load(Ordering::Relaxed))
        .unwrap_or_default()
}

pub fn clear() {
    if let Some(store) = STORE.get() {
        store.clear();
    }
}
