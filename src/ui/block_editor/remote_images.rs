//! Materializes the images of a pasted body.
//!
//! Pasting web content leaves `<img src="https://…">` in the document (or a
//! markdown `![](https://…)` when the clipboard carried no HTML). Sending that
//! untouched hotlinks the image: it breaks for the recipient as soon as the
//! host disappears or requires a session cookie, and it reports back when the
//! mail is opened. Every eligible source is therefore rewritten to a `cid:`
//! reference against an [`InlineImage`] the draft owns — `data:` payloads are
//! decoded on the spot, remote ones are downloaded by the runtime
//! (`Cmd::FetchInlineImage`).
//!
//! **A placeholder must never reach a recipient.** An empty `InlineImage` would
//! be worse than the hotlink it replaced, so the editor keeps the original URL
//! of every in-flight cid: a failed download restores it in the document, and
//! [`BlockEditor::build_outgoing`] puts it back for a mail sent while the
//! download is still running.

use super::{initial_image_width, BlockEditor, EbKind};
use crate::blocks::BlockKind;
use crate::model::InlineImage;
use crate::runtime::Cmd;
use crate::ui::inline_images;
use gpui::{Context, Window};
use std::ops::Range;

/// Cap on how many sources one paste may materialize. A page pasted whole can
/// carry hundreds of tracking pixels and icons; beyond this the remainder stays
/// hotlinked rather than queueing that many downloads.
const MAX_SOURCES_PER_PASTE: usize = 24;

impl BlockEditor {
    /// Rewrites the remote and `data:` image sources of freshly pasted blocks
    /// to `cid:` references, then starts the downloads. Call before importing
    /// `kinds` so the editor never builds blocks pointing at an external host.
    pub(super) fn adopt_pasted_images(&mut self, kinds: &mut [BlockKind], cx: &mut Context<Self>) {
        let mut queued: Vec<(String, String)> = Vec::new();
        let mut adopted = 0usize;
        adopt_in_kinds(kinds, &mut |url| {
            if adopted >= MAX_SOURCES_PER_PASTE {
                return None;
            }
            let cid = self.adopt_source(url, &mut queued)?;
            adopted += 1;
            Some(cid)
        });
        if queued.is_empty() {
            return;
        }
        let Some(tx) = self.runtime_tx.clone() else {
            // No runtime channel (a signature or template editor): the `data:`
            // sources are already materialized, the remote ones cannot be, so
            // give them their URL back instead of leaving empty placeholders.
            for (cid, url) in queued {
                self.pending_remote_images.remove(&cid);
                self.images.retain(|image| image.cid != cid);
                log::debug!("no runtime channel to fetch {url} (cid {cid})");
            }
            return;
        };
        for (cid, url) in queued {
            let _ = tx.send(Cmd::FetchInlineImage {
                editor_id: self.scope.clone(),
                cid,
                url,
            });
        }
        cx.notify();
    }

    /// Allocates the cid backing one source. `data:` bytes are decoded here;
    /// a remote URL is pushed onto `queued` for the caller to request.
    fn adopt_source(&mut self, url: &str, queued: &mut Vec<(String, String)>) -> Option<String> {
        if let Some((mime, bytes)) = decode_data_uri(url) {
            if bytes.is_empty() {
                return None;
            }
            let cid = self.next_image_cid();
            self.images.push(InlineImage {
                cid: cid.clone(),
                mime,
                bytes,
            });
            return Some(cid);
        }
        if !is_fetchable(url) {
            return None;
        }
        let cid = self.next_image_cid();
        self.images.push(InlineImage {
            cid: cid.clone(),
            mime: "image/png".to_string(),
            bytes: Vec::new(),
        });
        self.pending_remote_images
            .insert(cid.clone(), url.to_string());
        queued.push((cid.clone(), url.to_string()));
        Some(cid)
    }

