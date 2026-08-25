//! Block-document copy/cut/paste and internal image sidecar.

use super::{remote_images, BlockEditor, CopySelection, CutSelection, EbKind, TextStyle};
use crate::{
    blocks::{self, Block, BlockKind},
    model::InlineImage,
    ui::rich_clipboard,
};
use gpui::{App, ClipboardEntry, ClipboardItem, Context, Window};

/// The system clipboard carries only Markdown on Linux. This sidecar restores
/// inline images when pasted text matches the latest
/// internal copy, including between two composers in the same process.
type ClipImagesStore = std::sync::Mutex<Option<(String, Vec<InlineImage>)>>;

static CLIP_IMAGES: std::sync::OnceLock<ClipImagesStore> = std::sync::OnceLock::new();

fn clip_images() -> &'static ClipImagesStore {
    CLIP_IMAGES.get_or_init(|| std::sync::Mutex::new(None))
}

impl BlockEditor {
    /// Markdown for the whole-block selection, intended for the clipboard.
    fn selection_markdown(&self, cx: &App) -> Option<String> {
        let (lo, hi) = self.sel_range()?;
        let blocks: Vec<Block> = self.blocks[lo..=hi]
            .iter()
            .filter_map(|block| self.export_block(block, cx))
            .collect();
        Some(blocks::blocks_to_markdown(&blocks))
    }

    /// Copies Markdown to the system clipboard and retains referenced inline
    /// images in the internal sidecar.
    fn copy_selection_to_clipboard(&self, cx: &App) -> bool {
        let Some(markdown) = self.selection_markdown(cx) else {
            return false;
        };
        let referenced: Vec<InlineImage> = self
            .images
            .iter()
            .filter(|image| {
                markdown.contains(&format!("cid:{}", image.cid))
                    || markdown.contains(&format!("bytes://cid-{}", image.cid))
            })
            .cloned()
            .collect();
        *clip_images().lock().expect("clipboard sidecar") = if referenced.is_empty() {
            None
        } else {
            Some((markdown.clone(), referenced))
        };
        rich_clipboard::clear();
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
        true
    }

    pub(super) fn on_copy_selection(
        &mut self,
        _: &CopySelection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_selection_to_clipboard(cx);
    }

    pub(super) fn on_cut_selection(
        &mut self,
        _: &CutSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.copy_selection_to_clipboard(cx) {
            self.remove_selection(window, cx);
        }
    }

