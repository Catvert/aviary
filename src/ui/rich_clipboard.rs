//! Aviary's internal HTML clipboard.
//!
//! GPUI exposes text and bitmaps but not yet a portable `text/html` MIME entry
//! on Linux. Text therefore goes to the system clipboard, HTML goes into its
//! metadata when the platform retains it, and an in-memory sidecar (HTML plus
//! CID images) supports pasting between Aviary views in the same process.

use crate::model::InlineImage;
use gpui::{App, ClipboardItem};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

const FORMAT: &str = "aviary/html-selection-v1";

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    format: String,
    html: String,
}

#[derive(Clone)]
pub(crate) struct RichContent {
    pub html: String,
    pub images: Vec<InlineImage>,
}

#[derive(Clone)]
struct StoredContent {
    text: String,
    rich: RichContent,
}

fn store() -> &'static Mutex<Option<StoredContent>> {
    static STORE: OnceLock<Mutex<Option<StoredContent>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn write(text: String, html: String, images: Vec<InlineImage>, cx: &App) {
    *store().lock().expect("presse-papiers HTML Aviary") = Some(StoredContent {
        text: text.clone(),
        rich: RichContent {
            html: html.clone(),
            images,
        },
    });
    cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
        text,
        Metadata {
            format: FORMAT.to_string(),
            html,
        },
    ));
}

pub(crate) fn read(item: &ClipboardItem) -> Option<RichContent> {
    let text = item.text()?;
    if let Some(stored) = store()
        .lock()
        .expect("presse-papiers HTML Aviary")
        .as_ref()
        .filter(|stored| stored.text == text)
    {
        return Some(stored.rich.clone());
    }

    let metadata: Metadata = serde_json::from_str(item.metadata()?).ok()?;
    (metadata.format == FORMAT).then_some(RichContent {
        html: metadata.html,
        images: Vec::new(),
    })
}

pub(crate) fn clear() {
    *store().lock().expect("presse-papiers HTML Aviary") = None;
}