    /// Fills in a downloaded image. Returns whether this editor owned the
    /// request, so the caller can stop routing the event.
    pub(crate) fn apply_fetched_inline_image(
        &mut self,
        editor_id: &str,
        cid: &str,
        bytes: Vec<u8>,
        mime: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scope != editor_id {
            return false;
        }
        if self.pending_remote_images.remove(cid).is_none() {
            // Undone or deleted while in flight: the cid no longer belongs to
            // the document, so its bytes have nowhere to go.
            return true;
        }
        let Some(image) = self.images.iter_mut().find(|image| image.cid == cid) else {
            return true;
        };
        image.mime = mime;
        image.bytes = bytes;
        let path = inline_images::register_bytes(&self.scope, cid, &image.bytes);
        let intrinsic = initial_image_width(&image.bytes);
        for block in &mut self.blocks {
            if let EbKind::Image {
                cid: block_cid,
                width,
                path: block_path,
                ..
            } = &mut block.kind
            {
                if block_cid.as_str() == cid {
                    *block_path = Some(path.clone());
                    *width = width.or(intrinsic);
                }
            }
        }
        cx.notify();
        true
    }

    /// Puts the original URL back after a failed download: the body keeps the
    /// hotlink it was pasted with instead of losing the image.
    pub(crate) fn apply_inline_image_failure(
        &mut self,
        editor_id: &str,
        cid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scope != editor_id {
            return false;
        }
        let Some(url) = self.pending_remote_images.remove(cid) else {
            return true;
        };
        self.images.retain(|image| image.cid != cid);
        self.restore_url_in_document(cid, &url, window, cx);
        cx.notify();
        true
    }

