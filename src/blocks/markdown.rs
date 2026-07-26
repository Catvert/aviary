use std::collections::HashSet;

use crate::model::InlineImage;

use super::model::{Block, BlockKind, ListItem};

/// CommonMark ignores runs of blank lines, so an empty editor paragraph needs
/// an explicit block to survive Markdown serialization and HTML rendering.
const EMPTY_PARAGRAPH_HTML: &str = "<p><br></p>";

/// Top-level CommonMark block we're currently capturing during the walk in
/// `markdown_to_blocks`.
enum TopMarker {
    Paragraph,
    Heading(u8),
    Quote,
    Code(String),
    List,
    Table,
}

/// Walk a markdown source with `pulldown_cmark` and emit one `BlockKind` per
/// top-level CommonMark block. We use `into_offset_iter` to slice the original
/// markdown text for each block — that way inline formatting (`**bold**`,
/// `_italic_`, inline ` ``code`` `, links) is preserved verbatim and round-trips
/// through `blocks_to_markdown` on send.
pub(crate) fn markdown_to_blocks(md: &str) -> Vec<BlockKind> {
    use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut blocks: Vec<BlockKind> = Vec::new();
    let mut marker: Option<TopMarker> = None;
    let mut start_off: usize = 0;
    // Nesting depth of containers we should not split on while a top-level
    // marker is active (BlockQuote, List, Item, FootnoteDef).
    let mut container_depth: u32 = 0;

    for (event, range) in Parser::new_ext(md, opts).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if marker.is_none() {
                    match tag {
                        Tag::Paragraph => {
                            marker = Some(TopMarker::Paragraph);
                            start_off = range.start;
                        }
                        Tag::Heading { level, .. } => {
                            let l = match level {
                                HeadingLevel::H1 => 1,
                                HeadingLevel::H2 => 2,
                                _ => 3,
                            };
                            marker = Some(TopMarker::Heading(l));
                            start_off = range.start;
                        }
                        Tag::BlockQuote(_) => {
                            marker = Some(TopMarker::Quote);
                            start_off = range.start;
                            container_depth += 1;
                        }
                        Tag::CodeBlock(kind) => {
                            let lang = match kind {
                                CodeBlockKind::Fenced(s) => s.to_string(),
                                CodeBlockKind::Indented => String::new(),
                            };
                            marker = Some(TopMarker::Code(lang));
                            start_off = range.start;
                        }
                        Tag::List(_) => {
                            marker = Some(TopMarker::List);
                            start_off = range.start;
                            container_depth += 1;
                        }
                        Tag::Table(_) => {
                            marker = Some(TopMarker::Table);
                            start_off = range.start;
                        }
                        _ => {}
                    }
                } else {
                    // Already inside a top-level block; only track containers
                    // so we know when the outermost one closes.
                    if matches!(
                        tag,
                        Tag::BlockQuote(_) | Tag::List(_) | Tag::Item | Tag::FootnoteDefinition(_)
                    ) {
                        container_depth += 1;
                    }
                }
            }
            Event::End(end) => {
                let close = match (&marker, &end) {
                    (Some(TopMarker::Paragraph), TagEnd::Paragraph) if container_depth == 0 => true,
                    (Some(TopMarker::Heading(_)), TagEnd::Heading(_)) if container_depth == 0 => {
                        true
                    }
                    (Some(TopMarker::Code(_)), TagEnd::CodeBlock) if container_depth == 0 => true,
                    (Some(TopMarker::Quote), TagEnd::BlockQuote(_)) if container_depth == 1 => true,
                    (Some(TopMarker::List), TagEnd::List(_)) if container_depth == 1 => true,
                    (Some(TopMarker::Table), TagEnd::Table) if container_depth == 0 => true,
                    _ => false,
                };
                if matches!(
                    end,
                    TagEnd::BlockQuote(_)
                        | TagEnd::List(_)
                        | TagEnd::Item
                        | TagEnd::FootnoteDefinition
                ) {
                    container_depth = container_depth.saturating_sub(1);
                }
                if close {
                    let raw = md.get(start_off..range.end).unwrap_or("");
                    if let Some(m) = marker.take() {
                        push_block_from_raw(&mut blocks, m, raw);
                    }
                }
            }
            Event::Rule if marker.is_none() => {
                blocks.push(BlockKind::Divider);
            }
            Event::Html(html)
                if marker.is_none() && html.trim().eq_ignore_ascii_case(EMPTY_PARAGRAPH_HTML) =>
            {
                blocks.push(BlockKind::Paragraph(String::new()));
            }
            _ => {}
        }
    }

    blocks
}

