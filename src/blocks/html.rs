//! Structural HTML → block-document import.
//!
//! Reopening a draft has to go the other way round than everything else in
//! this module: the provider hands back HTML, and the editor needs blocks.
//! The obvious path — reuse the reader's HTML→Markdown conversion and feed
//! [`markdown_to_blocks`](super::markdown_to_blocks) — loses exactly what the
//! WYSIWYG editor is for. That converter is tuned for *reading* mail: it
//! unwraps layout tables, drops the class names Aviary writes on its own
//! structures, and rewrites `cid:` sources to the reader's `bytes://` scheme.
//! A draft written here came back as a wall of paragraphs with its signature
//! dissolved into them.
//!
//! So this walks the DOM itself. Two rules shape it:
//!
//! - **Structure the editor can hold becomes a real block** — headings,
//!   lists, quotes, code, tables, dividers, inline images — and inline marks
//!   (bold, italic, strike, underline, links, code spans) are written back as
//!   the Markdown the block model stores in its text. Presentational
//!   containers (`<div>` wrappers, `<span>`s, the outer font wrapper
//!   `build_html_body` emits) are transparent.
//! - **What it cannot hold stays opaque rather than being flattened.** A
//!   signature comes back as [`BlockKind::Signature`], a quoted original as
//!   [`BlockKind::OriginalMessage`], a table whose cells hold their own
//!   structure as [`BlockKind::RawHtml`]. Each of those renders faithfully in
//!   the editor, so nothing is silently degraded on a round trip.
//!
//! Aviary's own markers are recognised first (`div.aviary-signature`,
//! `div.aviary-original-message`), then the two other clients' conventions a
//! draft is likely to arrive with — Gmail's `gmail_signature`/`gmail_quote`
//! and Outlook Web's `<div id="Signature">`.
//!
//! Two things are deliberately not attempted. Text is imported verbatim, not
//! Markdown-escaped: the editor displays its own syntax with dimmed
//! delimiters, so escaping would put visible backslashes in front of every
//! `*` a user typed, and CommonMark's flanking rules already keep isolated
//! punctuation inert. And CSS beyond the handful of properties that map onto
//! an inline mark (weight, style, decoration) is dropped — a block document
//! has nowhere to put it.

use std::sync::OnceLock;

use scraper::{ElementRef, Html, Node, Selector};

use super::markdown::referenced_inline_images;
use super::model::{BlockKind, ListItem};
use crate::model::InlineImage;

/// Headings above this level have no representation in the editor.
const MAX_HEADING_LEVEL: u8 = 3;

/// Parses a literal CSS selector once, at its call site.
macro_rules! selector {
    ($css:literal) => {{
        static SELECTOR: OnceLock<Selector> = OnceLock::new();
        SELECTOR.get_or_init(|| Selector::parse($css).expect("static selector"))
    }};
}

/// What the message carrying the HTML lends to the blocks rebuilt from it.
pub(crate) struct HtmlImport<'a> {
    /// Inline parts of that message. A faithful sub-block keeps the ones its
    /// own `cid:` sources reference, so it still renders offline.
    pub inline_images: &'a [InlineImage],
    /// Namespaces the `bytes://` preview cache of faithful sub-blocks: the id
    /// of the message the HTML was read from.
    pub source_id: &'a str,
}

#[cfg(test)]
impl HtmlImport<'_> {
    /// For HTML with no message behind it.
    pub(crate) fn bare() -> HtmlImport<'static> {
        HtmlImport {
            inline_images: &[],
            source_id: "",
        }
    }
}

/// Rebuild an editable block document from an HTML body.
pub(crate) fn html_to_blocks(html: &str, import: &HtmlImport<'_>) -> Vec<BlockKind> {
    let document = Html::parse_document(html);
    let Some(body) = document.select(selector!("body")).next() else {
        return Vec::new();
    };
    let mut walker = Walker::new(import);
    walker.walk_children(body);
    walker.flush();
    collapse_blank_paragraphs(walker.out)
}

/// Visible text of an HTML fragment, whitespace-collapsed.
///
/// Used to recognise a signature whose identifying attribute a provider
/// stripped: comparing rendered markup would fail on any reformatting, while
/// the words are what the user would recognise as "the same signature".
pub(crate) fn html_text(html: &str) -> String {
    let document = Html::parse_document(html);
    let Some(body) = document.select(selector!("body")).next() else {
        return String::new();
    };
    // Joined rather than concatenated: HTML puts no character between two
    // blocks, so `<p>a</p><p>b</p>` would otherwise read as one word.
    collapse_whitespace(&body.text().collect::<Vec<_>>().join(" "))
        .trim()
        .to_string()
}