    /// Pastes into a paragraph. A bitmap becomes an image block; multi-block
    /// Markdown splits the paragraph into actual blocks.
    pub(super) fn on_paste(
        &mut self,
        bid: u64,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // A pasted URL becomes a link. This runs before the structured path
        // below, which handles paragraphs only: a link is just as expected in a
        // heading, a quote or a list row.
        if let Some(text) = item.text() {
            if let Some(input) = self.focused_input(window, cx) {
                if self.paste_as_link(&input, &text, window, cx) {
                    cx.stop_propagation();
                    return;
                }
            }
        }
        if row.is_some() {
            return;
        }
        let Some(index) = self.block_ix(bid) else {
            return;
        };
        let input = match &self.blocks[index].kind {
            EbKind::Text(text) if text.style == TextStyle::Paragraph => text.input.clone(),
            _ => return,
        };

        let pasted_images: Vec<(String, Vec<u8>)> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => {
                    Some((image.format.mime_type().to_string(), image.bytes.clone()))
                }
                _ => None,
            })
            .collect();
        if !pasted_images.is_empty() {
            cx.stop_propagation();
            self.push_undo(cx);
            let mut at = index + 1;
            for (mime, bytes) in pasted_images {
                let block = self.import_image_bytes(mime, bytes);
                self.blocks.insert(at, block);
                at += 1;
            }
            let tail = self.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
            if let EbKind::Text(text) = &tail.kind {
                text.input.update(cx, |state, cx| state.focus(window, cx));
            }
            self.blocks.insert(at, tail);
            cx.notify();
            return;
        }

        let Some(text) = item.text() else {
            return;
        };
        let rich = rich_clipboard::read(&item);
        let mut pasted_html = rich.is_some() || looks_like_html(&text);
        let sidecar: Vec<InlineImage> = rich
            .as_ref()
            .map(|content| content.images.clone())
            .unwrap_or_else(|| {
                clip_images()
                    .lock()
                    .expect("clipboard sidecar")
                    .as_ref()
                    .filter(|(markdown, _)| *markdown == text)
                    .map(|(_, images)| images.clone())
                    .unwrap_or_default()
            });
        let mut kinds = if pasted_html {
            let html = rich
                .as_ref()
                .map(|content| content.html.clone())
                .unwrap_or_else(|| text.clone());
            match editable_html_paste_kinds(&html, &text, &sidecar) {
                Some(kinds) => kinds,
                // Rien d'éditable dans le fragment : son texte en clair rend
                // mieux service que le bloc opaque.
                None => {
                    pasted_html = false;
                    blocks::markdown_to_blocks(&text)
                }
            }
        } else {
            blocks::markdown_to_blocks(&text)
        };
        // A single paragraph of pasted HTML — a sentence copied from a read
        // message — belongs at the cursor, not in a block of its own below
        // the current one. Images still need the structured path (adoption,
        // registry).
        if pasted_html {
            if let [BlockKind::Paragraph(markdown)] = kinds.as_slice() {
                if sidecar.is_empty()
                    && !markdown.contains("cid:")
                    && !remote_images::markdown_has_adoptable_source(markdown)
                {
                    cx.stop_propagation();
                    self.push_undo(cx);
                    let markdown = markdown.clone();
                    input.update(cx, |state, cx| state.insert(&markdown, window, cx));
                    cx.notify();
                    return;
                }
            }
        }
        if !pasted_html && kinds.len() < 2 {
            let known_image = kinds.first().is_some_and(|kind| match kind {
                BlockKind::Paragraph(text) => {
                    standalone_image_cid(text).is_some_and(|cid| {
                        sidecar.iter().any(|image| image.cid == cid)
                            || self.images.iter().any(|image| image.cid == cid)
                    })
                    // A remote image needs the structured path to be adopted;
                    // the input's own handler would paste its markdown as text.
                    || remote_images::markdown_has_adoptable_source(text)
                }
                _ => false,
            });
            if !known_image {
                return;
            }
        }

        cx.stop_propagation();
        self.push_undo(cx);
        for image in sidecar {
            if !self.images.iter().any(|existing| existing.cid == image.cid) {
                self.images.push(image);
            }
        }
        // Adopt external sources before the blocks are built, so no block ever
        // points at a remote host.
        self.adopt_pasted_images(&mut kinds, cx);
        let (value, cursor) = {
            let state = input.read(cx);
            let value = state.text().to_string();
            let cursor = state.cursor().min(value.len());
            (value, cursor)
        };
        let before = value[..cursor].to_string();
        let after = value[cursor..].to_string();
        input.update(cx, |state, cx| state.set_value(before, window, cx));

        let imported: Vec<_> = kinds
            .into_iter()
            .map(|kind| self.import_kind(kind, window, cx))
            .collect();
        let mut at = index + 1;
        for block in imported {
            self.blocks.insert(at, block);
            at += 1;
        }
        let tail = self.make_text(TextStyle::Paragraph, after, "", window, cx);
        if let EbKind::Text(text) = &tail.kind {
            Self::focus_at(&text.input, 0, window, cx);
        }
        self.blocks.insert(at, tail);
        cx.notify();
    }
}

/// Blocs à coller pour un presse-papiers HTML, ou `None` quand le fragment
/// est resté **entièrement opaque** (que des `RawHtml`) alors que le texte en
/// clair du presse-papiers n'est pas lui-même du HTML : le vrai contenu — ce
/// que l'utilisateur voyait sélectionné — est alors ce texte, et le coller
/// éditable rend mieux service qu'un bloc « Fragment HTML » infidèle par
/// construction (le fragment copié peut être un conteneur de mise en page
/// bien plus large que la sélection).
fn editable_html_paste_kinds(
    html: &str,
    text: &str,
    images: &[crate::model::InlineImage],
) -> Option<Vec<BlockKind>> {
    let kinds = html_paste_kinds(html, images);
    let only_opaque = kinds
        .iter()
        .all(|kind| matches!(kind, BlockKind::RawHtml { .. }));
    if only_opaque && !text.trim().is_empty() && !looks_like_html(text) {
        return None;
    }
    Some(kinds)
}

/// Pasted HTML becomes real editable blocks — the same return path a draft
/// takes when it comes back from the provider. What the editor can hold
/// (paragraphs, headings, lists, quotes, tables, `cid:` images) is editable
/// in place; what it cannot stays an opaque `RawHtml` fragment, and an HTML
/// body `scraper` cannot make anything of falls back to one such fragment
/// rather than pasting nothing.
fn html_paste_kinds(html: &str, images: &[crate::model::InlineImage]) -> Vec<BlockKind> {
    use std::hash::{Hash, Hasher};
    // Namespaces the `bytes://` preview cache of faithful sub-blocks; two
    // pastes of different originals must not collide on identical CIDs.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    html.hash(&mut hasher);
    let source_id = format!("paste-{:016x}", hasher.finish());
    let kinds = blocks::html_to_blocks(
        html,
        &blocks::HtmlImport {
            inline_images: images,
            source_id: &source_id,
        },
    );
    if kinds.is_empty() {
        vec![BlockKind::RawHtml {
            html: html.to_string(),
        }]
    } else {
        kinds
    }
}