fn push_block_from_raw(out: &mut Vec<BlockKind>, marker: TopMarker, raw: &str) {
    match marker {
        TopMarker::Paragraph => {
            let t = raw.trim();
            if !t.is_empty() {
                out.push(BlockKind::Paragraph(t.to_string()));
            }
        }
        TopMarker::Heading(level) => {
            // Strip ATX `#` prefix, optional trailing `#`, or setext underline
            // (=== / ---) lines so the stored text is the heading content only.
            let mut text_parts: Vec<String> = Vec::new();
            for line in raw.lines() {
                let l = line.trim();
                if l.is_empty() {
                    continue;
                }
                if l.chars().all(|c| c == '=') || l.chars().all(|c| c == '-') {
                    continue;
                }
                let cleaned = l.trim_start_matches('#').trim_start();
                let cleaned = cleaned.trim_end();
                // CommonMark only treats trailing `#` as an ATX closing
                // sequence when whitespace separates it from the content.
                // Keep the language name in headings such as `# Learn C#`.
                let cleaned = cleaned
                    .rfind(char::is_whitespace)
                    .filter(|&space| {
                        let suffix = &cleaned[space..];
                        suffix.chars().any(|ch| ch == '#')
                            && suffix.chars().all(|ch| ch == '#' || ch.is_whitespace())
                    })
                    .map_or(cleaned, |space| cleaned[..space].trim_end());
                if !cleaned.is_empty() {
                    text_parts.push(cleaned.to_string());
                }
            }
            let text = text_parts.join(" ");
            if !text.is_empty() {
                out.push(BlockKind::Heading { level, text });
            }
        }
        TopMarker::Quote => {
            // Drop the leading `> ` marker from each line and collapse the
            // result back into a single Quote block.
            let mut lines: Vec<String> = Vec::new();
            for line in raw.lines() {
                let trimmed = line.trim_start();
                let stripped = trimmed
                    .strip_prefix('>')
                    .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    .unwrap_or(trimmed);
                lines.push(stripped.to_string());
            }
            // Trim leading/trailing blank lines but keep interior ones so a
            // multi-paragraph quote stays multi-paragraph.
            while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.remove(0);
            }
            while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.pop();
            }
            let text = lines.join("\n");
            if !text.trim().is_empty() {
                out.push(BlockKind::Quote(text));
            }
        }
        TopMarker::Code(language) => {
            let mut lines: Vec<&str> = raw.lines().collect();
            if lines
                .first()
                .map(|l| l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~"))
                .unwrap_or(false)
            {
                lines.remove(0);
            }
            if lines
                .last()
                .map(|l| {
                    let t = l.trim();
                    t == "```" || t == "~~~"
                })
                .unwrap_or(false)
            {
                lines.pop();
            }
            let text = lines.join("\n");
            out.push(BlockKind::Code { language, text });
        }
        TopMarker::List => {
            if let Some(block) = parse_list_block(raw) {
                out.push(block);
            } else {
                let t = raw.trim();
                if !t.is_empty() {
                    out.push(BlockKind::Paragraph(t.to_string()));
                }
            }
        }
        TopMarker::Table => {
            if let Some(block) = parse_table_block(raw) {
                out.push(block);
            }
        }
    }
}

/// Converts a pipe-delimited Markdown table into a matrix of cells.
/// Escaped pipes (`\|`) remain in the text, and the CommonMark separator row
/// is removed.
fn parse_table_block(raw: &str) -> Option<BlockKind> {
    let mut rows: Vec<Vec<String>> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(split_table_row)
        .filter(|row| !row.is_empty())
        .collect();
    if rows.len() >= 2 && rows[1].iter().all(|cell| is_table_separator(cell)) {
        rows.remove(1);
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 || rows.is_empty() {
        return None;
    }
    for row in &mut rows {
        row.resize(columns, String::new());
    }
    Some(BlockKind::Table { rows })
}

fn split_table_row(line: &str) -> Vec<String> {
    let line = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            if ch != '|' && ch != '\\' {
                cell.push('\\');
            }
            cell.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(ch);
        }
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell.trim().to_string());
    cells
}

fn is_table_separator(cell: &str) -> bool {
    let cell = cell.trim().trim_matches(':').trim();
    cell.len() >= 3 && cell.chars().all(|ch| ch == '-')
}

fn table_cell_markdown(cell: &str) -> String {
    cell.replace('|', "\\|").replace('\n', "<br>")
}