struct Walker<'a> {
    out: Vec<BlockKind>,
    /// Inline text seen since the last block was emitted. Mail HTML mixes
    /// bare text with block elements at the same level, so it is buffered
    /// here and flushed as a paragraph whenever a block interrupts it.
    pending: String,
    import: &'a HtmlImport<'a>,
}

impl<'a> Walker<'a> {
    fn new(import: &'a HtmlImport<'a>) -> Self {
        Self {
            out: Vec::new(),
            pending: String::new(),
            import,
        }
    }

    fn flush(&mut self) {
        let text = self.pending.trim().to_string();
        self.pending.clear();
        if !text.is_empty() {
            self.out.push(BlockKind::Paragraph(text));
        }
    }

    fn push_block(&mut self, kind: BlockKind) {
        self.flush();
        self.out.push(kind);
    }

    fn walk_children(&mut self, element: ElementRef<'_>) {
        let mut children = element.children();
        while let Some(child) = children.next() {
            match child.value() {
                Node::Text(text) => self.pending.push_str(&collapse_whitespace(text)),
                Node::Element(_) => {
                    let Some(child) = ElementRef::wrap(child) else {
                        continue;
                    };
                    // Outlook Web does not wrap what it quotes: it drops the
                    // attribution headers in one div and leaves the original
                    // message as its *siblings*. Everything from there on is
                    // the quote, and it stays faithful as a whole.
                    if opens_quoted_tail(child) {
                        let mut html = child.html();
                        for rest in children.by_ref() {
                            match rest.value() {
                                Node::Text(text) => html.push_str(text),
                                Node::Element(_) => {
                                    if let Some(rest) = ElementRef::wrap(rest) {
                                        html.push_str(&rest.html());
                                    }
                                }
                                _ => {}
                            }
                        }
                        let quote = self.quote_block(html);
                        self.push_block(quote);
                        return;
                    }
                    self.walk_element(child);
                }
                _ => {}
            }
        }
    }