/// A paragraph containing only `![](cid:x)` or `![](bytes://cid-x)` becomes a CID.
pub(super) fn standalone_image_cid(text: &str) -> Option<String> {
    let text = text.trim();
    let inner = text.strip_prefix("![](")?.strip_suffix(')')?;
    if inner.contains(' ') || inner.contains('(') {
        return None;
    }
    inner
        .strip_prefix("cid:")
        .or_else(|| inner.strip_prefix("bytes://cid-"))
        .map(str::to_string)
}

/// Detects explicit HTML without mistaking `<name@example.org>` for an
/// HTML tag.
fn looks_like_html(text: &str) -> bool {
    static HTML_TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    HTML_TAG
        .get_or_init(|| {
            regex::Regex::new(
                r"(?is)<\s*/?\s*(?:html|body|div|span|p|br|strong|b|em|i|u|a|img|table|thead|tbody|tfoot|tr|td|th|ul|ol|li|h[1-6]|blockquote|hr|style)\b",
            )
            .expect("valid HTML detection regex")
        })
        .is_match(text)
}

#[cfg(test)]
mod tests {
    use super::{editable_html_paste_kinds, html_paste_kinds, looks_like_html};
    use crate::blocks::BlockKind;

    #[test]
    fn detects_html_fragments_without_treating_addresses_as_tags() {
        assert!(looks_like_html(
            r#"<table><tr><td style="color:red">Signature</td></tr></table>"#
        ));
        assert!(looks_like_html("<div>Signature</div>"));
        assert!(!looks_like_html("Contact <nom@example.org>"));
        assert!(!looks_like_html("2 < 3 et 4 > 1"));
    }

    /// Un HTML simple collé — typiquement copié d'un courriel envoyé par
    /// Aviary lui-même — doit devenir de vrais blocs éditables, pas un
    /// fragment opaque.
    #[test]
    fn simple_pasted_html_becomes_editable_blocks() {
        let kinds = html_paste_kinds(
            "<h2>Titre</h2><p>Un <strong>texte</strong> modifiable.</p>\
             <ul><li>a</li><li>b</li></ul>",
            &[],
        );
        assert!(matches!(
            kinds.as_slice(),
            [
                BlockKind::Heading { level: 2, .. },
                BlockKind::Paragraph(_),
                BlockKind::List { .. },
            ]
        ));
        let BlockKind::Paragraph(markdown) = &kinds[1] else {
            unreachable!();
        };
        assert_eq!(markdown, "Un **texte** modifiable.");
    }

    /// Une image `cid:` du HTML collé devient un bloc image (rendu par le
    /// registre de l'éditeur), pas un lien mort.
    #[test]
    fn pasted_html_cid_image_becomes_an_image_block() {
        let kinds = html_paste_kinds(r#"<p><img src="cid:logo@example"></p>"#, &[]);
        assert!(matches!(
            kinds.as_slice(),
            [BlockKind::Image { cid, .. }] if cid == "logo@example"
        ));
    }

    /// Un HTML dont rien n'est reconstructible retombe sur un fragment opaque
    /// plutôt que de coller du vide.
    #[test]
    fn unparseable_pasted_html_falls_back_to_a_raw_fragment() {
        let html = "<style>.a{color:red}</style>";
        let kinds = html_paste_kinds(html, &[]);
        assert!(matches!(
            kinds.as_slice(),
            [BlockKind::RawHtml { html: kept }] if kept == html
        ));
    }

    /// Un fragment resté entièrement opaque alors que le presse-papiers porte
    /// un texte en clair ordinaire — une phrase sélectionnée dont la copie a
    /// embarqué son conteneur de mise en page — doit se coller comme ce
    /// texte, pas comme un bloc « Fragment HTML » inéditable.
    #[test]
    fn opaque_only_fragment_with_plain_text_degrades_to_the_text() {
        let layout_soup = r#"<table><tr>
            <td><table><tr><td><p>une bête phrase</p></td></tr></table></td>
            <td><img src="https://tracking.example/pixel"></td>
        </tr></table>"#;
        assert!(editable_html_paste_kinds(layout_soup, "une bête phrase", &[]).is_none());
        // Du HTML collé *en tant que texte source* reste en revanche un
        // fragment fidèle : le texte du presse-papiers est le HTML lui-même.
        assert!(editable_html_paste_kinds(layout_soup, layout_soup, &[]).is_some());
        // Et un fragment qui a produit au moins un bloc éditable est gardé.
        assert!(
            editable_html_paste_kinds("<p>une bête phrase</p>", "une bête phrase", &[]).is_some()
        );
    }
}