/// Walk a captured top-level markdown list and produce a `BlockKind::List`
/// with one `ListItem` per line. The line scanner is intentionally minimal:
/// it picks up the indent (counted in 2-space steps), the marker type to
/// decide ordered vs unordered, and the inline content. Multi-paragraph or
/// multi-line items are flattened into the single line they started on —
/// the editor model has no notion of paragraphs inside a list row, and
/// pasting CommonMark with rich items remains a corner case we don't need
/// to support yet.
///
/// Returns `None` if the raw text doesn't look like a list (no recognised
/// marker on the first non-empty line); the caller falls back to dumping
/// the raw markdown into a Paragraph.
fn parse_list_block(raw: &str) -> Option<BlockKind> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // (^|\n)(spaces)(marker)(rest of line). Marker is `-`, `*`, `+` or
    // `digits.`. We don't support `digits)` (rare CommonMark) on purpose.
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?m)^(?P<indent>[ \t]*)(?P<marker>-|\*|\+|\d+\.)\s+(?P<text>.*)$")
            .unwrap()
    });
    let mut items: Vec<ListItem> = Vec::new();
    let mut ordered: Option<bool> = None;
    let mut next_id: u64 = 0;
    for cap in re.captures_iter(raw) {
        let indent_str = cap.name("indent").map(|m| m.as_str()).unwrap_or("");
        let marker = cap.name("marker").map(|m| m.as_str()).unwrap_or("-");
        let text = cap.name("text").map(|m| m.as_str()).unwrap_or("").trim();
        let is_ordered = marker.ends_with('.');
        if ordered.is_none() {
            ordered = Some(is_ordered);
        }
        // Translate column count → indent level. Tabs count as 4 columns,
        // every 2 spaces = 1 level. The first item lands at indent 0
        // regardless of leading whitespace (CommonMark allows up to 3
        // leading spaces before the marker without indenting).
        let columns: usize = indent_str
            .chars()
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum();
        let indent: u8 = (columns / 2).min(u8::MAX as usize) as u8;
        next_id = next_id.saturating_add(1);
        items.push(ListItem {
            id: 0,
            indent,
            text: text.to_string(),
        });
    }
    if items.is_empty() {
        return None;
    }
    // Normalise the indent so the shallowest row sits at 0 — pulldown_cmark
    // sometimes hands us a slice that starts with extra indentation when the
    // list lives inside a quote or another container.
    let min_indent = items.iter().map(|i| i.indent).min().unwrap_or(0);
    if min_indent > 0 {
        for it in items.iter_mut() {
            it.indent -= min_indent;
        }
    }
    Some(BlockKind::List {
        ordered: ordered.unwrap_or(false),
        items,
    })
}