    fn walk_element(&mut self, element: ElementRef<'_>) {
        if let Some(kind) = self.faithful_block(element) {
            self.push_block(kind);
            return;
        }
        let name = element.value().name();
        match name {
            "script" | "style" | "head" | "meta" | "link" | "title" | "noscript" => {}
            "br" => self.pending.push('\n'),
            "hr" => self.push_block(BlockKind::Divider),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name
                    .strip_prefix('h')
                    .and_then(|level| level.parse::<u8>().ok())
                    .unwrap_or(1)
                    .clamp(1, MAX_HEADING_LEVEL);
                let text = inline_children(element).trim().to_string();
                if !text.is_empty() {
                    self.push_block(BlockKind::Heading { level, text });
                }
            }
            "p" => self.push_paragraph(element),
            "blockquote" => {
                let text = self.quote_text(element);
                if !text.trim().is_empty() {
                    self.push_block(BlockKind::Quote(text));
                }
            }
            "pre" => self.push_block(code_block(element)),
            "ul" | "ol" => {
                if let Some(list) = list_block(element, name == "ol") {
                    self.push_block(list);
                }
            }
            "table" => self.push_table(element),
            "img" => match image_block(element) {
                Some(image) => self.push_block(image),
                None => self.pending.push_str(&image_markdown(element)),
            },
            _ if is_container(name) => {
                if has_block_child(element) {
                    self.walk_children(element);
                } else {
                    self.push_paragraph(element);
                }
            }
            _ => {
                let mut inline = String::new();
                push_inline(element, &mut inline);
                self.pending.push_str(&inline);
            }
        }
    }

    /// A paragraph, an image standing on its own, or the blank line an empty
    /// `<p>`/`<div>` stands for.
    fn push_paragraph(&mut self, element: ElementRef<'_>) {
        if let Some(image) = sole_image_block(element) {
            self.push_block(image);
            return;
        }
        let text = styled_inline(element);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            let name = element.value().name();
            if matches!(name, "p" | "div")
                && element.select(selector!("img, table, hr")).count() == 0
            {
                self.push_block(BlockKind::Paragraph(String::new()));
            }
            return;
        }
        self.push_block(BlockKind::Paragraph(trimmed.to_string()));
    }

    fn push_table(&mut self, element: ElementRef<'_>) {
        // A single-cell table with no header is a wrapper, not data: many
        // clients use one to constrain the width of the whole message. Its
        // content belongs in the document, not inside a one-cell grid.
        if !is_data_table(element) {
            if let Some(cell) = sole_cell(element) {
                self.flush();
                self.walk_children(cell);
                return;
            }
        }
        match table_block(element) {
            Some(table) => self.push_block(table),
            // Cells with their own structure (nested tables, images, lists)
            // would lose it in the editor's flat grid. Keep the markup.
            None => self.push_block(BlockKind::RawHtml {
                html: element.html(),
            }),
        }
    }

    /// A quote is one text in the block model, so the nested blocks are
    /// reduced to their lines.
    fn quote_text(&self, element: ElementRef<'_>) -> String {
        let mut inner = Walker::new(self.import);
        inner.walk_children(element);
        inner.flush();
        inner
            .out
            .iter()
            .filter_map(|kind| match kind {
                BlockKind::Paragraph(text) | BlockKind::Quote(text) => Some(text.clone()),
                BlockKind::Heading { text, .. } => Some(text.clone()),
                BlockKind::Code { text, .. } => Some(text.clone()),
                BlockKind::List { items, .. } => Some(
                    items
                        .iter()
                        .map(|item| item.text.clone())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    /// Containers whose content must survive as markup rather than as blocks.
    fn faithful_block(&self, element: ElementRef<'_>) -> Option<BlockKind> {
        let value = element.value();
        let class = value.attr("class").unwrap_or_default().to_ascii_lowercase();
        let id = value.attr("id").unwrap_or_default();
        let is_signature = class.contains("aviary-signature")
            || class.contains("gmail_signature")
            || value
                .attr("data-smartmail")
                .is_some_and(|value| value.eq_ignore_ascii_case("gmail_signature"))
            || id.eq_ignore_ascii_case("signature");
        if is_signature {
            let html = element.inner_html();
            if html.trim().is_empty() {
                return None;
            }
            return Some(BlockKind::Signature {
                // The name never leaves the machine, so only the id travels;
                // the caller turns it back into a name it can display.
                signature_id: value
                    .attr("data-aviary-signature-id")
                    .and_then(|id| id.trim().parse().ok()),
                name: String::new(),
                html,
            });
        }
        let is_quote = class.contains("aviary-original-message")
            || class.contains("gmail_quote")
            || (value.name() == "blockquote"
                && value
                    .attr("type")
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("cite")));
        if is_quote {
            let html = element.inner_html();
            if html.trim().is_empty() {
                return None;
            }
            return Some(self.quote_block(html));
        }
        None
    }

    fn quote_block(&self, html: String) -> BlockKind {
        let inline_images = referenced_inline_images(&html, self.import.inline_images);
        BlockKind::OriginalMessage {
            html,
            inline_images,
            source_id: self.import.source_id.to_string(),
        }
    }
}

/// Outlook Web's quoted-headers block: `<div id="divRplyFwdMsg">` holds the
/// `From:`/`Sent:` lines, and the message it answers follows it.
fn opens_quoted_tail(element: ElementRef<'_>) -> bool {
    element
        .value()
        .attr("id")
        .is_some_and(|id| id.eq_ignore_ascii_case("divRplyFwdMsg"))
}

/// Runs of empty paragraphs are the punctuation of generated mail HTML
/// (Word emits one per `<o:p>`), not a spacing choice worth restoring.
fn collapse_blank_paragraphs(blocks: Vec<BlockKind>) -> Vec<BlockKind> {
    let mut out: Vec<BlockKind> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let blank = matches!(&block, BlockKind::Paragraph(text) if text.is_empty());
        if blank && matches!(out.last(), Some(BlockKind::Paragraph(text)) if text.is_empty()) {
            continue;
        }
        out.push(block);
    }
    out
}

fn is_container(name: &str) -> bool {
    matches!(
        name,
        "div"
            | "section"
            | "article"
            | "main"
            | "aside"
            | "header"
            | "footer"
            | "nav"
            | "center"
            | "figure"
            | "figcaption"
            | "form"
            | "fieldset"
            | "dl"
            | "dd"
            | "dt"
            | "li"
            | "tr"
            | "td"
            | "th"
            | "tbody"
            | "thead"
            | "tfoot"
            | "caption"
            | "body"
            | "html"
    )
}

fn is_block_level(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
            | "ul"
            | "ol"
            | "table"
            | "hr"
            | "section"
            | "article"
            | "main"
            | "aside"
            | "header"
            | "footer"
            | "nav"
            | "center"
            | "figure"
            | "form"
            | "fieldset"
            | "dl"
    )
}

