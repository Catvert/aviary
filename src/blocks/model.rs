use crate::model::InlineImage;
use serde::{Deserialize, Serialize};

/// Template-only marker removed before sending and used by the editor to
/// position the cursor after insertion.
pub(crate) const TEMPLATE_CURSOR_PLACEHOLDER: &str = "{{cursor}}";

/// One editable building-block of an outgoing message, signature, or event
/// description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Block {
    pub id: u64,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ListItem {
    pub id: u64,
    /// Nesting level — 0 = top of the list, each Tab press increments by one
    /// (capped to `prev_indent + 1` so a row can't out-indent its parent).
    pub indent: u8,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum BlockKind {
    Paragraph(String),
    Heading {
        level: u8,
        text: String,
    },
    Quote(String),
    Code {
        language: String,
        text: String,
    },
    /// Bulleted (`ordered = false`) or numbered (`ordered = true`) list with
    /// per-item indentation. Items are rendered as a vertical sequence of
    /// `TextEdit`s; Tab/Shift+Tab adjust `indent`, Enter splits an item, and
    /// Backspace at offset 0 either dedents or merges with the previous item.
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    /// Editable table. The first row is the header; the editor normalizes all
    /// rows to the same number of columns.
    Table {
        rows: Vec<Vec<String>>,
    },
    Image {
        cid: String,
        /// Display width in CSS pixels. `None` means "intrinsic size, capped
        /// to the editor's default height". Set as soon as the user grabs the
        /// resize handle. The send path emits a raw `<img width="N">` tag
        /// when this is `Some`, so the receiving MUA renders the same size.
        width: Option<u32>,
    },
    Divider,
    /// HTML fragment explicitly supplied by the user (an imported or pasted
    /// signature). It remains opaque to the block editor so inline styles,
    /// tables, and structure survive without a destructive Markdown round trip.
    RawHtml {
        html: String,
    },
    /// The account signature, kept as **one** block instead of being dissolved
    /// into the document.
    ///
    /// Dissolved, it was indistinguishable from what the user had typed: no
    /// way to say which paragraphs were the signature, hence no way to swap it
    /// for another one, and an imported HTML signature showed up as an opaque
    /// "HTML fragment" block with no name on it.
    Signature {
        /// Which signature this came from, so the block can be swapped for
        /// another. `None` once that signature no longer exists.
        signature_id: Option<i64>,
        /// Its name when it was inserted, displayed on the block: a signature
        /// since renamed or deleted must still say what it is.
        name: String,
        /// Rendered once, at insertion. A draft written yesterday must not
        /// silently change because the signature was edited in Preferences
        /// today — the same reason `OriginalMessage` carries its own HTML.
        html: String,
    },
    /// Read-only rendered preview of a quoted or forwarded message. Built
    /// from the original `Message::raw_body` so tables, inline styles and
    /// CID placement survive without a lossy HTML→Markdown→HTML round trip.
    /// The user can delete the block via the row toolbar but cannot edit its
    /// contents inline.
    OriginalMessage {
        html: String,
        inline_images: Vec<InlineImage>,
        /// Stable id (the source message's id) used to namespace the
        /// `bytes://` cache so multiple drafts citing different originals
        /// don't collide on identical CIDs.
        source_id: String,
    },
}
