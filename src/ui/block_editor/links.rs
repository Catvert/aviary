//! Hyperlinks in the block editor.
//!
//! A link is markdown inside the block's own text: `[label](url)`. Nothing new
//! is persisted and nothing new is serialized — `build_html_body` already turns
//! it into an `<a href>`, and `html_to_markdown_rs` produces that same form for
//! the links of a quoted message. What this module adds is the editing side:
//! recognizing the link under the cursor, and rendering a (label, url) pair back
//! to markdown.
//!
//! The labelled form is **always** what gets written, even when the label is
//! only the address again. `<url>` would look identical once folded, but its
//! visible text *is* its destination: renaming it in place would break the link,
//! where a label can simply be typed over.
//!
//! Bare URLs are deliberately *not* auto-detected in the text. pulldown-cmark
//! implements CommonMark, which linkifies `<url>` but not a bare `url`, so
//! anything this module accepted without the angle brackets would be styled as
//! a link in the editor and then sent as plain text. The paste path adds them
//! instead, which keeps one syntax as the single source of truth.

use super::{BlockEditor, InsertLink};
use crate::ui::components::display_map::FoldableRange;
use gpui::{prelude::*, Context, Entity, Window};
use gpui_component::{
    input::{Input, InputState as TextField},
    v_flex, ActiveTheme as _, WindowExt as _,
};
use std::ops::Range;

impl BlockEditor {
    pub(super) fn on_insert_link(
        &mut self,
        _: &InsertLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_link_dialog(window, cx);
    }

    /// Opens the link dialog for the focused block: it edits the link under the
    /// cursor when there is one, otherwise it wraps the selection (or inserts at
    /// the caret).
    pub(crate) fn open_link_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.focused_input(window, cx) else {
            return;
        };
        let (value, selection, cursor) = {
            let state = input.read(cx);
            (
                state.value().to_string(),
                state.selection_range(),
                state.cursor(),
            )
        };
        let start = selection.start.min(value.len());
        let end = selection.end.min(value.len()).max(start);
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) {
            return;
        }
        let existing = link_at(&value, cursor.min(value.len()));
        let editing = existing.is_some();
        let (range, label, url) = match existing {
            Some(span) => (span.range, span.label, span.url),
            None => (start..end, value[start..end].to_string(), String::new()),
        };

        let label_field = cx.new(|cx| {
            TextField::new(window, cx)
                .placeholder(tr!("compose-link-text-hint"))
                .default_value(label)
        });
        let url_field = cx.new(|cx| {
            TextField::new(window, cx)
                .placeholder(tr!("compose-link-url-hint"))
                .default_value(url)
        });
        // The URL is what the user came to type; the label is often already
        // filled in from the selection.
        url_field.update(cx, |state, cx| state.focus(window, cx));

        let editor = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let editor = editor.clone();
            let input = input.clone();
            let label_field = label_field.clone();
            let url_field = url_field.clone();
            let range = range.clone();
            dialog
                .title(match editing {
                    true => tr!("compose-link-edit-title"),
                    false => tr!("compose-link-title"),
                })
                .confirm()
                .child(
                    v_flex()
                        .gap_2()
                        .child(field_label(tr!("compose-link-text"), _cx))
                        .child(Input::new(&label_field))
                        .child(field_label(tr!("compose-link-url"), _cx))
                        .child(Input::new(&url_field)),
                )
                .on_ok(move |_, window, cx| {
                    let label = label_field.read(cx).value().to_string();
                    let url = url_field.read(cx).value().to_string();
                    editor.update(cx, |this, cx| {
                        this.apply_link(&input, range.clone(), &label, &url, window, cx);
                    });
                    true
                })
        });
    }

    /// Replaces `range` with the markdown for (label, url). An empty URL removes
    /// the link, keeping its label as plain text — the only way out of a link
    /// that does not involve deleting the words.
    fn apply_link(
        &mut self,
        input: &Entity<super::InputState>,
        range: Range<usize>,
        label: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = input.read(cx).value().to_string();
        let start = range.start.min(value.len());
        let end = range.end.min(value.len()).max(start);
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) {
            return;
        }
        let replacement = match render_link(label, url) {
            Some(markdown) => markdown,
            // No URL: unwrap to the label, or to what was already there.
            None => match label.trim().is_empty() {
                true => value[start..end].to_string(),
                false => label.trim().to_string(),
            },
        };
        self.push_undo(cx);
        let mut next = value;
        next.replace_range(start..end, &replacement);
        let cursor = start + replacement.len();
        input.update(cx, |state, cx| {
            state.set_value(next, window, cx);
            state.set_cursor_offset(cursor, window, cx);
        });
        cx.notify();
    }

    /// Turns a pasted URL into a link: wrapping the selection when there is one,
    /// otherwise inserting the URL as its own label. Returns whether it handled
    /// the paste.
    pub(super) fn paste_as_link(
        &mut self,
        input: &Entity<super::InputState>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(url) = sole_url(text) else {
            return false;
        };
        let (value, selection) = {
            let state = input.read(cx);
            (state.value().to_string(), state.selection_range())
        };
        let start = selection.start.min(value.len());
        let end = selection.end.min(value.len()).max(start);
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) {
            return false;
        }
        // Pasting a URL over a link replaces its destination and keeps the label.
        let (range, label) = match link_at(&value, start) {
            Some(span) if start == end => (span.range, span.label),
            _ => (start..end, value[start..end].to_string()),
        };
        self.apply_link(input, range, &label, url, window, cx);
        true
    }
}