fn has_block_child(element: ElementRef<'_>) -> bool {
    element
        .child_elements()
        .any(|child| is_block_level(child.value().name()))
}

/// The image of a paragraph that holds nothing else.
fn sole_image_block(element: ElementRef<'_>) -> Option<BlockKind> {
    if element.text().any(|text| !text.trim().is_empty()) {
        return None;
    }
    let mut images = element
        .child_elements()
        .filter(|child| child.value().name() != "br");
    let image = images.next()?;
    if images.next().is_some() || image.value().name() != "img" {
        return None;
    }
    image_block(image)
}

fn image_block(element: ElementRef<'_>) -> Option<BlockKind> {
    let src = element.value().attr("src")?.trim();
    let cid = src
        .strip_prefix("cid:")
        .or_else(|| src.strip_prefix("CID:"))?;
    let cid = normalize_cid(cid);
    if cid.is_empty() {
        return None;
    }
    Some(BlockKind::Image {
        cid,
        width: image_width(element),
    })
}

/// `width="240"` as the send path writes it, or the CSS the editor's resize
/// handle produced in another client.
fn image_width(element: ElementRef<'_>) -> Option<u32> {
    if let Some(width) = element
        .value()
        .attr("width")
        .and_then(|width| width.trim().trim_end_matches("px").trim().parse().ok())
    {
        return Some(width);
    }
    style_property(element, "width")?
        .trim()
        .strip_suffix("px")?
        .trim()
        .parse()
        .ok()
}

fn normalize_cid(value: &str) -> String {
    let decoded = urlencoding::decode(value.trim())
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.trim().to_string());
    decoded
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

fn code_block(element: ElementRef<'_>) -> BlockKind {
    let code = element
        .child_elements()
        .find(|child| child.value().name() == "code");
    let source = code.unwrap_or(element);
    let language = source
        .value()
        .attr("class")
        .and_then(|class| {
            class
                .split_whitespace()
                .find_map(|token| token.strip_prefix("language-"))
        })
        .unwrap_or_default()
        .to_string();
    let text = source
        .text()
        .collect::<String>()
        .replace('\u{a0}', " ")
        .trim_end_matches('\n')
        .to_string();
    BlockKind::Code { language, text }
}

fn list_block(element: ElementRef<'_>, ordered: bool) -> Option<BlockKind> {
    let mut items = Vec::new();
    collect_list_items(element, 0, &mut items);
    (!items.is_empty()).then_some(BlockKind::List { ordered, items })
}

fn collect_list_items(list: ElementRef<'_>, indent: u8, items: &mut Vec<ListItem>) {
    for child in list.child_elements() {
        match child.value().name() {
            "li" => {
                let text = list_item_text(child);
                if !text.is_empty() || child.child_elements().next().is_none() {
                    items.push(ListItem {
                        id: 0,
                        indent,
                        text,
                    });
                }
                for nested in child.child_elements() {
                    if matches!(nested.value().name(), "ul" | "ol") {
                        collect_list_items(nested, indent.saturating_add(1), items);
                    }
                }
            }
            // Sub-lists are sometimes siblings of the row they belong to.
            "ul" | "ol" => collect_list_items(child, indent.saturating_add(1), items),
            _ => {}
        }
    }
}

/// The row's own content — a nested list is walked separately, as its own
/// rows, so it must not be inlined here too.
fn list_item_text(item: ElementRef<'_>) -> String {
    let mut out = String::new();
    for child in item.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&collapse_whitespace(text)),
            Node::Element(element) => {
                if matches!(element.name(), "ul" | "ol") {
                    continue;
                }
                if let Some(child) = ElementRef::wrap(child) {
                    push_inline(child, &mut out);
                }
            }
            _ => {}
        }
    }
    out.trim().replace('\n', " ")
}

fn is_data_table(element: ElementRef<'_>) -> bool {
    element.select(selector!("th, thead")).next().is_some()
}