    /// Rewrites every `cid:` reference to this placeholder back to `url`.
    ///
    /// An Image block cannot show a cid it no longer has bytes for, so it
    /// degrades to the markdown paragraph the source came from; the send path
    /// turns that back into an `<img>` tag.
    fn restore_url_in_document(
        &mut self,
        cid: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let needle = format!("cid:{cid}");
        let mut inputs = Vec::new();
        let mut html_rewrites: Vec<(usize, String)> = Vec::new();
        let mut image_blocks: Vec<usize> = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            match &block.kind {
                EbKind::Text(text) => inputs.push(text.input.clone()),
                EbKind::List(list) => inputs.extend(list.rows.iter().map(|row| row.input.clone())),
                EbKind::Table(table) => {
                    inputs.extend(table.rows.iter().flatten().map(|cell| cell.input.clone()));
                }
                EbKind::Image { cid: block_cid, .. } if block_cid.as_str() == cid => {
                    image_blocks.push(index);
                }
                EbKind::Original {
                    kind: BlockKind::RawHtml { html },
                } if html.contains(&needle) => {
                    html_rewrites.push((index, html.replace(&needle, &escape_attr(url))));
                }
                _ => {}
            }
        }
        for input in inputs {
            let value = input.read(cx).value().to_string();
            if value.contains(&needle) {
                let value = value.replace(&needle, url);
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
        for (index, rewritten) in html_rewrites {
            if let Some(EbKind::Original {
                kind: BlockKind::RawHtml { html },
            }) = self.blocks.get_mut(index).map(|block| &mut block.kind)
            {
                *html = rewritten;
            }
        }
        for index in image_blocks {
            let block = self.make_text(
                super::TextStyle::Paragraph,
                format!("![]({url})"),
                "",
                window,
                cx,
            );
            self.blocks[index] = block;
        }
    }
}

/// Rewrites every image source of `kinds` that `adopt` claims, in place.
///
/// `adopt` receives the source with its document escaping already undone and
/// returns the cid replacing it. Split out from
/// [`BlockEditor::adopt_pasted_images`] so the traversal — which decides what a
/// paste may and may not touch — is testable without a gpui context.
fn adopt_in_kinds(kinds: &mut [BlockKind], adopt: &mut impl FnMut(&str) -> Option<String>) {
    for kind in kinds {
        match kind {
            BlockKind::Paragraph(text) | BlockKind::Quote(text) => {
                adopt_in_text(text, SourceSyntax::Markdown, adopt);
            }
            BlockKind::Heading { text, .. } => adopt_in_text(text, SourceSyntax::Markdown, adopt),
            BlockKind::List { items, .. } => {
                for item in items {
                    adopt_in_text(&mut item.text, SourceSyntax::Markdown, adopt);
                }
            }
            BlockKind::Table { rows } => {
                for cell in rows.iter_mut().flatten() {
                    adopt_in_text(cell, SourceSyntax::Markdown, adopt);
                }
            }
            BlockKind::RawHtml { html } => adopt_in_text(html, SourceSyntax::Html, adopt),
            // A code block shows its markup rather than rendering it, `Image`
            // already holds a cid, and an `OriginalMessage` carries its own
            // inline images.
            BlockKind::Code { .. }
            | BlockKind::Image { .. }
            | BlockKind::Divider
            | BlockKind::Signature { .. }
            | BlockKind::OriginalMessage { .. } => {}
        }
    }
}

fn adopt_in_text(
    text: &mut String,
    syntax: SourceSyntax,
    adopt: &mut impl FnMut(&str) -> Option<String>,
) {
    let sources = match syntax {
        SourceSyntax::Markdown => markdown_image_sources(text),
        SourceSyntax::Html => html_image_sources(text),
    };
    let rewritten = replace_ranges(text, &sources, |source| {
        let url = match syntax {
            SourceSyntax::Html => unescape_attr(source),
            SourceSyntax::Markdown => source.to_string(),
        };
        adopt(&url).map(|cid| format!("cid:{cid}"))
    });
    if let Some(rewritten) = rewritten {
        *text = rewritten;
    }
}

/// Which escaping the surrounding document uses for an image source.
#[derive(Clone, Copy)]
enum SourceSyntax {
    Markdown,
    Html,
}

/// Puts the original URL back into an outgoing body for every cid still being
/// downloaded, and drops those placeholders from the attached images.
///
/// This is the guarantee that a send racing a download degrades to the pasted
/// hotlink instead of shipping a zero-byte image.
pub(super) fn restore_pending(
    html: &str,
    images: &mut Vec<InlineImage>,
    pending: &std::collections::HashMap<String, String>,
) -> String {
    // Backstop for a cid whose URL is no longer known — undo dropped the image
    // while it was in flight, then redo brought the placeholder back. The tag
    // stays broken, but no recipient receives a zero-byte attachment.
    images.retain(|image| !image.bytes.is_empty());
    if pending.is_empty() {
        return html.to_string();
    }
    images.retain(|image| !pending.contains_key(&image.cid));
    let sources = html_image_sources(html);
    replace_ranges(html, &sources, |source| {
        let cid = unescape_attr(source).strip_prefix("cid:")?.to_string();
        pending.get(&cid).map(|url| escape_attr(url))
    })
    .unwrap_or_else(|| html.to_string())
}

/// Whether a markdown fragment carries an image this module would adopt.
///
/// The paste path needs this *before* deciding to intercept: a single-paragraph
/// paste is normally left to the input's own handler, but one holding a remote
/// image has to go through the structured path to be materialized.
pub(super) fn markdown_has_adoptable_source(text: &str) -> bool {
    markdown_image_sources(text)
        .into_iter()
        .filter_map(|range| text.get(range))
        .any(|source| is_fetchable(source) || decode_data_uri(source).is_some())
}

/// Whether the runtime can download this source.
fn is_fetchable(url: &str) -> bool {
    let url = url.trim();
    ["http://", "https://"]
        .iter()
        .any(|scheme| url.len() > scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme))
}

/// Byte ranges of the image sources of a markdown fragment.
fn markdown_image_sources(text: &str) -> Vec<Range<usize>> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"!\[[^\]]*\]\(\s*([^()\s]+)").expect("valid markdown image regex")
    });
    re.captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.range()))
        .collect()
}

/// Byte ranges of the `src` attribute values of an HTML fragment.
///
/// The `\s` before `src` is what keeps `data-src` and `srcset` — both common in
/// lazy-loading pages — from being mistaken for the real source.
fn html_image_sources(html: &str) -> Vec<Range<usize>> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?is)<img\b[^>]*?\ssrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#)
            .expect("valid HTML image regex")
    });
    re.captures_iter(html)
        .filter_map(|caps| (1..=3).find_map(|group| caps.get(group)).map(|m| m.range()))
        .collect()
}

/// Rewrites the given ranges, right to left so earlier offsets stay valid.
/// Returns `None` when `replace` declined every range.
fn replace_ranges(
    text: &str,
    ranges: &[Range<usize>],
    mut replace: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let mut out = text.to_string();
    let mut changed = false;
    for range in ranges.iter().rev() {
        let Some(source) = text.get(range.clone()) else {
            continue;
        };
        if let Some(replacement) = replace(source) {
            out.replace_range(range.clone(), &replacement);
            changed = true;
        }
    }
    changed.then_some(out)
}