/// Same as `blocks_to_markdown` but lets the caller pick the URI scheme used
/// for inline images. The send path emits `cid:` (RFC 2392) so receiving
/// MUAs resolve images against the multipart/related body; the live preview
/// uses `bytes://blocks-{id}-` so the preview can resolve the registered bytes.
pub(crate) fn blocks_to_markdown_with_image_prefix(blocks: &[Block], prefix: &str) -> String {
    let mut out = String::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        match &b.kind {
            BlockKind::Paragraph(t) if t.trim().is_empty() => out.push_str(EMPTY_PARAGRAPH_HTML),
            BlockKind::Paragraph(t) => out.push_str(&with_hard_breaks(t)),
            BlockKind::Heading { level, text } => {
                let level = (*level).clamp(1, 6) as usize;
                out.push_str(&"#".repeat(level));
                out.push(' ');
                out.push_str(text);
            }
            BlockKind::Quote(t) => {
                for (j, line) in t.lines().enumerate() {
                    if j > 0 {
                        out.push_str("  \n");
                    }
                    out.push_str("> ");
                    out.push_str(line);
                }
                if t.is_empty() {
                    out.push_str("> ");
                }
            }
            BlockKind::Code { language, text } => {
                out.push_str("```");
                out.push_str(language);
                out.push('\n');
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```");
            }
            BlockKind::List { ordered, items } => {
                // Build markdown lines per item. Indent uses 2 spaces per
                // level — the conventional CommonMark style and what
                // pulldown_cmark + html renderers expect to detect a
                // nested list. Numbering for ordered lists restarts at
                // each new sub-list (same algorithm as `list_item_markers`
                // in the editor).
                let mut counters: Vec<u32> = Vec::new();
                for (j, it) in items.iter().enumerate() {
                    if j > 0 {
                        out.push('\n');
                    }
                    let lvl = it.indent as usize;
                    while counters.len() > lvl + 1 {
                        counters.pop();
                    }
                    while counters.len() < lvl + 1 {
                        counters.push(0);
                    }
                    counters[lvl] += 1;
                    out.push_str(&"  ".repeat(lvl));
                    if *ordered {
                        out.push_str(&format!("{}. ", counters[lvl]));
                    } else {
                        out.push_str("- ");
                    }
                    // Soft breaks inside an item would be rare (the editor
                    // doesn't even let you type them), but flatten any \n
                    // to a space anyway so we don't accidentally close the
                    // list when round-tripping pasted content.
                    out.push_str(&it.text.replace('\n', " "));
                }
            }
            BlockKind::Table { rows } => {
                let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
                if columns > 0 {
                    let header = rows.first().cloned().unwrap_or_default();
                    out.push('|');
                    for col in 0..columns {
                        out.push(' ');
                        out.push_str(&table_cell_markdown(
                            header.get(col).map(String::as_str).unwrap_or(""),
                        ));
                        out.push_str(" |");
                    }
                    out.push('\n');
                    out.push('|');
                    for _ in 0..columns {
                        out.push_str(" --- |");
                    }
                    for row in rows.iter().skip(1) {
                        out.push('\n');
                        out.push('|');
                        for col in 0..columns {
                            out.push(' ');
                            out.push_str(&table_cell_markdown(
                                row.get(col).map(String::as_str).unwrap_or(""),
                            ));
                            out.push_str(" |");
                        }
                    }
                }
            }
            BlockKind::Image { cid, width: _ } => {
                // Always emit the markdown image syntax — width attributes
                // are stamped onto the resulting `<img>` tag in
                // `build_html_body` instead, so this serializer keeps raw
                // HTML out of the Markdown preview.
                out.push_str(&format!("![]({prefix}{cid})"));
            }
            BlockKind::Divider => out.push_str("---"),
            BlockKind::RawHtml { .. } => {
                // As with faithful quotes, substitute the fragment after the
                // Markdown pass so pulldown-cmark neither normalizes nor
                // escapes it.
                out.push_str(&format!(r#"<div data-aviary-html="{}"></div>"#, b.id));
            }
            BlockKind::Signature { .. } => {
                out.push_str(&format!(r#"<div data-aviary-signature="{}"></div>"#, b.id));
            }
            BlockKind::OriginalMessage { .. } => {
                // Emit a block-level HTML placeholder. Pulldown_cmark
                // recognises `<div ...>` on its own line as a Type 6 HTML
                // block and passes it through verbatim, so the marker
                // survives the markdown→HTML pipeline. `build_html_body`
                // then does a string replace to swap in the actual
                // `<blockquote>…</blockquote>` content — going via a
                // marker (rather than embedding the original HTML in the
                // markdown stream) sidesteps any chance of the source
                // document's `<html><body>` wrappers confusing the
                // commonmark parser.
                out.push_str(&format!(r#"<div data-aviary-quote="{}"></div>"#, b.id));
            }
        }
    }
    out
}

/// Serialize the block list into a Markdown string for the send path. Uses
/// the `cid:` URI scheme for inline images (RFC 2392) so the resulting HTML
/// resolves against the multipart/related body the receiving MUA gets.
pub(crate) fn blocks_to_markdown(blocks: &[Block]) -> String {
    blocks_to_markdown_with_image_prefix(blocks, "cid:")
}

/// Inject `width="N"` into each `<img src="cid:CID"...>` tag whose CID is a
/// key in `widths`. Used by the send path to carry the user's resize choice
/// through to the receiving MUA. Idempotent: if the tag already has a width
/// attribute we leave it alone.
///
/// Also normalises pulldown_cmark's XHTML-style self-closing `<img … />` to
/// plain `<img …>`. The trailing `/` would otherwise be left dangling next
/// to the freshly-appended attribute (`alt="" / width="53">`), which is
/// broken HTML — and `html_to_markdown_rs` later mis-parses it and leaks
/// the literal text `width="53">` back into the draft on reopen. Doing the
/// strip here keeps every code path (with or without widths) uniform.
fn inject_image_widths(html: &str, widths: &std::collections::HashMap<&str, u32>) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r#"<img\s+([^>]*?)src="cid:([^"]+)"([^>]*)>"#).unwrap());
    re.replace_all(html, |caps: &regex::Captures| {
        let pre = &caps[1];
        let cid = &caps[2];
        let post = caps[3].trim_end().trim_end_matches('/').trim_end();
        let already_sized = pre.contains("width=") || post.contains("width=");
        match widths.get(cid) {
            Some(w) if !already_sized => {
                format!(r#"<img {pre}src="cid:{cid}"{post} width="{w}">"#)
            }
            _ => format!(r#"<img {pre}src="cid:{cid}"{post}>"#),
        }
    })
    .into_owned()
}

/// Append two trailing spaces before each `\n` so pulldown_cmark emits `<br>`
/// for every soft break. Doesn't touch trailing newlines (no break needed at
/// end of block) or already-doubled blank lines (those are paragraph breaks
/// and should pass through verbatim).
fn with_hard_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.matches('\n').count() * 2);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            // Don't add trailing spaces if next char is also '\n' (paragraph
            // break) or if the previous char is already a space — pulldown
            // treats either as a hard break.
            let next_is_nl = chars.get(i + 1) == Some(&'\n');
            let prev_is_space = i > 0 && chars[i - 1] == ' ';
            if !next_is_nl && !prev_is_space {
                out.push_str("  ");
            }
        }
        out.push(c);
    }
    out
}

/// Build the HTML body for an outgoing message: render the blocks as
/// markdown, convert to HTML, inject image widths, and restore any faithful
/// original-message fragments.
///
/// Inline MIME parts are deliberately not appended here. An image belongs in
/// the body only where a block (including `OriginalMessage`) references its
/// CID; orphan aliases supplied by providers must not become visible images
/// at the end of a forwarded message.
pub(crate) fn build_html_body(blocks: &[Block]) -> String {
    // The cursor marker belongs only to the template editor and must never
    // appear in a sent message.
    let md = blocks_to_markdown(blocks).replace(super::TEMPLATE_CURSOR_PLACEHOLDER, "");
    let html = render_markdown(&md);
    // Apply user-set widths from `BlockKind::Image { width: Some(_) }` by
    // injecting `width="N"` on the matching `<img src="cid:...">` tag in the
    // rendered HTML. We do this here rather than in the markdown serializer
    // because raw `<img>` HTML is deliberately absent from preview Markdown.
    let widths: std::collections::HashMap<&str, u32> = blocks
        .iter()
        .filter_map(|b| match &b.kind {
            BlockKind::Image {
                cid,
                width: Some(w),
            } => Some((cid.as_str(), *w)),
            _ => None,
        })
        .collect();
    let mut html = inject_image_widths(&html, &widths);
    html = style_markdown_blocks(&html);
    // Substitute opaque HTML and OriginalMessage placeholders emitted by
    // `blocks_to_markdown_with_image_prefix`. Full documents are reduced to
    // their body so we don't nest `<html><body>…` inside our outer body.
    for b in blocks {
        match &b.kind {
            BlockKind::RawHtml { html: inner } => {
                let placeholder = format!(r#"<div data-aviary-html="{}"></div>"#, b.id);
                let fragment = extract_user_html_fragment(inner)
                    .replace(super::TEMPLATE_CURSOR_PLACEHOLDER, "");
                html = html.replace(&placeholder, &fragment);
            }
            BlockKind::Signature {
                html: inner,
                signature_id,
                ..
            } => {
                let placeholder = format!(r#"<div data-aviary-signature="{}"></div>"#, b.id);
                // Marked with a class, the way Gmail and Outlook mark theirs:
                // a receiving client — or Aviary reading its own mail back —
                // can then tell the signature from the message.
                //
                // The id rides along so reopening a draft saved on the server
                // restores a signature block that still knows which signature
                // it is, and can therefore still be swapped. Only the id: the
                // name is the user's own wording and stays on the machine.
                let identity = signature_id
                    .map(|id| format!(r#" data-aviary-signature-id="{id}""#))
                    .unwrap_or_default();
                let fragment = extract_user_html_fragment(inner)
                    .replace(super::TEMPLATE_CURSOR_PLACEHOLDER, "");
                html = html.replace(
                    &placeholder,
                    &format!(r#"<div class="aviary-signature"{identity}>{fragment}</div>"#),
                );
            }
            BlockKind::OriginalMessage { html: inner, .. } => {
                let body_only = extract_body_html(inner);
                let placeholder = format!(r#"<div data-aviary-quote="{}"></div>"#, b.id);
                let replacement =
                    format!(r#"<div class="aviary-original-message">{body_only}</div>"#);
                html = html.replace(&placeholder, &replacement);
            }
            _ => {}
        }
    }
    format!(
        r#"<div style="font-family:Inter,'Noto Color Emoji',sans-serif;font-size:{}px;line-height:{}px">{html}</div>"#,
        super::COMPOSE_BODY_FONT_SIZE,
        super::COMPOSE_BODY_LINE_HEIGHT,
    )
}

/// Apply the same compact block metrics as the editor. Inline styles are used
/// because many email clients discard document stylesheets.
fn style_markdown_blocks(html: &str) -> String {
    let unordered_list = format!(
        r#"<ul style="margin:0;padding-left:{}px">"#,
        super::COMPOSE_LIST_INDENT
    );
    let ordered_list = format!(
        r#"<ol style="margin:0;padding-left:{}px">"#,
        super::COMPOSE_LIST_INDENT
    );
    let html = html
        .replace("<p>", r#"<p style="margin:0">"#)
        .replace(
            "<h1>",
            r#"<h1 style="font-size:26px;line-height:36.4px;font-weight:700;margin:0">"#,
        )
        .replace(
            "<h2>",
            r#"<h2 style="font-size:21px;line-height:29.4px;font-weight:700;margin:0">"#,
        )
        .replace(
            "<h3>",
            r#"<h3 style="font-size:17px;line-height:23.8px;font-weight:600;margin:0">"#,
        )
        .replace(
            "<h4>",
            r#"<h4 style="font-size:17px;line-height:23.8px;font-weight:600;margin:0">"#,
        )
        .replace(
            "<h5>",
            r#"<h5 style="font-size:17px;line-height:23.8px;font-weight:600;margin:0">"#,
        )
        .replace(
            "<h6>",
            r#"<h6 style="font-size:17px;line-height:23.8px;font-weight:600;margin:0">"#,
        )
        .replace(
            "<blockquote>",
            r##"<blockquote style="margin:0;padding-left:12px;border-left:2px solid #d4d4d4;color:#6b7280;font-style:italic">"##,
        )
        .replace(
            "<pre>",
            r##"<pre style="margin:0;padding:8px;border-radius:4px;background:#f3f4f6;font-family:'JetBrains Mono',monospace;white-space:pre-wrap">"##,
        )
        .replace("<ul>", &unordered_list)
        .replace("<ol>", &ordered_list)
        .replace("<li>", r#"<li style="margin:0;padding:0">"#)
        .replace(
            "<hr />",
            r##"<hr style="margin:8px 0;border:0;border-top:1px solid #d4d4d4">"##,
        );
    style_markdown_tables(&html)
}

/// Email clients do not apply a stylesheet to a CommonMark table. Simple
/// inline styles therefore ensure visible cells in Outlook, Gmail, and IMAP
/// clients.
fn style_markdown_tables(html: &str) -> String {
    html.replace(
        "<table>",
        r#"<table style="border-collapse:collapse;width:100%;table-layout:fixed">"#,
    )
    .replace(
        "<th>",
        r#"<th style="border:1px solid #cbd5e1;padding:6px 8px;text-align:left;vertical-align:top;background:#f1f5f9">"#,
    )
    .replace(
        "<td>",
        r#"<td style="border:1px solid #cbd5e1;padding:6px 8px;text-align:left;vertical-align:top">"#,
    )
}

/// Keep exactly the inline MIME parts referenced by the final outgoing HTML.
/// Graph can expose the same physical attachment under both its `contentId`
/// and its filename; only the identifiers that actually occur in `src=cid:`
/// belong in the forwarded multipart body.
pub(crate) fn referenced_inline_images(
    html: &str,
    attachments: &[InlineImage],
) -> Vec<InlineImage> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)cid:([^"'\s>)]+)"#).unwrap());
    let referenced: HashSet<String> = re
        .captures_iter(html)
        .map(|caps| caps[1].to_ascii_lowercase())
        .collect();
    let mut emitted = HashSet::new();
    attachments
        .iter()
        .filter(|image| {
            let cid = image.cid.to_ascii_lowercase();
            referenced.contains(&cid) && emitted.insert(cid)
        })
        .cloned()
        .collect()
}

/// Strip the `<html>`/`<body>` wrappers from a full document so the inner
/// content can be embedded inside another body. Falls back to the raw
/// input if the html doesn't look like a complete document or has no
/// recognisable body.
fn extract_body_html(raw: &str) -> String {
    let head = raw.trim_start().to_lowercase();
    if !head.starts_with("<!doctype") && !head.starts_with("<html") {
        return raw.to_string();
    }
    let doc = scraper::Html::parse_document(raw);
    let selector = match scraper::Selector::parse("body") {
        Ok(s) => s,
        Err(_) => return raw.to_string(),
    };
    if let Some(body) = doc.select(&selector).next() {
        return body.inner_html();
    }
    raw.to_string()
}

/// Version intended for user-created HTML fragments. For a complete document,
/// also preserves `<style>` sheets from the head: signatures exported from
/// another client often keep their styling there, while the wrapping `<html>`
/// element cannot be nested.
fn extract_user_html_fragment(raw: &str) -> String {
    let head = raw.trim_start().to_lowercase();
    if !head.starts_with("<!doctype") && !head.starts_with("<html") {
        return raw.to_string();
    }
    let doc = scraper::Html::parse_document(raw);
    let body_selector = match scraper::Selector::parse("body") {
        Ok(selector) => selector,
        Err(_) => return raw.to_string(),
    };
    let Some(body) = doc.select(&body_selector).next() else {
        return raw.to_string();
    };
    let mut fragment = String::new();
    if let Ok(style_selector) = scraper::Selector::parse("head style") {
        for style in doc.select(&style_selector) {
            fragment.push_str(&style.html());
        }
    }
    fragment.push_str(&body.inner_html());
    fragment
}

pub(crate) fn render_markdown(src: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(src, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(cid: &str, byte: u8) -> InlineImage {
        InlineImage {
            cid: cid.to_string(),
            mime: "image/png".to_string(),
            bytes: vec![byte],
        }
    }

    /// Both link forms the block editor writes have to survive the send path —
    /// the editor styles them as links, so a body that shipped them as literal
    /// text would contradict what the user was shown.
    #[test]
    fn both_link_forms_reach_the_outgoing_body_as_anchors() {
        let blocks = vec![
            Block {
                id: 1,
                kind: BlockKind::Paragraph(
                    "voir [Contact A](https://example.test/a) ici".to_string(),
                ),
            },
            Block {
                id: 2,
                kind: BlockKind::Paragraph("ou <https://example.test/b>".to_string()),
            },
        ];

        let html = build_html_body(&blocks);

        assert!(
            html.contains(r#"<a href="https://example.test/a">Contact A</a>"#),
            "labelled link missing: {html}"
        );
        assert!(
            html.contains(r#"href="https://example.test/b""#),
            "autolink missing: {html}"
        );
    }

    #[test]
    fn atx_heading_keeps_unspaced_trailing_hash() {
        assert_eq!(
            markdown_to_blocks("# Learn C#"),
            vec![BlockKind::Heading {
                level: 1,
                text: "Learn C#".to_string(),
            }]
        );
        assert_eq!(
            markdown_to_blocks("# Closed heading ##"),
            vec![BlockKind::Heading {
                level: 1,
                text: "Closed heading".to_string(),
            }]
        );
    }

    #[test]
    fn original_message_keeps_html_layout_without_trailing_images() {
        let blocks = vec![Block {
            id: 7,
            kind: BlockKind::OriginalMessage {
                html: r#"<html><body><table><tr><td>Signature</td></tr></table><img src="cid:real"></body></html>"#.to_string(),
                inline_images: vec![image("real", 1)],
                source_id: "message-1".to_string(),
            },
        }];

        let html = build_html_body(&blocks);

        assert!(html.contains("<table><tbody><tr><td>Signature</td></tr></tbody></table>"));
        assert_eq!(html.matches("cid:real").count(), 1);
        assert!(html.contains(r#"<div class="aviary-original-message">"#));
        assert!(!html.contains("<blockquote"));
    }

    /// The signature block ships the fragment it was rendered from, marked as
    /// a signature the way other clients mark theirs and carrying the id that
    /// lets a reopened draft recognise it — and never its placeholder, which
    /// would show up as an empty div in the mail.
    #[test]
    fn signature_block_reaches_the_outgoing_body_marked_as_one() {
        let blocks = vec![
            Block {
                id: 41,
                kind: BlockKind::Paragraph("Bonjour,".to_string()),
            },
            Block {
                id: 42,
                kind: BlockKind::Signature {
                    signature_id: Some(7),
                    name: "Pro".to_string(),
                    html: r#"<div style="color:#123456">Contact A<br><img src="cid:logo"></div>"#
                        .to_string(),
                },
            },
        ];

        let html = build_html_body(&blocks);

        assert!(html.contains(r#"<div class="aviary-signature" data-aviary-signature-id="7">"#));
        assert!(html.contains(r#"style="color:#123456""#));
        assert!(html.contains(r#"src="cid:logo""#));
        // The placeholder itself must be gone; only the identity attribute
        // built from it remains.
        assert!(!html.contains(r#"data-aviary-signature=""#));
    }

    /// A signature whose own definition is a full HTML document must be
    /// reduced to its fragment, like any imported HTML.
    #[test]
    fn signature_block_of_a_full_document_is_reduced_to_its_fragment() {
        let blocks = vec![Block {
            id: 43,
            kind: BlockKind::Signature {
                signature_id: None,
                name: "Importée".to_string(),
                html: "<html><head><style>.s{color:red}</style></head><body><p>Contact A</p></body></html>"
                    .to_string(),
            },
        }];

        let html = build_html_body(&blocks);

        assert!(html.contains("<style>.s{color:red}</style>"));
        assert!(html.contains("<p>Contact A</p>"));
        assert!(!html.contains("<html>"));
        assert!(!html.contains("<body>"));
    }

    #[test]
    fn raw_html_fragment_is_preserved_in_outgoing_body() {
        let blocks = vec![Block {
            id: 11,
            kind: BlockKind::RawHtml {
                html: r#"<html><head><style>.brand{font-weight:bold}</style></head><body><table class="brand" style="color:#123456"><tr><td>Logo</td></tr></table></body></html>"#.to_string(),
            },
        }];

        let html = build_html_body(&blocks);

        assert!(html.contains(r#"style="color:#123456""#));
        assert!(html.contains("<style>.brand{font-weight:bold}</style>"));
        assert!(html.contains("<td>Logo</td>"));
        assert!(!html.contains("data-aviary-html"));
        assert!(!html.contains("<html>"));
    }

    #[test]
    fn template_cursor_marker_is_never_sent() {
        let blocks = vec![
            Block {
                id: 21,
                kind: BlockKind::Paragraph(format!(
                    "Before {} after",
                    crate::blocks::TEMPLATE_CURSOR_PLACEHOLDER
                )),
            },
            Block {
                id: 22,
                kind: BlockKind::RawHtml {
                    html: format!(
                        "<div>HTML {} after</div>",
                        crate::blocks::TEMPLATE_CURSOR_PLACEHOLDER
                    ),
                },
            },
        ];

        let html = build_html_body(&blocks);

        assert!(!html.contains(crate::blocks::TEMPLATE_CURSOR_PLACEHOLDER));
        assert!(html.contains("Before  after"));
        assert!(html.contains("HTML  after"));
    }

    #[test]
    fn empty_paragraph_survives_markdown_and_outgoing_html() {
        let blocks = vec![
            Block {
                id: 31,
                kind: BlockKind::Paragraph("Ligne A".to_string()),
            },
            Block {
                id: 32,
                kind: BlockKind::Paragraph(String::new()),
            },
            Block {
                id: 33,
                kind: BlockKind::Paragraph("Ligne B".to_string()),
            },
        ];

        let markdown = blocks_to_markdown(&blocks);
        let round_trip = markdown_to_blocks(&markdown);
        let html = build_html_body(&blocks);

        assert_eq!(
            round_trip,
            blocks
                .into_iter()
                .map(|block| block.kind)
                .collect::<Vec<_>>()
        );
        assert!(html.contains(
            "<p style=\"margin:0\">Ligne A</p>\n<p style=\"margin:0\"><br></p>\n<p style=\"margin:0\">Ligne B</p>"
        ));
    }

    #[test]
    fn outgoing_blocks_use_editor_metrics() {
        let blocks = vec![
            Block {
                id: 41,
                kind: BlockKind::Heading {
                    level: 1,
                    text: "Titre de test".to_string(),
                },
            },
            Block {
                id: 42,
                kind: BlockKind::List {
                    ordered: false,
                    items: vec![
                        ListItem {
                            id: 1,
                            indent: 0,
                            text: "Élément A".to_string(),
                        },
                        ListItem {
                            id: 2,
                            indent: 1,
                            text: "Élément B".to_string(),
                        },
                    ],
                },
            },
            Block {
                id: 43,
                kind: BlockKind::Quote("Citation de test".to_string()),
            },
            Block {
                id: 44,
                kind: BlockKind::Code {
                    language: String::new(),
                    text: "valeur = 1".to_string(),
                },
            },
            Block {
                id: 45,
                kind: BlockKind::Divider,
            },
        ];

        let html = build_html_body(&blocks);

        assert!(html.starts_with(
            r#"<div style="font-family:Inter,'Noto Color Emoji',sans-serif;font-size:14px;line-height:20px">"#
        ));
        assert!(html.contains(
            r#"<h1 style="font-size:26px;line-height:36.4px;font-weight:700;margin:0">"#
        ));
        assert!(html.contains(r#"<ul style="margin:0;padding-left:24px">"#));
        assert_eq!(
            html.matches(r#"<ul style="margin:0;padding-left:24px">"#)
                .count(),
            2
        );
        assert_eq!(
            html.matches(r#"<li style="margin:0;padding:0">"#).count(),
            2
        );
        assert!(html.contains(r#"<blockquote style="margin:0;padding-left:12px;"#));
        assert!(html.contains(r#"<pre style="margin:0;padding:8px;"#));
        assert!(html.contains(r#"<hr style="margin:8px 0;"#));
    }

    #[test]
    fn outgoing_inline_images_drop_graph_aliases_and_duplicates() {
        let html = r#"<p><img src="cid:content-id"></p>"#;
        let images = vec![
            image("content-id", 1),
            image("image.png", 1),
            image("content-id", 1),
            image("unused", 2),
        ];

        let selected = referenced_inline_images(html, &images);

        assert_eq!(selected, vec![image("content-id", 1)]);
    }

    #[test]
    fn markdown_table_round_trips_as_an_editable_block() {
        let source = "| Name | Status |\n| --- | --- |\n| Aviary | **Ready** |";

        let kinds = markdown_to_blocks(source);

        assert_eq!(
            kinds,
            vec![BlockKind::Table {
                rows: vec![
                    vec!["Name".to_string(), "Status".to_string()],
                    vec!["Aviary".to_string(), "**Ready**".to_string()],
                ],
            }]
        );
        let blocks = vec![Block {
            id: 1,
            kind: kinds[0].clone(),
        }];
        assert_eq!(blocks_to_markdown(&blocks), source);
    }

    #[test]
    fn outgoing_table_has_email_safe_inline_borders() {
        let blocks = vec![Block {
            id: 1,
            kind: BlockKind::Table {
                rows: vec![
                    vec!["Name".to_string(), "Status".to_string()],
                    vec!["Aviary".to_string(), "<u>Ready</u>".to_string()],
                ],
            },
        }];

        let html = build_html_body(&blocks);

        assert!(html.contains("border-collapse:collapse"));
        assert!(html.contains("border:1px solid #cbd5e1"));
        assert!(html.contains("<u>Ready</u>"));
        assert!(html.contains("<thead>"));
        assert!(html.contains("<tbody>"));
    }
}