fn sole_cell(element: ElementRef<'_>) -> Option<ElementRef<'_>> {
    let mut cells = element.select(selector!("td, th"));
    let cell = cells.next()?;
    cells.next().is_none().then_some(cell)
}

fn table_block(element: ElementRef<'_>) -> Option<BlockKind> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in element.select(selector!("tr")) {
        let mut cells: Vec<String> = Vec::new();
        for cell in row.child_elements() {
            if !matches!(cell.value().name(), "td" | "th") {
                continue;
            }
            // Structure inside a cell has nowhere to go in a grid of strings.
            if has_block_child(cell)
                || cell
                    .select(selector!("table, ul, ol, img, hr"))
                    .next()
                    .is_some()
            {
                return None;
            }
            cells.push(styled_inline(cell).trim().to_string());
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return None;
    }
    for row in &mut rows {
        row.resize(columns, String::new());
    }
    Some(BlockKind::Table { rows })
}

/// Inline content of an element, with the marks its own CSS stands for.
fn styled_inline(element: ElementRef<'_>) -> String {
    apply_style_marks(element, inline_children(element))
}

fn inline_children(element: ElementRef<'_>) -> String {
    let mut out = String::new();
    for child in element.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&collapse_whitespace(text)),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    push_inline(child, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

fn push_inline(element: ElementRef<'_>, out: &mut String) {
    match element.value().name() {
        "script" | "style" | "head" | "meta" | "link" | "title" | "noscript" => {}
        "br" => out.push('\n'),
        "img" => out.push_str(&image_markdown(element)),
        "a" => out.push_str(&link_markdown(element)),
        "code" | "kbd" | "samp" | "tt" => {
            let text = inline_children(element);
            if text.trim().is_empty() {
                out.push_str(&text);
            } else {
                emphasize(&text, "`", "`", out);
            }
        }
        "strong" | "b" => emphasize(&inline_children(element), "**", "**", out),
        "em" | "i" | "cite" | "var" => emphasize(&inline_children(element), "*", "*", out),
        "s" | "del" | "strike" => emphasize(&inline_children(element), "~~", "~~", out),
        // Markdown has no underline; the send path keeps inline HTML.
        "u" | "ins" => emphasize(&inline_children(element), "<u>", "</u>", out),
        "p" | "div" | "li" | "tr" => {
            // A block that reached the inline path (a paragraph inside a table
            // cell, a stray `<div>` inside a link) still ends its line.
            let text = styled_inline(element);
            if !text.trim().is_empty() {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(text.trim());
            }
        }
        _ => out.push_str(&styled_inline(element)),
    }
}

/// Wraps `inner` in a mark, keeping the surrounding whitespace outside it —
/// CommonMark does not open emphasis on `** bold**`, and mail HTML routinely
/// puts the space inside the tag.
fn emphasize(inner: &str, open: &str, close: &str, out: &mut String) {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        out.push_str(inner);
        return;
    }
    let leading = &inner[..inner.len() - inner.trim_start().len()];
    let trailing = &inner[inner.trim_end().len()..];
    out.push_str(leading);
    out.push_str(open);
    out.push_str(trimmed);
    out.push_str(close);
    out.push_str(trailing);
}

/// The three CSS properties that carry meaning a block document can store.
/// Outlook and Gmail write these on `<span>`s instead of using `<b>`/`<i>`.
fn apply_style_marks(element: ElementRef<'_>, inner: String) -> String {
    if inner.trim().is_empty() {
        return inner;
    }
    let mut out = inner;
    let weight = style_property(element, "font-weight");
    let is_bold = weight.as_deref().is_some_and(|weight| {
        let weight = weight.trim();
        weight.eq_ignore_ascii_case("bold")
            || weight.eq_ignore_ascii_case("bolder")
            || weight.parse::<u32>().is_ok_and(|weight| weight >= 600)
    });
    if style_property(element, "font-style")
        .is_some_and(|style| style.trim().eq_ignore_ascii_case("italic"))
    {
        let mut marked = String::new();
        emphasize(&out, "*", "*", &mut marked);
        out = marked;
    }
    if is_bold {
        let mut marked = String::new();
        emphasize(&out, "**", "**", &mut marked);
        out = marked;
    }
    let decoration = style_property(element, "text-decoration")
        .or_else(|| style_property(element, "text-decoration-line"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if decoration.contains("line-through") {
        let mut marked = String::new();
        emphasize(&out, "~~", "~~", &mut marked);
        out = marked;
    }
    if decoration.contains("underline") {
        let mut marked = String::new();
        emphasize(&out, "<u>", "</u>", &mut marked);
        out = marked;
    }
    out
}

fn style_property(element: ElementRef<'_>, property: &str) -> Option<String> {
    let style = element.value().attr("style")?;
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(property)
            .then(|| value.trim().to_string())
    })
}

fn image_markdown(element: ElementRef<'_>) -> String {
    let value = element.value();
    let Some(src) = value
        .attr("src")
        .map(str::trim)
        .filter(|src| !src.is_empty())
    else {
        return String::new();
    };
    let alt = value.attr("alt").unwrap_or_default().trim();
    format!("![{}]({})", escape_brackets(alt), markdown_url(src))
}

fn link_markdown(element: ElementRef<'_>) -> String {
    let label = inline_children(element);
    let trimmed = label.trim();
    let href = element.value().attr("href").unwrap_or_default().trim();
    if href.is_empty() || href.starts_with("javascript:") || href.starts_with('#') {
        return label;
    }
    if trimmed.is_empty() {
        return label;
    }
    // Always the labelled form, even when the label is the address itself:
    // `<url>` looks the same folded, but its visible text *is* its
    // destination, so renaming it in place would break the link.
    let leading = &label[..label.len() - label.trim_start().len()];
    let trailing = &label[label.trim_end().len()..];
    format!(
        "{leading}[{}]({}){trailing}",
        escape_brackets(trimmed),
        markdown_url(href)
    )
}

/// An unbalanced bracket in a label would close the link early, so both are
/// escaped — losing a character the user typed is worse than a visible `\`.
fn escape_brackets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '[' | ']') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// A destination with spaces or parentheses only survives the `<…>` form.
fn markdown_url(url: &str) -> String {
    if url.contains(char::is_whitespace) || url.contains('(') || url.contains(')') {
        format!("<{url}>")
    } else {
        url.to_string()
    }
}

