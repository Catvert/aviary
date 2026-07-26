//! HTML pre/post-processing helpers shared by all email providers.
//!
//! Marketing emails almost always nest their visual structure inside
//! `<table>` elements (the only block primitive Outlook reliably renders).
//! `html_to_markdown_rs` faithfully turns each one into a GFM Markdown
//! table — which collapses multi-row layouts into a single garbled line and
//! produces useless `| --- |` separators around image-only or single-cell
//! rows.
//!
//! [`unwrap_layout_tables`] rewrites the tags of every "layout" table
//! (a table containing no `<th>` and no `<thead>` element) into generic
//! `<div>`/`<span>` so the converter sees flow content instead. Real data
//! tables — those with header cells — are left untouched and still render
//! as Markdown tables.
//!
//! The other two helpers ([`html_md_options`], [`collapse_blank_lines`])
//! were previously duplicated in every provider; they live here now so the
//! three backends share one definition.

use std::ops::Range;
use std::sync::OnceLock;

/// Extract normalized `cid:` references from an HTML body.
///
/// Email generators vary the casing, angle brackets and URL encoding of
/// Content-IDs. Keeping normalization shared lets every provider classify the
/// corresponding MIME parts consistently.
pub(crate) fn extract_cids_from_html(html: &str) -> Vec<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)cid:([^"'\s>)]+)"#).unwrap());
    re.captures_iter(html)
        .map(|capture| normalize_cid(&capture[1]))
        .collect()
}

pub(crate) fn normalize_cid(value: &str) -> String {
    let decoded = urlencoding::decode(value.trim())
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.trim().to_string());
    decoded
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

pub(crate) fn cid_matches(left: &str, right: &str) -> bool {
    normalize_cid(left).eq_ignore_ascii_case(&normalize_cid(right))
}

/// Some Outlook/MIME producers use the attachment filename as the CID while
/// others append a generated `@…` suffix to it.
pub(crate) fn cid_references_name(reference: &str, name: &str) -> bool {
    let reference = normalize_cid(reference).to_ascii_lowercase();
    let name = normalize_cid(name).to_ascii_lowercase();
    reference == name
        || reference
            .strip_prefix(&name)
            .is_some_and(|suffix| suffix.starts_with('@'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableTag {
    Table,
    Thead,
    Tbody,
    Tfoot,
    Tr,
    Td,
    Th,
    Caption,
    Colgroup,
    Col,
}

struct ParsedTag {
    range: Range<usize>,
    closing: bool,
    kind: TableTag,
}

fn tag_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Match an opening or closing tag for any of the table-related
        // elements. The body of the tag is matched with quoted-string
        // awareness so an attribute like `title="a > b"` doesn't
        // prematurely close the tag. `\b` after the name prevents `<th`
        // from matching `<thead`.
        regex::Regex::new(
            r#"(?i)<(?P<close>/?)(?P<name>table|thead|tbody|tfoot|tr|td|th|caption|colgroup|col)\b(?:"[^"]*"|'[^']*'|[^>])*>"#,
        )
        .expect("layout-table regex")
    })
}

fn classify_name(s: &str) -> Option<TableTag> {
    Some(match s.to_ascii_lowercase().as_str() {
        "table" => TableTag::Table,
        "thead" => TableTag::Thead,
        "tbody" => TableTag::Tbody,
        "tfoot" => TableTag::Tfoot,
        "tr" => TableTag::Tr,
        "td" => TableTag::Td,
        "th" => TableTag::Th,
        "caption" => TableTag::Caption,
        "colgroup" => TableTag::Colgroup,
        "col" => TableTag::Col,
        _ => return None,
    })
}

/// Rewrite the tags of every "layout" `<table>` (one without `<th>` /
/// `<thead>`) into generic flow elements, so the Markdown converter does
/// not produce broken GFM tables. Tables that *do* contain header cells —
/// real data tables — are left intact.
///
/// The decision is made per-table: a layout outer table holding a real
/// data inner table will have its own structure unwrapped while the inner
/// data table's tags remain untouched.
pub fn unwrap_layout_tables(html: &str) -> String {
    let mut tags: Vec<ParsedTag> = Vec::new();
    for caps in tag_regex().captures_iter(html) {
        let m = caps.get(0).expect("regex match");
        let closing = caps.name("close").map(|c| !c.is_empty()).unwrap_or(false);
        let Some(kind) = caps.name("name").and_then(|n| classify_name(n.as_str())) else {
            continue;
        };
        tags.push(ParsedTag {
            range: m.range(),
            closing,
            kind,
        });
    }
    if tags.is_empty() {
        return html.to_string();
    }

    // Pass 1: walk tags, maintaining a stack of `<table>` frames. For
    // each tag we record which frame it belongs to (None = outside any
    // table). When we see `<th>` or `<thead>` we mark the *current* frame
    // (innermost open table) as a data table.
    struct Frame {
        has_th: bool,
    }
    let mut frames: Vec<Frame> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut tag_frame: Vec<Option<usize>> = vec![None; tags.len()];

    for (i, tag) in tags.iter().enumerate() {
        match (tag.kind, tag.closing) {
            (TableTag::Table, false) => {
                frames.push(Frame { has_th: false });
                stack.push(frames.len() - 1);
                tag_frame[i] = stack.last().copied();
            }
            (TableTag::Table, true) => {
                tag_frame[i] = stack.last().copied();
                stack.pop();
            }
            (TableTag::Th | TableTag::Thead, false) => {
                if let Some(&top) = stack.last() {
                    frames[top].has_th = true;
                }
                tag_frame[i] = stack.last().copied();
            }
            _ => {
                tag_frame[i] = stack.last().copied();
            }
        }
    }

    // Pass 2: stitch the output. Tags whose enclosing table is a layout
    // table get rewritten; everything else (including tags inside nested
    // data tables) flows through verbatim.
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    for (i, tag) in tags.iter().enumerate() {
        let Some(frame_idx) = tag_frame[i] else {
            continue;
        };
        if frames[frame_idx].has_th {
            continue;
        }
        out.push_str(&html[cursor..tag.range.start]);
        match tag.kind {
            // Column descriptors carry no content; drop them.
            TableTag::Col | TableTag::Colgroup => {}
            // Cells stay inline within their row.
            TableTag::Td | TableTag::Th => {
                if tag.closing {
                    // Trailing space so adjacent cells don't run together.
                    out.push_str("</span> ");
                } else {
                    out.push_str("<span>");
                }
            }
            // Everything else becomes a block break.
            _ => {
                if tag.closing {
                    out.push_str("</div>");
                } else {
                    out.push_str("<div>");
                }
            }
        }
        cursor = tag.range.end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// Conversion options shared by every provider's HTML→Markdown pipeline.
///
/// `extract_metadata` is off because we already have envelope headers; the
/// `exclude_selectors` list strips the Outlook/Apple boilerplate (CSS
/// preamble, `meta-Generator`, `meta-x-apple-disable-message-reformatting`
/// …) that would otherwise leak into the rendered body.
pub fn html_md_options() -> html_to_markdown_rs::ConversionOptions {
    html_to_markdown_rs::ConversionOptions {
        extract_metadata: false,
        exclude_selectors: vec![
            "head".into(),
            "meta".into(),
            "title".into(),
            "link".into(),
            "style".into(),
            "script".into(),
        ],
        ..Default::default()
    }
}

/// Collapse runs of blank lines down to a single blank — `html_to_markdown`
/// is generous with newlines around block boundaries and unwrapped layout
/// tables compound it.
pub fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Convert an email HTML body to Markdown, applying the layout-table
/// unwrap pre-pass. On any conversion error the original HTML is returned
/// (matching previous behaviour) so the caller can still substitute CIDs
/// against it.
pub fn convert_email_html(html: &str) -> String {
    let started = std::time::Instant::now();
    let prepare_started = std::time::Instant::now();
    let prepared = preserve_underlines(&unwrap_layout_tables(html));
    let prepare_elapsed = prepare_started.elapsed();
    let conversion_started = std::time::Instant::now();
    let converted = html_to_markdown_rs::convert(&prepared, Some(html_md_options()));
    let conversion_elapsed = conversion_started.elapsed();
    let output = match converted {
        Ok(r) => r
            .content
            .map(restore_underlines)
            .unwrap_or_else(|| html.to_string()),
        Err(e) => {
            log::warn!("html→markdown conversion failed: {e:#}");
            html.to_string()
        }
    };
    log::debug!(
        "email HTML converted in {} ms \
         (prepare_ms={}, converter_ms={}, input_bytes={}, prepared_bytes={}, output_bytes={})",
        started.elapsed().as_millis(),
        prepare_elapsed.as_millis(),
        conversion_elapsed.as_millis(),
        html.len(),
        prepared.len(),
        output.len()
    );
    output
}

const UNDERLINE_OPEN: &str = "AVIARYUNDERLINEOPEN9C4E";
const UNDERLINE_CLOSE: &str = "AVIARYUNDERLINECLOSE9C4E";

/// Markdown has no underline syntax, so the converter removes `<u>` tags.
/// Text sentinels carry them through
/// conversion, then restores them as inline HTML accepted by the
/// pipeline Markdown sortant.
fn preserve_underlines(html: &str) -> String {
    use std::sync::OnceLock;
    static OPEN: OnceLock<regex::Regex> = OnceLock::new();
    static CLOSE: OnceLock<regex::Regex> = OnceLock::new();
    let open = OPEN.get_or_init(|| regex::Regex::new(r"(?i)<\s*u(?:\s[^>]*)?>").unwrap());
    let close = CLOSE.get_or_init(|| regex::Regex::new(r"(?i)<\s*/\s*u\s*>").unwrap());
    let html = open.replace_all(html, UNDERLINE_OPEN);
    close.replace_all(&html, UNDERLINE_CLOSE).into_owned()
}

fn restore_underlines(markdown: String) -> String {
    markdown
        .replace(UNDERLINE_OPEN, "<u>")
        .replace(UNDERLINE_CLOSE, "</u>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_table_is_unwrapped() {
        let html = r#"<table role="presentation"><tr><td>foo</td><td>bar</td></tr></table>"#;
        let out = unwrap_layout_tables(html);
        assert!(
            !out.contains("<table"),
            "table tag should be rewritten: {out}"
        );
        assert!(!out.contains("<tr"), "tr tag should be rewritten: {out}");
        assert!(out.contains("<div>"));
        assert!(out.contains("<span>foo</span>"));
        assert!(out.contains("<span>bar</span>"));
    }

    #[test]
    fn data_table_with_th_is_preserved() {
        let html = "<table><tr><th>Col</th></tr><tr><td>v</td></tr></table>";
        let out = unwrap_layout_tables(html);
        assert_eq!(
            out, html,
            "table with <th> must be left intact for Markdown table rendering"
        );
    }

    #[test]
    fn data_table_with_thead_is_preserved() {
        let html =
            "<table><thead><tr><td>h</td></tr></thead><tbody><tr><td>v</td></tr></tbody></table>";
        let out = unwrap_layout_tables(html);
        assert_eq!(out, html);
    }

    #[test]
    fn outer_layout_with_nested_data_table() {
        // The outer table has no <th> directly, so it is layout. The
        // inner table has <th>, so it stays as a data table.
        let html = "<table><tr><td><table><tr><th>H</th></tr><tr><td>v</td></tr></table></td></tr></table>";
        let out = unwrap_layout_tables(html);
        // Outer unwrapped:
        assert!(out.starts_with("<div><div><span>"));
        // Inner preserved:
        assert!(out.contains("<table><tr><th>H</th></tr><tr><td>v</td></tr></table>"));
    }

    #[test]
    fn preserves_attributes_on_unrelated_tags() {
        // Ensure non-table tags around the table aren't disturbed.
        let html = r#"<p class="x">before</p><table><tr><td>cell</td></tr></table><p>after</p>"#;
        let out = unwrap_layout_tables(html);
        assert!(out.contains(r#"<p class="x">before</p>"#));
        assert!(out.contains("<p>after</p>"));
    }

    #[test]
    fn quoted_attribute_with_gt() {
        // A `>` inside a quoted attribute must not terminate the tag.
        let html = r#"<table title="a > b"><tr><td>x</td></tr></table>"#;
        let out = unwrap_layout_tables(html);
        assert!(!out.contains("<table"));
        assert!(out.contains("<span>x</span>"));
    }

    #[test]
    fn th_does_not_match_thead_prefix() {
        // <thead> must mark the table as data, not get rewritten itself.
        // A bug here would either rewrite <thead> as a span, or fail to
        // detect the table as a data table.
        let html = "<table><thead><tr><td>h</td></tr></thead></table>";
        let out = unwrap_layout_tables(html);
        assert!(out.contains("<thead>"));
    }

    #[test]
    fn synthetic_streaks_block_unwraps_cleanly() {
        // This minimal synthetic fixture covers a layout-table regression. The
        // converter previously turned it into a single-line broken
        // Markdown table (`| Jeustreak icon | Venstreak icon | …`).
        let html = r#"<table class="mobile-streak-table">
            <tr>
                <td><div>Jeu</div><img alt="streak icon"></td>
                <td><div>Ven</div><img alt="streak icon"></td>
            </tr>
        </table>"#;
        let prepared = unwrap_layout_tables(html);
        let md = html_to_markdown_rs::convert(&prepared, Some(html_md_options()))
            .unwrap()
            .content
            .unwrap();
        // No GFM-table separator should leak through.
        assert!(!md.contains(" | --- |"), "broken table separator: {md}");
        assert!(md.contains("Jeu"));
        assert!(md.contains("Ven"));
    }

    #[test]
    fn underline_survives_html_markdown_round_trip() {
        let md = convert_email_html("<p>Before <u>underlined</u> after</p>");

        assert!(md.contains("Before <u>underlined</u> after"), "{md}");
    }
}