fn field_label(text: impl Into<gpui::SharedString>, cx: &gpui::App) -> impl IntoElement {
    gpui::div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// The markdown link occupying `range` in a block's text.
pub(super) struct LinkSpan {
    pub range: Range<usize>,
    /// Empty for an autolink (`<url>`), whose label *is* its URL.
    pub label: String,
    pub url: String,
}

/// Finds the link containing `offset`, cursor at either edge included — Ctrl+K
/// with the caret just past a link edits that link rather than opening an empty
/// dialog next to it.
pub(super) fn link_at(text: &str, offset: usize) -> Option<LinkSpan> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

    for (event, range) in Parser::new_ext(text, Options::empty()).into_offset_iter() {
        let Event::Start(Tag::Link {
            link_type,
            dest_url,
            ..
        }) = event
        else {
            continue;
        };
        if offset < range.start || offset > range.end {
            continue;
        }
        let label = match link_type {
            LinkType::Autolink | LinkType::Email => String::new(),
            _ => label_range(text, &range)
                .map(|label| unescape_label(&text[label]))
                .unwrap_or_default(),
        };
        return Some(LinkSpan {
            range,
            label,
            url: dest_url.to_string(),
        });
    }
    None
}

/// The label of `[label](url)`, i.e. what sits between the outer brackets.
///
/// Tracks nesting so `[a [b] c](url)` keeps its whole label, and skips
/// backslash-escaped brackets, which are literal text.
pub(super) fn label_range(text: &str, source: &Range<usize>) -> Option<Range<usize>> {
    let inner = text.get(source.clone())?;
    if !inner.starts_with('[') {
        return None;
    }
    let mut depth = 0usize;
    let mut escaped = false;
    for (offset, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(source.start + 1..source.start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Markdown for a link, picking the form that shows the least syntax.
pub(super) fn render_link(label: &str, url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // Angle brackets would terminate an angle-bracket destination. Encoding them
    // keeps the URL usable, where dropping them would corrupt it silently.
    let url = url.replace('<', "%3C").replace('>', "%3E");
    let label = label.trim();
    // An empty label shows the address — but as a real label, never as `<url>`.
    // That is the whole point: a label is editable in place (select it, type over
    // it) where the text of an autolink *is* its destination and cannot be
    // renamed without breaking the link.
    let label = match label.is_empty() {
        true => url.as_str(),
        false => label,
    };
    // Only these need the angle-bracket destination form.
    let destination = match url.contains([' ', '(', ')']) {
        true => format!("<{url}>"),
        false => url.clone(),
    };
    Some(format!("[{}]({destination})", escape_label(label)))
}

/// The parts of every link in `text` that the input may hide: the brackets and
/// the destination, leaving the label alone on screen.
///
/// The editor computes them because it is the side that understands markdown;
/// the input decides when to apply them, because it is the side that knows where
/// the caret is.
pub(super) fn foldable_ranges(text: &str) -> Vec<FoldableRange> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

    let mut folds = Vec::new();
    for (event, range) in Parser::new_ext(text, Options::empty()).into_offset_iter() {
        let Event::Start(Tag::Link { link_type, .. }) = event else {
            continue;
        };
        let inner = match link_type {
            LinkType::Autolink | LinkType::Email => {
                range.start.saturating_add(1)..range.end.saturating_sub(1)
            }
            _ => match label_range(text, &range) {
                Some(label) => label,
                None => continue,
            },
        };
        if inner.start >= inner.end {
            continue;
        }
        // Hiding an empty label would leave a link with nothing to click.
        for hidden in [range.start..inner.start, inner.end..range.end] {
            if hidden.start < hidden.end {
                folds.push(FoldableRange {
                    hidden,
                    reveal: range.clone(),
                });
            }
        }
    }
    folds
}

/// A single URL, when that is all `text` holds. Used by the paste path to
/// decide between inserting text and inserting a link.
pub(super) fn sole_url(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    // Already markdown: leave it to the normal paste path.
    if text.contains("](") || text.starts_with('<') {
        return None;
    }
    let known = ["http://", "https://", "mailto:"].iter().any(|scheme| {
        text.len() > scheme.len() && text[..scheme.len()].eq_ignore_ascii_case(scheme)
    });
    known.then_some(text)
}

/// Brackets inside a label would close it early.
fn escape_label(label: &str) -> String {
    label
        .replace('\\', r"\\")
        .replace('[', r"\[")
        .replace(']', r"\]")
}

fn unescape_label(label: &str) -> String {
    label
        .replace(r"\[", "[")
        .replace(r"\]", "]")
        .replace(r"\\", r"\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_link_under_the_cursor_with_its_label() {
        let text = "avant [Contact A](https://example.test/a) après";
        let span = link_at(text, 10).expect("cursor inside the label");
        assert_eq!(span.label, "Contact A");
        assert_eq!(span.url, "https://example.test/a");
        assert_eq!(&text[span.range], "[Contact A](https://example.test/a)");
    }

    /// The caret just past `)` still edits that link — otherwise Ctrl+K right
    /// after typing one would offer to insert a second.
    #[test]
    fn both_edges_of_a_link_belong_to_it() {
        let text = "[a](https://example.test/a)";
        assert!(link_at(text, 0).is_some());
        assert!(link_at(text, text.len()).is_some());
        assert!(link_at("plain text", 4).is_none());
    }

    #[test]
    fn an_autolink_reports_no_label() {
        let span = link_at("voir <https://example.test/a> ici", 10).expect("autolink");
        assert_eq!(span.label, "");
        assert_eq!(span.url, "https://example.test/a");
        assert_eq!(span.range, 5..29);
    }

    #[test]
    fn a_nested_label_is_kept_whole() {
        let span = link_at("[a [b] c](https://example.test/a)", 2).expect("nested label");
        assert_eq!(span.label, "a [b] c");
    }

    /// Even without a label, the labelled form is produced: `<url>` would show
    /// the same thing but could not be renamed in place.
    #[test]
    fn an_empty_label_falls_back_to_the_address_not_to_an_autolink() {
        let url = "https://example.test/a";
        assert_eq!(render_link("", url).unwrap(), format!("[{url}]({url})"));
        assert_eq!(render_link(url, url).unwrap(), format!("[{url}]({url})"));
        assert_eq!(
            render_link("Contact A", url).unwrap(),
            format!("[Contact A]({url})")
        );
        assert!(render_link("Contact A", "   ").is_none());
    }

    #[test]
    fn a_destination_with_a_space_uses_angle_brackets() {
        assert_eq!(
            render_link("A", "https://example.test/a b").unwrap(),
            "[A](<https://example.test/a b>)"
        );
    }

    /// A label holding a bracket would close the link early; the escape has to
    /// survive the round trip so editing twice does not accumulate backslashes.
    #[test]
    fn label_brackets_round_trip() {
        let markdown = render_link("a [b] c", "https://example.test/a").unwrap();
        assert_eq!(markdown, r"[a \[b\] c](https://example.test/a)");
        let span = link_at(&markdown, 1).expect("escaped label parses back");
        assert_eq!(span.label, "a [b] c");
    }

    /// The two halves of the feature have to agree: what the editor declares
    /// foldable is what the input hides. Asserting on the resulting display text
    /// is what catches an off-by-one that reasoning about offsets would miss.
    #[test]
    fn folding_a_link_leaves_only_its_label_on_screen() {
        use crate::ui::components::display_map::DisplayMap;

        for (source, folded) in [
            ("voir [Aviary](https://example.test) ici", "voir Aviary ici"),
            (
                "voir <https://example.test> ici",
                "voir https://example.test ici",
            ),
            (
                "voir [a](https://example.test/a) et [b](https://example.test/b) ici",
                "voir a et b ici",
            ),
            ("aucun lien ici", "aucun lien ici"),
        ] {
            let folds = foldable_ranges(source);
            // Caret at the start: no link here begins there, so none is
            // revealed. Every case keeps text on both sides for that reason.
            let map = DisplayMap::new(source, &folds, Some(&(0..0)));
            assert_eq!(map.display_text(source), folded, "source: {source}");
        }
    }

    /// A caret resting against a link reveals it, which is what makes editing
    /// possible at all — and is why the case above keeps the caret away from one.
    #[test]
    fn a_caret_against_a_link_reveals_it() {
        use crate::ui::components::display_map::DisplayMap;

        let source = "[a](https://example.test/a)";
        let folds = foldable_ranges(source);
        for caret in [0, source.len()] {
            let map = DisplayMap::new(source, &folds, Some(&(caret..caret)));
            assert_eq!(map.display_text(source), source, "caret {caret}");
        }
    }

    #[test]
    fn a_link_without_a_label_is_left_visible() {
        // Hiding both sides would leave nothing to click or select.
        assert!(foldable_ranges("[](https://example.test)").is_empty());
    }

    #[test]
    fn only_a_bare_single_url_is_treated_as_pasted_link() {
        assert_eq!(
            sole_url(" https://example.test/a \n"),
            Some("https://example.test/a")
        );
        assert_eq!(
            sole_url("mailto:contact@example.test"),
            Some("mailto:contact@example.test")
        );
        assert_eq!(sole_url("HTTP://example.test"), Some("HTTP://example.test"));
        assert!(sole_url("https://example.test/a et du texte").is_none());
        assert!(sole_url("[a](https://example.test/a)").is_none());
        assert!(sole_url("<https://example.test/a>").is_none());
        assert!(sole_url("example.test").is_none(), "no scheme, no autolink");
        assert!(sole_url("").is_none());
    }
}