/// HTML collapses whitespace; the source indentation of a `<p>` must not
/// become line breaks in the block's text. Non-breaking spaces come back as
/// ordinary ones — clients use them for indentation, and an invisible
/// U+00A0 in an editable paragraph is a trap.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() || character == '\u{a0}' || character == '\u{feff}' {
            if character == '\u{feff}' {
                continue;
            }
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(character);
            in_space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{build_html_body, Block};

    fn blocks(kinds: Vec<BlockKind>) -> Vec<Block> {
        kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| Block {
                id: index as u64 + 1,
                kind,
            })
            .collect()
    }

    fn reimport(kinds: Vec<BlockKind>) -> Vec<BlockKind> {
        let html = build_html_body(&blocks(kinds));
        html_to_blocks(&html, &HtmlImport::bare())
    }

    /// The whole point: what the composer sent to the provider must come back
    /// as the same document, not as a wall of paragraphs.
    #[test]
    fn a_draft_written_here_round_trips_through_its_own_html() {
        let document = vec![
            BlockKind::Heading {
                level: 2,
                text: "Compte rendu".to_string(),
            },
            BlockKind::Paragraph("Bonjour **Contact A**,".to_string()),
            BlockKind::Paragraph(String::new()),
            BlockKind::List {
                ordered: false,
                items: vec![
                    ListItem {
                        id: 0,
                        indent: 0,
                        text: "premier point".to_string(),
                    },
                    ListItem {
                        id: 0,
                        indent: 1,
                        text: "détail".to_string(),
                    },
                    ListItem {
                        id: 0,
                        indent: 0,
                        text: "second point".to_string(),
                    },
                ],
            },
            BlockKind::Quote("une citation".to_string()),
            BlockKind::Code {
                language: "rust".to_string(),
                text: "let x = 1;".to_string(),
            },
            BlockKind::Divider,
            BlockKind::Table {
                rows: vec![
                    vec!["Colonne".to_string(), "Valeur".to_string()],
                    vec!["A".to_string(), "1".to_string()],
                ],
            },
        ];

        assert_eq!(reimport(document.clone()), document);
    }

    /// Graph hands drafts back as a complete document, headers and all.
    #[test]
    fn a_draft_wrapped_in_a_full_document_by_the_provider_still_round_trips() {
        let document = vec![
            BlockKind::Paragraph("Bonjour,".to_string()),
            BlockKind::Signature {
                signature_id: Some(3),
                name: "Pro".to_string(),
                html: "<p>Contact A</p>".to_string(),
            },
        ];
        let sent = build_html_body(&blocks(document));
        let returned = format!(
            "<html><head><meta http-equiv=\"Content-Type\" \
             content=\"text/html; charset=utf-8\"><style>p {{margin:0}}</style>\
             </head><body>{sent}</body></html>"
        );

        let restored = html_to_blocks(&returned, &HtmlImport::bare());

        assert!(
            matches!(&restored[0], BlockKind::Paragraph(text) if text == "Bonjour,"),
            "{restored:?}"
        );
        assert!(
            matches!(&restored[1], BlockKind::Signature { signature_id, .. } if *signature_id == Some(3)),
            "{restored:?}"
        );
    }

    /// Reopening and re-saving must not nest the document one level deeper
    /// every time: a draft is edited over days.
    #[test]
    fn saving_a_reopened_draft_produces_the_same_html_again() {
        let document = vec![
            BlockKind::Paragraph("Bonjour,".to_string()),
            BlockKind::OriginalMessage {
                html: "<p>message d'origine</p>".to_string(),
                inline_images: Vec::new(),
                source_id: String::new(),
            },
            BlockKind::Signature {
                signature_id: Some(3),
                name: "Pro".to_string(),
                html: "<p>Contact A</p>".to_string(),
            },
        ];

        let first = build_html_body(&blocks(document.clone()));
        let reopened = html_to_blocks(&first, &HtmlImport::bare());
        let second = build_html_body(&blocks(reopened.clone()));

        assert_eq!(first, second);
        assert_eq!(html_to_blocks(&second, &HtmlImport::bare()), reopened);
    }

    #[test]
    fn inline_marks_and_links_survive_the_round_trip() {
        let document = vec![BlockKind::Paragraph(
            "du **gras**, de l'*italique*, du ~~barré~~, du `code` et \
             [un lien](https://example.test/a)"
                .to_string(),
        )];

        assert_eq!(reimport(document.clone()), document);
    }

    #[test]
    fn a_resized_inline_image_keeps_its_width() {
        let document = vec![BlockKind::Image {
            cid: "logo@example".to_string(),
            width: Some(240),
        }];

        assert_eq!(reimport(document.clone()), document);
    }

    /// The signature identity has to survive the trip, otherwise reopening a
    /// draft turns it back into an anonymous fragment nobody can swap.
    #[test]
    fn a_signature_comes_back_as_a_signature_block() {
        let document = vec![
            BlockKind::Paragraph("Bonjour,".to_string()),
            BlockKind::Signature {
                signature_id: Some(7),
                name: "Pro".to_string(),
                html: "<p>Contact A<br>Organisation de test</p>".to_string(),
            },
        ];

        let restored = reimport(document);

        assert_eq!(restored.len(), 2, "{restored:?}");
        let BlockKind::Signature {
            signature_id,
            name,
            html,
        } = &restored[1]
        else {
            panic!("signature block expected: {restored:?}");
        };
        assert_eq!(*signature_id, Some(7));
        // The name is resolved by the caller from the mailbox settings.
        assert!(name.is_empty());
        assert!(html.contains("Organisation de test"), "{html}");
    }

    #[test]
    fn a_quoted_original_stays_faithful() {
        let inline = InlineImage {
            cid: "photo@example".to_string(),
            mime: "image/png".to_string(),
            bytes: vec![1, 2, 3],
        };
        let document = vec![BlockKind::OriginalMessage {
            html: r#"<table><tr><th>H</th></tr></table><img src="cid:photo@example">"#.to_string(),
            inline_images: vec![inline.clone()],
            source_id: "message-1".to_string(),
        }];
        let html = build_html_body(&blocks(document));

        let restored = html_to_blocks(
            &html,
            &HtmlImport {
                inline_images: std::slice::from_ref(&inline),
                source_id: "message-1",
            },
        );

        assert_eq!(restored.len(), 1, "{restored:?}");
        let BlockKind::OriginalMessage {
            html,
            inline_images,
            source_id,
        } = &restored[0]
        else {
            panic!("faithful quote expected: {restored:?}");
        };
        assert!(html.contains("<th"), "table layout lost: {html}");
        assert_eq!(inline_images, &[inline]);
        assert_eq!(source_id, "message-1");
    }

    #[test]
    fn gmail_conventions_are_recognised() {
        let html = r#"<div dir="ltr">Bonjour,<div><br></div>
            <div class="gmail_signature">Contact A</div></div>
            <blockquote class="gmail_quote"><p>message d'origine</p></blockquote>"#;

        let blocks = html_to_blocks(html, &HtmlImport::bare());

        assert!(
            matches!(&blocks[0], BlockKind::Paragraph(text) if text == "Bonjour,"),
            "{blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, BlockKind::Signature { .. })),
            "{blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, BlockKind::OriginalMessage { .. })),
            "{blocks:?}"
        );
    }

    #[test]
    fn an_outlook_reply_keeps_everything_below_the_headers_as_the_original() {
        let html = r#"<div><div>Ma réponse</div>
            <div id="divRplyFwdMsg"><b>De :</b> Contact A</div>
            <div>message d'origine</div>
            <table><tr><th>H</th></tr></table></div>"#;

        let blocks = html_to_blocks(html, &HtmlImport::bare());

        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(
            matches!(&blocks[0], BlockKind::Paragraph(text) if text == "Ma réponse"),
            "{blocks:?}"
        );
        let BlockKind::OriginalMessage { html, .. } = &blocks[1] else {
            panic!("faithful quote expected: {blocks:?}");
        };
        assert!(html.contains("message d'origine"), "{html}");
        assert!(html.contains("<th>H</th>"), "{html}");
    }

    /// Outlook writes one `<div>` per line and marks with CSS rather than
    /// tags. Flattened into a single paragraph, a draft became unreadable.
    #[test]
    fn outlook_style_lines_and_css_marks_become_blocks() {
        let html = r#"<div><div>Première ligne</div>
            <div><span style="font-weight:700">grasse</span> et
            <span style="text-decoration:underline">soulignée</span></div>
            <div><o:p>&nbsp;</o:p></div>
            <div>Dernière ligne</div></div>"#;

        let blocks = html_to_blocks(html, &HtmlImport::bare());

        assert_eq!(
            blocks,
            vec![
                BlockKind::Paragraph("Première ligne".to_string()),
                BlockKind::Paragraph("**grasse** et <u>soulignée</u>".to_string()),
                BlockKind::Paragraph(String::new()),
                BlockKind::Paragraph("Dernière ligne".to_string()),
            ]
        );
    }

    /// A one-cell table is a width constraint, not data: its content has to
    /// keep flowing as blocks.
    #[test]
    fn a_single_cell_layout_table_is_unwrapped() {
        let html = r#"<table width="600"><tr><td><p>Bonjour</p><p>Merci</p></td></tr></table>"#;

        assert_eq!(
            html_to_blocks(html, &HtmlImport::bare()),
            vec![
                BlockKind::Paragraph("Bonjour".to_string()),
                BlockKind::Paragraph("Merci".to_string()),
            ]
        );
    }

    /// …but a grid the editor cannot hold must not be flattened either.
    #[test]
    fn a_table_of_structured_cells_stays_opaque() {
        let html =
            r#"<table><tr><td><img src="cid:logo"></td><td><ul><li>a</li></ul></td></tr></table>"#;

        let blocks = html_to_blocks(html, &HtmlImport::bare());

        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert!(
            matches!(&blocks[0], BlockKind::RawHtml { html } if html.contains("<ul>")),
            "{blocks:?}"
        );
    }

    #[test]
    fn html_indentation_never_becomes_line_breaks() {
        let html = "<p>\n   une phrase\n   coupée dans la source\n</p>";

        assert_eq!(
            html_to_blocks(html, &HtmlImport::bare()),
            vec![BlockKind::Paragraph(
                "une phrase coupée dans la source".to_string()
            )]
        );
    }

    #[test]
    fn a_line_break_inside_a_paragraph_is_kept() {
        let html = "<p>première<br>seconde</p>";

        assert_eq!(
            html_to_blocks(html, &HtmlImport::bare()),
            vec![BlockKind::Paragraph("première\nseconde".to_string())]
        );
    }

    #[test]
    fn visible_text_ignores_markup() {
        assert_eq!(
            html_text("<div><p>Contact A</p><p>Organisation\n de test</p></div>"),
            "Contact A Organisation de test"
        );
    }
}