/// Decodes a `data:` image URI into its MIME type and bytes.
///
/// Only base64 payloads are materialized. A percent-encoded one is almost
/// always inline SVG, which most MUAs refuse to render as an attached image, so
/// it is left in the document rather than turned into something unviewable.
fn decode_data_uri(url: &str) -> Option<(String, Vec<u8>)> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let rest = url
        .trim()
        .strip_prefix("data:")
        .or_else(|| url.trim().strip_prefix("DATA:"))?;
    let (meta, payload) = rest.split_once(',')?;
    let meta = meta.to_ascii_lowercase();
    let mut parts = meta.split(';');
    let mime = parts.next().unwrap_or_default().trim().to_string();
    if !meta.split(';').any(|part| part.trim() == "base64") {
        return None;
    }
    if !mime.starts_with("image/") {
        return None;
    }
    // Wrapped payloads (a `data:` URI that survived a line-wrapping mail
    // client) still decode once the whitespace is gone.
    let payload: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD.decode(payload).ok()?;
    Some((mime, bytes))
}

/// Minimal attribute-value escaping, matching what an `<img src="…">` needs.
fn escape_attr(url: &str) -> String {
    url.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Reverse of [`escape_attr`], plus the two other entities a real page uses for
/// the same characters. `&amp;` is resolved last so `&amp;quot;` in a URL does
/// not decay into a quote.
fn unescape_attr(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn markdown_sources_skip_titles_and_keep_data_uris_whole() {
        let text = "![logo](https://example.test/a.png) et ![](data:image/png;base64,AQL/) \
                    plus ![x](https://example.test/b.png \"titre\")";
        let ranges = markdown_image_sources(text);
        assert_eq!(
            sources(text, &ranges),
            [
                "https://example.test/a.png",
                "data:image/png;base64,AQL/",
                "https://example.test/b.png"
            ]
        );
    }

    #[test]
    fn html_sources_ignore_lazy_loading_attributes() {
        let html = r#"<img data-src="https://example.test/lazy.png" srcset="a.png 2x"
                       src='https://example.test/real.png'><img src=https://example.test/bare.png>"#;
        let ranges = html_image_sources(html);
        assert_eq!(
            sources(html, &ranges),
            [
                "https://example.test/real.png",
                "https://example.test/bare.png"
            ]
        );
    }

    #[test]
    fn ranges_are_rewritten_right_to_left() {
        let text = "![](https://example.test/a.png) ![](https://example.test/bb.png)";
        let ranges = markdown_image_sources(text);
        let mut next = 0;
        let out = replace_ranges(text, &ranges, |_| {
            next += 1;
            Some(format!("cid:c{next}"))
        })
        .expect("both sources rewritten");
        // Numbering runs backwards, which is exactly why the offsets survive.
        assert_eq!(out, "![](cid:c2) ![](cid:c1)");
    }

    #[test]
    fn declined_ranges_leave_the_text_untouched() {
        let text = "![](cid:already) ![](/relative.png)";
        let ranges = markdown_image_sources(text);
        assert!(replace_ranges(text, &ranges, |_| None).is_none());
    }

    #[test]
    fn only_base64_image_payloads_are_decoded() {
        assert_eq!(
            decode_data_uri("data:image/png;base64,AQL/"),
            Some(("image/png".to_string(), vec![1, 2, 255]))
        );
        assert_eq!(
            decode_data_uri("data:image/png;base64,AQ L/\n"),
            Some(("image/png".to_string(), vec![1, 2, 255])),
            "a payload wrapped by a mail client still decodes"
        );
        assert!(decode_data_uri("data:image/svg+xml,%3Csvg%3E").is_none());
        assert!(decode_data_uri("data:text/plain;base64,AQL/").is_none());
        assert!(decode_data_uri("https://example.test/a.png").is_none());
    }

    #[test]
    fn fetchable_schemes_exclude_relative_and_cid_sources() {
        assert!(is_fetchable("https://example.test/a.png"));
        assert!(is_fetchable("HTTP://example.test/a.png"));
        assert!(!is_fetchable("cid:x"));
        assert!(!is_fetchable("/relative.png"));
        assert!(!is_fetchable("https://"));
    }

    /// Sending while a download is in flight must degrade to the pasted
    /// hotlink; an empty inline image would render as nothing at all.
    #[test]
    fn pending_placeholders_are_restored_in_an_outgoing_body() {
        let mut pending = std::collections::HashMap::new();
        pending.insert(
            "e-img-1".to_string(),
            "https://example.test/a.png?x=1&y=2".to_string(),
        );
        let mut images = vec![
            InlineImage {
                cid: "e-img-1".to_string(),
                mime: "image/png".to_string(),
                bytes: Vec::new(),
            },
            InlineImage {
                cid: "e-img-2".to_string(),
                mime: "image/png".to_string(),
                bytes: vec![1, 2, 3],
            },
        ];
        let html = r#"<p><img src="cid:e-img-1"><img src="cid:e-img-2"></p>"#;

        let out = restore_pending(html, &mut images, &pending);

        assert_eq!(
            out,
            r#"<p><img src="https://example.test/a.png?x=1&amp;y=2"><img src="cid:e-img-2"></p>"#
        );
        assert_eq!(
            images.iter().map(|image| &image.cid).collect::<Vec<_>>(),
            ["e-img-2"],
            "only the resolved image stays attached"
        );
    }

    /// The traversal decides what a paste is allowed to touch: every editable
    /// text carrying markdown, the opaque HTML block, and nothing else.
    #[test]
    fn every_editable_text_is_adopted_and_code_is_left_alone() {
        let mut kinds = vec![
            BlockKind::Paragraph("![](https://example.test/p.png)".to_string()),
            BlockKind::Heading {
                level: 1,
                text: "![](https://example.test/h.png)".to_string(),
            },
            BlockKind::Quote("![](https://example.test/q.png)".to_string()),
            BlockKind::List {
                ordered: false,
                items: vec![crate::blocks::ListItem {
                    id: 1,
                    indent: 0,
                    text: "![](https://example.test/l.png)".to_string(),
                }],
            },
            BlockKind::Table {
                rows: vec![vec!["![](https://example.test/t.png)".to_string()]],
            },
            BlockKind::RawHtml {
                html: r#"<img src="https://example.test/r.png?a=1&amp;b=2">"#.to_string(),
            },
            BlockKind::Code {
                language: "md".to_string(),
                text: "![](https://example.test/c.png)".to_string(),
            },
        ];

        let mut seen = Vec::new();
        adopt_in_kinds(&mut kinds, &mut |url| {
            seen.push(url.to_string());
            Some(format!("c{}", seen.len()))
        });

        assert_eq!(
            seen,
            [
                "https://example.test/p.png",
                "https://example.test/h.png",
                "https://example.test/q.png",
                "https://example.test/l.png",
                "https://example.test/t.png",
                // Unescaped for the fetch: `&amp;` would not resolve over HTTP.
                "https://example.test/r.png?a=1&b=2",
            ],
            "a code block keeps its markup"
        );
        assert_eq!(kinds[0], BlockKind::Paragraph("![](cid:c1)".to_string()));
        assert_eq!(
            kinds[5],
            BlockKind::RawHtml {
                html: r#"<img src="cid:c6">"#.to_string()
            }
        );
        assert_eq!(
            kinds[6],
            BlockKind::Code {
                language: "md".to_string(),
                text: "![](https://example.test/c.png)".to_string()
            }
        );
    }

    #[test]
    fn a_declined_source_stays_as_pasted() {
        let mut kinds = vec![BlockKind::Paragraph(
            "![](https://example.test/a.png)".to_string(),
        )];
        adopt_in_kinds(&mut kinds, &mut |_| None);
        assert_eq!(
            kinds[0],
            BlockKind::Paragraph("![](https://example.test/a.png)".to_string())
        );
    }

    #[test]
    fn attribute_escaping_round_trips() {
        let url = "https://example.test/a.png?x=1&y=2";
        assert_eq!(unescape_attr(&escape_attr(url)), url);
        assert_eq!(
            unescape_attr("&amp;quot;"),
            "&quot;",
            "`&amp;` resolves last, so an escaped entity stays escaped"
        );
    }
}
