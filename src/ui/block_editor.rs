//! Block-based WYSIWYG message-body editor inspired by Notion. The document is
//! a list of blocks (`blocks::BlockKind`); each text block is a styled,
//! auto-growing multiline `Entity<InputState>`, so formatting remains visible
//! while typing.
//!
//! Interactions:
//! - **Enter** splits the block at the cursor (the `Enter` action is captured
//!   on an input ancestor before the input inserts `\n`). In a list it creates
//!   the next item, while an empty item exits the list. In quote/code blocks,
//!   Enter inserts a line, and Enter on a final empty line exits the block.
//! - **Backspace at offset 0** converts a heading/quote/code block back to a
//!   paragraph, merges a paragraph with the preceding block, or outdents/exits
//!   a list item.
//! - **Tab / Shift-Tab** move through fields from a text block, indent/outdent
//!   a list item, and change table cells (captured `IndentInline` and
//!   `OutdentInline` actions).
//! - **Markdown prefixes** at the beginning of a block (`# `, `## `, `### `,
//!   `> `, `- `, `1. `, ```` ``` ````, `---`) transform the paragraph as the
//!   user types, as in Notion.
//! - **Up/Down** at block boundaries move to the neighboring block.
//! - Each block has a menu (a `⋮` handle visible on hover) to transform, move,
//!   insert a paragraph, or delete.
//!
//! The editor owns the draft's inline images (`images()`): `Image` blocks are
//! rendered from the `ui/inline_images.rs` registry, and deleting an image
//! block removes the attachment if nothing else references it.

mod clipboard;
mod emoji;
mod history;
mod input_actions;
mod links;
mod mentions;
mod proofreading;
mod remote_images;
mod render;
mod tables;
mod toolbar;

use self::history::Snapshot;
use self::proofreading::{
    hunspell_is_suppressed, language_issue_at, LanguageToolResult, SpellingResult,
};

use self::clipboard::standalone_image_cid;
use super::{
    addresses::{AddressBook, RecipientInput},
    attachments::image_mime_for_path,
    components::block_input::{BlockInputEvent as InputEvent, BlockInputState as InputState},
    inline_images,
    settings::MailBodyOptions,
    spellcheck,
};
use crate::blocks::{self, Block, BlockKind, ListItem};
use crate::model::InlineImage;
use crate::proofreading::{
    LanguageToolCoverage, LanguageToolMode, LanguageToolSettings, ProofreadingCategory,
    ProofreadingIssue,
};
use crate::runtime::Cmd;
use gpui::{
    actions, prelude::*, px, App, Entity, EntityId, FocusHandle, Focusable as _, FontStyle,
    FontWeight, HighlightStyle, KeyBinding, Pixels, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    StrikethroughStyle, Subscription, UnderlineStyle, Window,
};
use gpui_component::menu::PopupMenuItem;
use gpui_component::ActiveTheme as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use tokio::sync::mpsc;

actions!(
    block_editor,
    [
        CopySelection,
        CutSelection,
        DeleteSelection,
        CancelSelection,
        SelectAllBlocks,
        SelectPrevBlock,
        SelectNextBlock,
        FocusSelectedBlock,
        UndoBlocks,
        RedoBlocks,
        InsertLink
    ]
);

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = block_editor, no_json)]
struct ApplySpellingSuggestion {
    input_id: EntityId,
    range: std::ops::Range<usize>,
    original: String,
    replacement: String,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = block_editor, no_json)]
struct IgnoreSpelling {
    word: String,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = block_editor, no_json)]
struct AddSpellingToDictionary {
    word: String,
}

#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = block_editor, no_json)]
struct IgnoreProofreadingRule {
    rule_id: String,
}

#[derive(Clone, Copy)]
enum EditorInitialization {
    Document { lead_blank: bool },
    TemplateDefinition,
}

/// Container keyboard context, active while a block selection has focus.
/// Inputs are deeper in the tree and retain priority while focused.
const CONTEXT: &str = "BlockEditor";
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const ZOOM_LINE_STEP: f32 = 0.1;
const IMAGE_DEFAULT_MAX_HEIGHT: f32 = 360.0;
const IMAGE_MAX_WIDTH: f32 = 1600.0;
/// Extra room to the right of a fixed-width image for the block menu,
/// inter-element gaps, and the resize handle.
const IMAGE_SCROLL_CHROME: f32 = 64.0;

fn fitted_image_width(width: u32, height: u32) -> Option<u32> {
    if width == 0 || height == 0 {
        return None;
    }
    let height_scale = (IMAGE_DEFAULT_MAX_HEIGHT / height as f32).min(1.0);
    Some(
        (width as f32 * height_scale)
            .round()
            .clamp(1.0, IMAGE_MAX_WIDTH) as u32,
    )
}

fn initial_image_width(bytes: &[u8]) -> Option<u32> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    fitted_image_width(width, height)
}

/// Call once at startup (see `ui::run`).
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-c", CopySelection, Some(CONTEXT)),
        KeyBinding::new("secondary-x", CutSelection, Some(CONTEXT)),
        KeyBinding::new("secondary-a", SelectAllBlocks, Some(CONTEXT)),
        KeyBinding::new("backspace", DeleteSelection, Some(CONTEXT)),
        KeyBinding::new("delete", DeleteSelection, Some(CONTEXT)),
        KeyBinding::new("escape", CancelSelection, Some(CONTEXT)),
        KeyBinding::new("up", SelectPrevBlock, Some(CONTEXT)),
        KeyBinding::new("down", SelectNextBlock, Some(CONTEXT)),
        KeyBinding::new("enter", FocusSelectedBlock, Some(CONTEXT)),
        KeyBinding::new("secondary-z", UndoBlocks, Some(CONTEXT)),
        KeyBinding::new("secondary-shift-z", RedoBlocks, Some(CONTEXT)),
        KeyBinding::new("ctrl-y", RedoBlocks, Some(CONTEXT)),
        KeyBinding::new("secondary-k", InsertLink, Some(CONTEXT)),
        // Also bound inside a focused input: inserting a link is something you
        // do while typing, not while a whole block is selected. The action is
        // unknown to `BlockInput`, so it bubbles up to the editor container
        // without a capture.
        KeyBinding::new(
            "secondary-k",
            InsertLink,
            Some(super::components::block_input::INPUT_CONTEXT),
        ),
    ]);
}

/// Global counter that gives each editor a unique scope in the image registry
/// (stable paths for the window lifetime, without collisions
/// between two composers).
static EDITOR_SEQ: AtomicU64 = AtomicU64::new(1);

/// Style for a text block (a single `InputState`, restyled in place when its
/// type change).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextStyle {
    Paragraph,
    Heading(u8),
    Quote,
    Code,
}

struct TextBlock {
    style: TextStyle,
    /// Code-block language preserved for round trips, but not edited.
    language: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

struct ListRow {
    indent: u8,
    input: Entity<InputState>,
    _sub: Subscription,
}

struct ListBlock {
    ordered: bool,
    rows: Vec<ListRow>,
}

struct TableCell {
    input: Entity<InputState>,
    _sub: Subscription,
}

struct TableBlock {
    rows: Vec<Vec<TableCell>>,
}

enum EbKind {
    Text(TextBlock),
    List(ListBlock),
    Table(TableBlock),
    Image {
        cid: String,
        width: Option<u32>,
        /// `aviary-cid/...` path when bytes are known; otherwise a degraded
        /// rendering using the CID name.
        path: Option<String>,
        /// Horizontal scrolling belongs to the image block so an oversized
        /// image does not move the rest of the document.
        scroll: ScrollHandle,
    },
    Divider,
    /// Quoted original message (read-only, with full HTML fidelity).
    Original {
        kind: BlockKind,
    },
}

#[derive(Clone, Copy)]
enum InlineFormat {
    Bold,
    Italic,
    Underline,
}

#[derive(Clone, Copy)]
enum InlineVisualStyle {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    /// Carries a theme colour: the palette is user-configurable, so these two
    /// cannot be constants like the others.
    Link(gpui::Hsla),
    /// A link's destination. Dimmed with a real colour rather than `fade_out`:
    /// fading blends toward the background, which on a dark canvas reads as
    /// nearly invisible and on a light one as washed out. It is also only on
    /// screen while the link is unfolded — that is, while the user is reading or
    /// editing it — so it has to stay legible.
    LinkDestination(gpui::Hsla),
    Syntax,
}

/// Markers remain discoverable without competing with formatted content.
const INLINE_SYNTAX_FADE_OUT: f32 = 0.72;

impl InlineVisualStyle {
    fn highlight(self) -> HighlightStyle {
        match self {
            Self::Bold => HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            Self::Italic => HighlightStyle {
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            Self::Underline => HighlightStyle {
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Self::Strikethrough => HighlightStyle {
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Self::Link(color) => HighlightStyle {
                color: Some(color),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Self::Syntax => HighlightStyle {
                fade_out: Some(INLINE_SYNTAX_FADE_OUT),
                ..Default::default()
            },
            Self::LinkDestination(color) => HighlightStyle {
                color: Some(color),
                ..Default::default()
            },
        }
    }
}

/// Theme colours the inline styling cannot hardcode.
#[derive(Clone, Copy)]
struct InlineColors {
    /// A link's label.
    link: gpui::Hsla,
    /// A link's destination.
    destination: gpui::Hsla,
}

impl InlineColors {
    fn from_theme(theme: &gpui_component::Theme) -> Self {
        Self {
            link: readable_link_color(theme.link, theme.foreground),
            destination: theme.muted_foreground,
        }
    }
}

/// Bounds that keep a link's hue perceptible whatever the body text does: a blue
/// at the lightness of near-black text reads as black, one at the lightness of
/// near-white text reads as white.
const LINK_MIN_LIGHTNESS: f32 = 0.35;
const LINK_MAX_LIGHTNESS: f32 = 0.82;
/// A near-grey `primary` still has to read as a link.
const LINK_MIN_SATURATION: f32 = 0.55;

/// Aligns a link's lightness with the body text around it.
///
/// `theme.link` follows the palette's `primary`, which is picked for buttons and
/// borders — sat on a background, not set among words. OneDark's `#61afef` is
/// *dimmer* than its own body text (0.66 against 0.71), so a link drawn with it
/// reads as dimmed rather than clickable. Matching the text's lightness leaves
/// the hue and the underline to do the distinguishing, which is what they are
/// for.
fn readable_link_color(link: gpui::Hsla, foreground: gpui::Hsla) -> gpui::Hsla {
    gpui::Hsla {
        l: foreground.l.clamp(LINK_MIN_LIGHTNESS, LINK_MAX_LIGHTNESS),
        s: link.s.max(LINK_MIN_SATURATION),
        ..link
    }
}

/// Translates inline marks retained in Markdown source into visual ranges for
/// the input. Delimiters remain present to preserve editing offsets and are
/// deliberately faded so they do not compete with formatted content.
fn inline_format_highlights(
    value: &str,
    colors: InlineColors,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

    /// A link's visible part is its label (`[label](url)`) or, for an autolink,
    /// the URL between the angle brackets. Everything else is syntax.
    fn push_link(
        ranges: &mut Vec<(std::ops::Range<usize>, HighlightStyle)>,
        value: &str,
        source: std::ops::Range<usize>,
        link_type: LinkType,
        colors: InlineColors,
    ) {
        let inner = match link_type {
            LinkType::Autolink | LinkType::Email => {
                source.start.saturating_add(1)..source.end.saturating_sub(1)
            }
            _ => match links::label_range(value, &source) {
                Some(label) => label,
                // A reference or shortcut link has no destination here; leave it
                // to the plain-text styling rather than guess at its extent.
                None => return,
            },
        };
        if inner.start >= inner.end {
            return;
        }
        ranges.push((
            inner.clone(),
            InlineVisualStyle::Link(colors.link).highlight(),
        ));
        ranges.push((
            source.start..inner.start,
            InlineVisualStyle::Syntax.highlight(),
        ));
        // `](url)` splits in two: the bracket is a marker like any other, the
        // destination behind it is the part that would otherwise dominate.
        let bracket = inner.end..inner.end.saturating_add(1).min(source.end);
        ranges.push((bracket.clone(), InlineVisualStyle::Syntax.highlight()));
        if bracket.end < source.end {
            ranges.push((
                bracket.end..source.end,
                InlineVisualStyle::LinkDestination(colors.destination).highlight(),
            ));
        }
    }

    fn push_inner(
        ranges: &mut Vec<(std::ops::Range<usize>, HighlightStyle)>,
        source: std::ops::Range<usize>,
        delimiter_len: usize,
        style: InlineVisualStyle,
    ) {
        let start = source.start.saturating_add(delimiter_len);
        let end = source.end.saturating_sub(delimiter_len);
        if start < end {
            ranges.push((start..end, style.highlight()));
            ranges.push((source.start..start, InlineVisualStyle::Syntax.highlight()));
            ranges.push((end..source.end, InlineVisualStyle::Syntax.highlight()));
        }
    }

    fn close_html(
        ranges: &mut Vec<(std::ops::Range<usize>, HighlightStyle)>,
        starts: &mut Vec<usize>,
        end: usize,
        style: InlineVisualStyle,
    ) {
        if let Some(start) = starts.pop() {
            if start < end {
                ranges.push((start..end, style.highlight()));
            }
        }
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut ranges = Vec::new();
    let mut underline = Vec::new();

    for (event, source) in Parser::new_ext(value, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Strong) => {
                push_inner(&mut ranges, source, 2, InlineVisualStyle::Bold)
            }
            Event::Start(Tag::Emphasis) => {
                push_inner(&mut ranges, source, 1, InlineVisualStyle::Italic)
            }
            Event::Start(Tag::Strikethrough) => {
                push_inner(&mut ranges, source, 2, InlineVisualStyle::Strikethrough)
            }
            Event::Start(Tag::Link { link_type, .. }) => {
                push_link(&mut ranges, value, source, link_type, colors)
            }
            Event::InlineHtml(tag) => match tag.trim().to_ascii_lowercase().as_str() {
                "<u>" => {
                    underline.push(source.end);
                    ranges.push((source, InlineVisualStyle::Syntax.highlight()));
                }
                "</u>" => {
                    close_html(
                        &mut ranges,
                        &mut underline,
                        source.start,
                        InlineVisualStyle::Underline,
                    );
                    ranges.push((source, InlineVisualStyle::Syntax.highlight()));
                }
                _ => {}
            },
            _ => {}
        }
    }

    ranges
}

impl InlineFormat {
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::Bold => ("**", "**"),
            Self::Italic => ("_", "_"),
            Self::Underline => ("<u>", "</u>"),
        }
    }
}

struct EbBlock {
    id: u64,
    kind: EbKind,
}

/// Targets in the transform menu.
#[derive(Clone, Copy)]
enum StyleTarget {
    Paragraph,
    Heading(u8),
    Quote,
    Code,
    Bullets,
    Numbered,
}

impl StyleTarget {
    /// `TextStyle` equivalent for textual targets.
    fn text_style(self) -> Option<TextStyle> {
        match self {
            StyleTarget::Paragraph => Some(TextStyle::Paragraph),
            StyleTarget::Heading(l) => Some(TextStyle::Heading(l)),
            StyleTarget::Quote => Some(TextStyle::Quote),
            StyleTarget::Code => Some(TextStyle::Code),
            StyleTarget::Bullets | StyleTarget::Numbered => None,
        }
    }
}

pub struct BlockEditor {
    scope: String,
    blocks: Vec<EbBlock>,
    images: Vec<InlineImage>,
    /// Blitz presentation options for faithful quotes.
    mail_body_options: MailBodyOptions,
    next_id: u64,
    /// Placeholder for the initial paragraph.
    placeholder: gpui::SharedString,
    /// Input and offset designated by `{{cursor}}` in a template. The marker is
    /// removed during import; the target remains the document's first Tab stop
    /// without stealing initial focus from header fields.
    template_cursor: Option<(EntityId, usize)>,
    /// Container focus, acquired while a block selection is active
    /// (`BlockEditor` context shortcuts then apply).
    focus_handle: FocusHandle,
    /// Block-level selection: `(anchor id, head id)`, inclusive bounds,
    /// in either order.
    sel: Option<(u64, u64)>,
    /// Block where a mouse drag began; becomes a selection as soon as the
    /// pointer enters another block while the button is held.
    drag_anchor: Option<u64>,
    /// Armed Ctrl+A: a second Ctrl+A in the same input selects all blocks,
    /// escalating as in Notion.
    select_all_armed: Option<EntityId>,
    /// Active image resize with the handle grabbed.
    resize: Option<ResizeDrag>,
    /// Displayed widths of image blocks, measured during rendering with
    /// `on_children_prepainted`; used as the resize starting point when no
    /// width has been fixed yet.
    measured: std::collections::HashMap<u64, f32>,
    /// Document width measured independently of its blocks. Used as a
    /// fallback for the Blitz original message during its first layout,
    /// in both the main pane and a composer window.
    layout_width: Option<f32>,
    /// Visual zoom for the editable document, independent of serialized content.
    zoom: f32,
    /// `OriginalMessage` blocks collapse together. Enabled when opening an
    /// inline reply; a regular composer displays them in full.
    original_messages_collapsed: bool,
    /// Undo history: whole-document snapshots pushed before each structural
    /// operation and each typing burst.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Copy of document state at the latest render, the before state to
    /// push when an `InputEvent::Change` arrives (the input text
    /// has already changed by then).
    mirror: Snapshot,
    /// Latest keystroke (input, instant): nearby keystrokes in the same input
    /// are coalesced into one undo step.
    last_edit: Option<(EntityId, std::time::Instant)>,
    /// Changes emitted by our own `set_value` calls: they must neither create
    /// an undo step nor coalesce the next real keystroke.
    ignored_input_changes: std::collections::HashSet<EntityId>,
    /// Last background spelling result per input. Keeping the source text
    /// alongside the ranges prevents stale results from underlining new text.
    spelling: std::collections::HashMap<EntityId, SpellingResult>,
    /// Replacing a task cancels the previous debounce/check for that input.
    spelling_tasks: std::collections::HashMap<EntityId, gpui::Task<()>>,
    /// Optional bridge to the background runtime, used for grammar checks and
    /// pasted-image downloads. Absent in the settings editors, which are not
    /// attached to an account.
    runtime_tx: Option<mpsc::UnboundedSender<Cmd>>,
    /// Original URL of every pasted image still being downloaded, keyed by the
    /// placeholder cid standing in for it. This is what lets a failed download —
    /// or a send racing one — fall back to the hotlink instead of shipping an
    /// empty inline image. See `remote_images`.
    pending_remote_images: std::collections::HashMap<String, String>,
    /// Settings are copied so detached composers keep working; the app
    /// refreshes every open editor when preferences change.
    languagetool_settings: LanguageToolSettings,
    languagetool_results: std::collections::HashMap<EntityId, LanguageToolResult>,
    languagetool_tasks: std::collections::HashMap<EntityId, gpui::Task<()>>,
    languagetool_revisions: std::collections::HashMap<EntityId, u64>,
    /// Source text for which the latest LanguageTool request failed. This
    /// keeps Hunspell active without immediately rescheduling on every render.
    languagetool_failures: std::collections::HashMap<EntityId, String>,
    /// Contact completion is enabled only for actual mail composers. Settings
    /// editors reuse BlockEditor without a recipient field.
    mention_completion: Option<super::components::block_input::BlockCompletionProvider>,
    mention_address_book: Option<AddressBook>,
    /// Signatures this draft can switch to, pushed by the composer for the
    /// sending account (and refreshed when that account changes). Empty in the
    /// settings editors, which edit signatures rather than use them.
    available_signatures: Vec<SignatureChoice>,
}

/// A signature offered by the block's picker, rendered ahead of time so
/// switching is a straight replacement.
#[derive(Clone)]
pub(crate) struct SignatureChoice {
    pub id: i64,
    pub name: String,
    pub html: String,
    pub images: Vec<InlineImage>,
}

fn strip_template_cursor(value: &str) -> Option<(String, usize)> {
    let marker = blocks::TEMPLATE_CURSOR_PLACEHOLDER;
    let offset = value.find(marker)?;
    Some((value.replace(marker, ""), offset))
}

/// Active drag on an image resize handle.
#[derive(Clone, Copy)]
struct ResizeDrag {
    bid: u64,
    start_x: Pixels,
    start_w: f32,
}

impl BlockEditor {
    pub fn set_placeholder(
        &mut self,
        placeholder: gpui::SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.clone();
        for block in &self.blocks {
            match &block.kind {
                EbKind::Text(text) => text.input.update(cx, |state, cx| {
                    state.set_placeholder(placeholder.clone(), window, cx);
                }),
                EbKind::List(list) => {
                    for row in &list.rows {
                        row.input.update(cx, |state, cx| {
                            state.set_placeholder(placeholder.clone(), window, cx);
                        });
                    }
                }
                EbKind::Table(_)
                | EbKind::Image { .. }
                | EbKind::Divider
                | EbKind::Original { .. } => {}
            }
        }
        cx.notify();
    }

    pub fn from_markdown(
        md: &str,
        images: Vec<InlineImage>,
        lead_blank: bool,
        mail_body_options: MailBodyOptions,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kinds = blocks::markdown_to_blocks(md);
        Self::new(
            kinds,
            images,
            lead_blank,
            mail_body_options,
            placeholder,
            window,
            cx,
        )
    }

    pub fn new(
        kinds: Vec<BlockKind>,
        images: Vec<InlineImage>,
        lead_blank: bool,
        mail_body_options: MailBodyOptions,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            kinds,
            images,
            mail_body_options,
            placeholder,
            EditorInitialization::Document { lead_blank },
            window,
            cx,
        )
    }

    /// Builds an editor for stored template definitions. Unlike a composer,
    /// this keeps `{{cursor}}` visible and serializable so editing an existing
    /// template cannot silently remove its insertion target.
    pub(crate) fn new_template_editor(
        kinds: Vec<BlockKind>,
        images: Vec<InlineImage>,
        mail_body_options: MailBodyOptions,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            kinds,
            images,
            mail_body_options,
            placeholder,
            EditorInitialization::TemplateDefinition,
            window,
            cx,
        )
    }

    fn new_inner(
        kinds: Vec<BlockKind>,
        images: Vec<InlineImage>,
        mail_body_options: MailBodyOptions,
        placeholder: &str,
        initialization: EditorInitialization,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (lead_blank, consume_template_cursor) = match initialization {
            EditorInitialization::Document { lead_blank } => (lead_blank, true),
            EditorInitialization::TemplateDefinition => (false, false),
        };
        let mut this = Self {
            scope: format!(
                "block-editor-{}",
                EDITOR_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            blocks: Vec::new(),
            images,
            mail_body_options,
            next_id: 1,
            placeholder: placeholder.to_string().into(),
            template_cursor: None,
            focus_handle: cx.focus_handle(),
            sel: None,
            drag_anchor: None,
            select_all_armed: None,
            resize: None,
            measured: std::collections::HashMap::new(),
            layout_width: None,
            zoom: 1.0,
            original_messages_collapsed: false,
            undo: Vec::new(),
            redo: Vec::new(),
            mirror: Snapshot::default(),
            last_edit: None,
            ignored_input_changes: std::collections::HashSet::new(),
            spelling: std::collections::HashMap::new(),
            spelling_tasks: std::collections::HashMap::new(),
            runtime_tx: None,
            pending_remote_images: std::collections::HashMap::new(),
            languagetool_settings: LanguageToolSettings::default(),
            languagetool_results: std::collections::HashMap::new(),
            languagetool_tasks: std::collections::HashMap::new(),
            languagetool_revisions: std::collections::HashMap::new(),
            languagetool_failures: std::collections::HashMap::new(),
            mention_completion: None,
            mention_address_book: None,
            available_signatures: Vec::new(),
        };
        let mut ebs: Vec<EbBlock> = Vec::new();
        if lead_blank && !kinds.is_empty() {
            let placeholder = this.placeholder.clone();
            ebs.push(this.make_text(
                TextStyle::Paragraph,
                String::new(),
                &placeholder,
                window,
                cx,
            ));
        }
        for kind in kinds {
            let b = this.import_kind(kind, window, cx);
            ebs.push(b);
        }
        if ebs.is_empty() {
            let ph = this.placeholder.clone();
            ebs.push(this.make_text(TextStyle::Paragraph, String::new(), &ph, window, cx));
        }
        this.blocks = ebs;
        if consume_template_cursor {
            this.consume_template_cursor(window, cx);
        }
        this.mirror = this.exact_snapshot(cx);
        this
    }

    /// Supplies the container's known width to stabilize the entire
    /// initial layout. The render probe then takes over during
    /// actual resizes.
    pub fn with_layout_width_hint(mut self, width: Option<f32>) -> Self {
        self.layout_width = width.filter(|width| *width >= 40.0);
        self
    }

    pub(crate) fn with_contact_mentions(
        mut self,
        address_book: AddressBook,
        recipient: Entity<RecipientInput>,
        cx: &mut Context<Self>,
    ) -> Self {
        self.mention_completion = Some(mentions::completion_provider(
            address_book.clone(),
            recipient,
        ));
        self.mention_address_book = Some(address_book);
        let provider = self.completion_provider();
        for input in self.all_text_inputs() {
            let provider = provider.clone();
            input.update(cx, |state, _| state.set_completion_provider(provider));
            self.apply_input_highlights(&input, cx);
        }
        self
    }

    fn adjust_zoom(&mut self, event: &ScrollWheelEvent, window: &Window) -> bool {
        if !event.modifiers.control && !event.modifiers.platform {
            return false;
        }
        let delta = match event.delta {
            ScrollDelta::Lines(delta) => {
                let axis = if delta.y == 0.0 { delta.x } else { delta.y };
                axis.signum() * ZOOM_LINE_STEP
            }
            ScrollDelta::Pixels(delta) => {
                let axis = if delta.y == px(0.) { delta.x } else { delta.y };
                let line_height = f32::from(window.line_height()).max(1.0);
                (f32::from(axis) / line_height * ZOOM_LINE_STEP).clamp(-0.2, 0.2)
            }
        };
        let zoom = ((self.zoom + delta).clamp(ZOOM_MIN, ZOOM_MAX) * 20.0).round() / 20.0;
        let changed = (zoom - self.zoom).abs() >= f32::EPSILON;
        self.zoom = zoom;
        changed
    }

    /// Collapses faithful quotes on first display. Their content remains in the
    /// document and is preserved on send or detach.
    pub fn collapse_original_messages(mut self) -> Self {
        self.original_messages_collapsed = true;
        self
    }

    pub fn images(&self) -> &[InlineImage] {
        &self.images
    }

    /// Focuses the first document input with the cursor at the beginning (for
    /// example, when opening an inline reply, where the first block is the
    /// empty paragraph above the quote).
    pub fn focus_first(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, _, input)) = self.nav_inputs().into_iter().next() {
            Self::focus_at(&input, 0, window, cx);
            input.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// Removes the template-only marker, remembers its position, and sets the
    /// input selection without focusing it.
    fn consume_template_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let target = self.all_inputs().into_iter().find_map(|input| {
            let value = input.read(cx).value().to_string();
            strip_template_cursor(&value).map(|(clean, offset)| (input, clean, offset))
        });
        let Some((input, clean, offset)) = target else {
            return false;
        };

        self.ignored_input_changes.insert(input.entity_id());
        input.update(cx, |state, cx| {
            state.set_value(clean, window, cx);
        });
        Self::focus_at(&input, offset, window, cx);
        self.template_cursor = Some((input.entity_id(), offset));
        // Removing the marker is part of insertion, not a separate user
        // keystroke in undo history.
        self.mirror = self.exact_snapshot(cx);
        self.last_edit = None;
        cx.notify();
        true
    }

    /// Focuses the remembered template-marker position. Returns `true` when a
    /// target still present in the document was found.
    pub fn focus_template_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.template_cursor.is_none() && !self.consume_template_cursor(window, cx) {
            return false;
        }
        let Some((target_id, offset)) = self.template_cursor else {
            return false;
        };
        let Some(input) = self
            .all_inputs()
            .into_iter()
            .find(|input| input.entity_id() == target_id)
        else {
            self.template_cursor = None;
            return false;
        };
        let offset = offset.min(input.read(cx).text().len());
        input.update(cx, |state, cx| state.focus(window, cx));
        // Apply selection after focus; this is particularly necessary when
        // entry comes from gpui Tab navigation.
        Self::focus_at(&input, offset, window, cx);
        true
    }

    /// Converts an editor block to the persistent model.
    ///
    /// Empty paragraphs are significant document structure: two Enter presses
    /// between text blocks deliberately create a visible blank line. Keep them
    /// here and let the block serializer give them an explicit HTML form.
    fn export_block(&self, b: &EbBlock, cx: &App) -> Option<Block> {
        let kind = match &b.kind {
            EbKind::Text(tb) => {
                let text = tb.input.read(cx).value().to_string();
                match tb.style {
                    TextStyle::Paragraph => BlockKind::Paragraph(text),
                    TextStyle::Heading(level) => BlockKind::Heading { level, text },
                    TextStyle::Quote => BlockKind::Quote(text),
                    TextStyle::Code => BlockKind::Code {
                        language: tb.language.clone(),
                        text,
                    },
                }
            }
            EbKind::List(lb) => BlockKind::List {
                ordered: lb.ordered,
                items: lb
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| ListItem {
                        id: i as u64 + 1,
                        indent: r.indent,
                        text: r.input.read(cx).value().to_string(),
                    })
                    .collect(),
            },
            EbKind::Table(table) => BlockKind::Table {
                rows: table
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|cell| cell.input.read(cx).value().to_string())
                            .collect()
                    })
                    .collect(),
            },
            EbKind::Image { cid, width, .. } => BlockKind::Image {
                cid: cid.clone(),
                width: *width,
            },
            EbKind::Divider => BlockKind::Divider,
            EbKind::Original { kind, .. } => kind.clone(),
        };
        Some(Block { id: b.id, kind })
    }

    /// Exports the document to the persistent model or send pipeline.
    pub fn to_blocks(&self, cx: &App) -> Vec<Block> {
        self.blocks
            .iter()
            .filter_map(|b| self.export_block(b, cx))
            .collect()
    }

    /// Exports only block kinds to faithfully move an inline composer into a
    /// window without passing through Markdown. Empty paragraphs remain because
    /// they are useful editing areas before a quote and after an image.
    pub fn to_kinds(&self, cx: &App) -> Vec<BlockKind> {
        self.exact_snapshot(cx).kinds
    }

    // ------------------------------------------------------------------
    // Undo/redo history
    // ------------------------------------------------------------------

    pub fn to_markdown(&self, cx: &App) -> String {
        blocks::blocks_to_markdown(&self.to_blocks(cx))
    }

    /// Portion of the document given to the AI assistant. A faithful quote
    /// (`OriginalMessage`) and everything after it stay outside the prompt so
    /// so the transformation can neither alter nor flatten the received email.
    pub fn ai_markdown(&self, cx: &App) -> String {
        let blocks = self
            .to_blocks(cx)
            .into_iter()
            .take_while(|block| !matches!(block.kind, BlockKind::OriginalMessage { .. }))
            .collect::<Vec<_>>();
        blocks::blocks_to_markdown(&blocks)
    }

    /// Replaces the editable portion with the AI's Markdown response as one
    /// undoable step while preserving internal quotes and images.
    pub fn apply_ai_markdown(
        &mut self,
        markdown: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.exact_snapshot(cx);
        let suffix_at = current
            .kinds
            .iter()
            .position(|kind| matches!(kind, BlockKind::OriginalMessage { .. }))
            .unwrap_or(current.kinds.len());
        let mut kinds = blocks::markdown_to_blocks(markdown);
        kinds.extend(current.kinds[suffix_at..].iter().cloned());
        self.push_undo(cx);
        self.apply_snapshot(
            Snapshot {
                kinds,
                images: current.images,
            },
            window,
            cx,
        );
    }

    /// HTML sortant (pipeline historique `build_html_body`).
    pub fn build_html(&self, cx: &App) -> String {
        blocks::build_html_body(&self.to_blocks(cx))
    }

    /// Builds HTML and retains only images it actually references. This avoids
    /// sending CID aliases added by Graph and images orphaned during editing.
    ///
    /// Pasted images still downloading are put back to their original URL: a
    /// send is not going to wait for them, and a zero-byte inline image renders
    /// as nothing at all, where the hotlink at least renders.
    pub fn build_outgoing(&self, cx: &App) -> (String, Vec<InlineImage>) {
        let html = self.build_html(cx);
        let mut images = blocks::referenced_inline_images(&html, &self.images);
        let html = remote_images::restore_pending(&html, &mut images, &self.pending_remote_images);
        (html, images)
    }

    /// Inserts template blocks after the focused block, or at the end, and
    /// imports their inline images.
    pub fn insert_kinds(
        &mut self,
        kinds: Vec<BlockKind>,
        images: Vec<InlineImage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_undo(cx);
        for image in images {
            if !self.images.iter().any(|i| i.cid == image.cid) {
                self.images.push(image);
            }
        }
        let at = self
            .focused_ix(window, cx)
            .map(|ix| ix + 1)
            .unwrap_or(self.blocks.len());
        let mut ebs = Vec::new();
        for kind in kinds {
            let b = self.import_kind(kind, window, cx);
            ebs.push(b);
        }
        for (i, b) in ebs.into_iter().enumerate() {
            self.blocks.insert(at + i, b);
        }
        cx.notify();
    }

    /// Inserts a template and honors its optional `{{cursor}}` marker.
    pub fn insert_template(
        &mut self,
        kinds: Vec<BlockKind>,
        images: Vec<InlineImage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_kinds(kinds, images, window, cx);
        // An explicit insertion may carry a new target, distinct
        // from that of the template loaded at startup.
        self.template_cursor = None;
        self.focus_template_cursor(window, cx);
    }

    /// Visibly adds the cursor marker to the active input in the
    /// template editor (or in a new paragraph as a fallback).
    pub fn insert_template_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let marker = blocks::TEMPLATE_CURSOR_PLACEHOLDER;
        let target = self
            .nav_inputs()
            .into_iter()
            .find(|(_, _, input)| input.focus_handle(cx).is_focused(window))
            .map(|(_, _, input)| input);
        if let Some(input) = target {
            input.update(cx, |state, cx| {
                state.insert(marker, window, cx);
                state.focus(window, cx);
            });
        } else {
            self.insert_kinds(
                vec![BlockKind::Paragraph(marker.to_string())],
                Vec::new(),
                window,
                cx,
            );
            if let Some((_, _, input)) = self.nav_inputs().into_iter().last() {
                let end = input.read(cx).text().len();
                Self::focus_at(&input, end, window, cx);
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    /// Signatures the block's picker offers. Pushed by the composer, so an
    /// editor that has no account (Preferences) simply shows none.
    pub(crate) fn set_available_signatures(
        &mut self,
        signatures: Vec<SignatureChoice>,
        cx: &mut Context<Self>,
    ) {
        if self.available_signatures.len() == signatures.len()
            && self
                .available_signatures
                .iter()
                .zip(&signatures)
                .all(|(current, new)| current.id == new.id && current.name == new.name)
        {
            return;
        }
        self.available_signatures = signatures;
        cx.notify();
    }

    /// Index of the draft's signature block, if it still has one.
    fn signature_ix(&self) -> Option<usize> {
        self.blocks.iter().position(|block| {
            matches!(
                &block.kind,
                EbKind::Original {
                    kind: BlockKind::Signature { .. }
                }
            )
        })
    }

    /// Switches the draft to `choice`: the existing signature block is
    /// replaced in place — a signature belongs where it was put — and one is
    /// inserted at the cursor when the draft has none.
    ///
    /// Images of the signature left behind are not pruned here: `build_outgoing`
    /// already ships only what the body references, and keeping them costs a
    /// few kilobytes of draft against the risk of dropping an image the user
    /// switches back to.
    pub(crate) fn apply_signature(
        &mut self,
        choice: &SignatureChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = BlockKind::Signature {
            signature_id: Some(choice.id),
            name: choice.name.clone(),
            html: choice.html.clone(),
        };
        let Some(ix) = self.signature_ix() else {
            self.insert_kinds(vec![kind], choice.images.clone(), window, cx);
            return;
        };
        self.push_undo(cx);
        for image in choice.images.iter().cloned() {
            if !self.images.iter().any(|current| current.cid == image.cid) {
                self.images.push(image);
            }
        }
        self.blocks[ix] = self.import_kind(kind, window, cx);
        cx.notify();
    }

    /// Drops the signature block. The rest of the draft is untouched.
    pub(crate) fn clear_signature(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.signature_ix() else {
            return;
        };
        self.push_undo(cx);
        self.blocks.remove(ix);
        self.ensure_not_empty(window, cx);
        cx.notify();
    }

    /// Inserts an opaque, faithful HTML fragment. Blitz renders it in the editor,
    /// and it is injected unchanged into outgoing HTML.
    pub fn insert_html(&mut self, html: String, window: &mut Window, cx: &mut Context<Self>) {
        if html.trim().is_empty() {
            return;
        }
        self.insert_kinds(vec![BlockKind::RawHtml { html }], Vec::new(), window, cx);
    }

    fn focused_input(&self, window: &Window, cx: &App) -> Option<Entity<InputState>> {
        self.all_inputs()
            .into_iter()
            .find(|input| input.focus_handle(cx).is_focused(window))
    }

    /// Applies an inline mark to selected text. A second click removes the mark
    /// when it already surrounds the selection; without a selection, both
    /// markers are inserted and the cursor stays between them.
    fn apply_inline_format(
        &mut self,
        format: InlineFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.focused_input(window, cx) else {
            return;
        };
        let (value, range) = {
            let state = input.read(cx);
            (state.value().to_string(), state.selection_range())
        };
        let (open, close) = format.markers();
        let start = range.start.min(value.len());
        let end = range.end.min(value.len()).max(start);
        if !value.is_char_boundary(start) || !value.is_char_boundary(end) {
            return;
        }

        self.push_undo(cx);
        let (new_value, cursor) = if start == end {
            if start >= open.len()
                && value[..start].ends_with(open)
                && value[start..].starts_with(close)
            {
                let mut next = value;
                next.replace_range(start..start + close.len(), "");
                next.replace_range(start - open.len()..start, "");
                (next, start - open.len())
            } else {
                let mut next = value;
                next.insert_str(start, close);
                next.insert_str(start, open);
                (next, start + open.len())
            }
        } else {
            let selected = &value[start..end];
            if selected.starts_with(open)
                && selected.ends_with(close)
                && selected.len() >= open.len() + close.len()
            {
                let inner = &selected[open.len()..selected.len() - close.len()];
                let mut next = value.clone();
                next.replace_range(start..end, inner);
                (next, start + inner.len())
            } else if start >= open.len()
                && value[..start].ends_with(open)
                && value[end..].starts_with(close)
            {
                let mut next = value.clone();
                next.replace_range(end..end + close.len(), "");
                next.replace_range(start - open.len()..start, "");
                (next, end - open.len())
            } else {
                let mut next = value;
                next.insert_str(end, close);
                next.insert_str(start, open);
                (next, end + open.len() + close.len())
            }
        };
        input.update(cx, |state, cx| {
            state.set_value(new_value, window, cx);
            state.set_cursor_offset(cursor, window, cx);
        });
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Block construction
    // ------------------------------------------------------------------

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn new_input(
        &self,
        text: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<InputState>, Subscription) {
        let completion = self.completion_provider();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .auto_grow(1, 512)
                .placeholder(placeholder.to_string())
                .default_value(text.to_string());
            input.set_completion_provider(completion);
            input.set_mouse_context_menu_builder(std::rc::Rc::new(
                |input_id, value, offset, _window, _cx| {
                    if let Some(issue) = language_issue_at(value, offset) {
                        let original = value
                            .get(issue.range.clone())
                            .unwrap_or_default()
                            .to_string();
                        let mut items = vec![PopupMenuItem::label(issue.message.clone())];
                        if issue.replacements.is_empty() {
                            items.push(PopupMenuItem::label(tr!("spellcheck-no-suggestions")));
                        } else {
                            items.extend(issue.replacements.into_iter().map(|replacement| {
                                PopupMenuItem::new(replacement.clone()).action(Box::new(
                                    ApplySpellingSuggestion {
                                        input_id,
                                        range: issue.range.clone(),
                                        original: original.clone(),
                                        replacement,
                                    },
                                ))
                            }));
                        }
                        items.push(PopupMenuItem::separator());
                        items.push(PopupMenuItem::new(tr!("proofreading-ignore-rule")).action(
                            Box::new(IgnoreProofreadingRule {
                                rule_id: issue.rule_id,
                            }),
                        ));
                        if issue.category == ProofreadingCategory::Spelling && !original.is_empty()
                        {
                            items.push(
                                PopupMenuItem::new(tr!("spellcheck-add-to-dictionary"))
                                    .action(Box::new(AddSpellingToDictionary { word: original })),
                            );
                        }
                        return items;
                    }
                    if hunspell_is_suppressed(value) {
                        return Vec::new();
                    }
                    let Some(issue) = spellcheck::issue_at(value, offset) else {
                        return Vec::new();
                    };
                    let mut items = vec![PopupMenuItem::label(tr!(
                        "spellcheck-menu-title",
                        { word: &issue.word }
                    ))];
                    let suggestions = spellcheck::suggestions(&issue.word);
                    if suggestions.is_empty() {
                        items.push(PopupMenuItem::label(tr!("spellcheck-no-suggestions")));
                    } else {
                        items.extend(suggestions.into_iter().map(|replacement| {
                            PopupMenuItem::new(replacement.clone()).action(Box::new(
                                ApplySpellingSuggestion {
                                    input_id,
                                    range: issue.range.clone(),
                                    original: issue.word.clone(),
                                    replacement,
                                },
                            ))
                        }));
                    }
                    items.push(PopupMenuItem::separator());
                    items.push(
                        PopupMenuItem::new(tr!("spellcheck-ignore")).action(Box::new(
                            IgnoreSpelling {
                                word: issue.word.clone(),
                            },
                        )),
                    );
                    items.push(
                        PopupMenuItem::new(tr!("spellcheck-add-to-dictionary"))
                            .action(Box::new(AddSpellingToDictionary { word: issue.word })),
                    );
                    items
                },
            ));
            input
        });
        let highlights = inline_format_highlights(text, InlineColors::from_theme(cx.theme()));
        let folds = links::foldable_ranges(text);
        if !highlights.is_empty() || !folds.is_empty() {
            input.update(cx, |state, cx| {
                state.set_text_highlights(highlights, cx);
                state.set_foldable_ranges(folds, cx);
            });
        }
        // Bubble every keystroke to the document entity. This keeps parent
        // composer observers (notably session autosave) informed even when a
        // plain paragraph edit does not structurally change the block list.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        let sub = cx.subscribe_in(&input, window, Self::on_input_event);
        (input, sub)
    }

    fn completion_provider(&self) -> super::components::block_input::BlockCompletionProvider {
        let emoji = emoji::completion_provider();
        let mentions = self.mention_completion.clone();
        std::rc::Rc::new(move |source, offset| {
            if let Some(mentions) = &mentions {
                let matches = mentions(source, offset);
                if !matches.is_empty() {
                    return matches;
                }
            }
            emoji(source, offset)
        })
    }

    fn all_text_inputs(&self) -> Vec<Entity<InputState>> {
        let mut inputs = Vec::new();
        for block in &self.blocks {
            match &block.kind {
                EbKind::Text(text) => inputs.push(text.input.clone()),
                EbKind::List(list) => {
                    inputs.extend(list.rows.iter().map(|row| row.input.clone()));
                }
                EbKind::Table(table) => {
                    inputs.extend(
                        table
                            .rows
                            .iter()
                            .flat_map(|row| row.iter().map(|cell| cell.input.clone())),
                    );
                }
                EbKind::Image { .. } | EbKind::Divider | EbKind::Original { .. } => {}
            }
        }
        inputs
    }

    fn make_text(
        &mut self,
        style: TextStyle,
        text: String,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EbBlock {
        let (input, _sub) = self.new_input(&text, placeholder, window, cx);
        EbBlock {
            id: self.alloc_id(),
            kind: EbKind::Text(TextBlock {
                style,
                language: String::new(),
                input,
                _sub,
            }),
        }
    }

    fn make_row(
        &mut self,
        indent: u8,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ListRow {
        let (input, _sub) = self.new_input(text, "", window, cx);
        ListRow {
            indent,
            input,
            _sub,
        }
    }

    fn import_kind(
        &mut self,
        kind: BlockKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EbBlock {
        match kind {
            BlockKind::Paragraph(text) => {
                // A paragraph containing only an image reference (signature,
                // template, or reopened draft) becomes a WYSIWYG Image block.
                if let Some(cid) = standalone_image_cid(&text) {
                    return self.make_image(cid, None);
                }
                self.make_text(TextStyle::Paragraph, text, "", window, cx)
            }
            BlockKind::Heading { level, text } => {
                self.make_text(TextStyle::Heading(level.clamp(1, 3)), text, "", window, cx)
            }
            BlockKind::Quote(text) => self.make_text(TextStyle::Quote, text, "", window, cx),
            BlockKind::Code { language, text } => {
                let mut b = self.make_text(TextStyle::Code, text, "", window, cx);
                if let EbKind::Text(tb) = &mut b.kind {
                    tb.language = language;
                }
                b
            }
            BlockKind::List { ordered, items } => {
                let rows = items
                    .into_iter()
                    .map(|it| self.make_row(it.indent, &it.text, window, cx))
                    .collect();
                EbBlock {
                    id: self.alloc_id(),
                    kind: EbKind::List(ListBlock { ordered, rows }),
                }
            }
            BlockKind::Table { rows } => self.make_table(rows, window, cx),
            BlockKind::Image { cid, width } => self.make_image(cid, width),
            BlockKind::Divider => EbBlock {
                id: self.alloc_id(),
                kind: EbKind::Divider,
            },
            BlockKind::RawHtml { .. }
            | BlockKind::Signature { .. }
            | BlockKind::OriginalMessage { .. } => EbBlock {
                id: self.alloc_id(),
                kind: EbKind::Original { kind },
            },
        }
    }

    fn make_image(&mut self, cid: String, width: Option<u32>) -> EbBlock {
        // A pasted remote image is registered before its bytes arrive; treating
        // the placeholder as present would hand gpui an empty image to decode
        // on every frame. `path: None` renders the loading state instead.
        let image = self
            .images
            .iter()
            .find(|image| image.cid == cid && !image.bytes.is_empty());
        // Bitmap clipboard entries do not carry an editor width. Preserve the
        // intrinsic aspect ratio while applying the same default height cap as
        // the renderer, so wide pasted images have a concrete scroll extent.
        let width = width.or_else(|| image.and_then(|image| initial_image_width(&image.bytes)));
        let path =
            image.map(|image| inline_images::register_bytes(&self.scope, &cid, &image.bytes));
        EbBlock {
            id: self.alloc_id(),
            kind: EbKind::Image {
                cid,
                width,
                path,
                scroll: ScrollHandle::new(),
            },
        }
    }

    // ------------------------------------------------------------------
    // Localisation / focus
    // ------------------------------------------------------------------

    fn block_ix(&self, bid: u64) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == bid)
    }

    fn focused_ix(&self, window: &Window, cx: &App) -> Option<usize> {
        self.blocks.iter().position(|b| match &b.kind {
            EbKind::Text(tb) => tb.input.focus_handle(cx).is_focused(window),
            EbKind::List(lb) => lb
                .rows
                .iter()
                .any(|r| r.input.focus_handle(cx).is_focused(window)),
            EbKind::Table(table) => table
                .rows
                .iter()
                .flatten()
                .any(|cell| cell.input.focus_handle(cx).is_focused(window)),
            _ => false,
        })
    }

    /// Focuses `input` with the cursor at the given byte offset.
    fn focus_at(
        input: &Entity<InputState>,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |s, cx| {
            s.set_cursor_offset(offset, window, cx);
        });
    }

    /// All focusable inputs in document order:
    /// `(block index, item index, entity)`.
    fn nav_inputs(&self) -> Vec<(usize, Option<usize>, Entity<InputState>)> {
        let mut out = Vec::new();
        for (ix, b) in self.blocks.iter().enumerate() {
            match &b.kind {
                EbKind::Text(tb) => out.push((ix, None, tb.input.clone())),
                EbKind::List(lb) => {
                    for (rx, r) in lb.rows.iter().enumerate() {
                        out.push((ix, Some(rx), r.input.clone()));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// All inputs, including table cells. Cells
    /// are not part of `nav_inputs`, because Enter inserts a new line there
    /// text instead of splitting a block.
    fn all_inputs(&self) -> Vec<Entity<InputState>> {
        let mut out = self
            .nav_inputs()
            .into_iter()
            .map(|(_, _, input)| input)
            .collect::<Vec<_>>();
        for block in &self.blocks {
            if let EbKind::Table(table) = &block.kind {
                out.extend(table.rows.iter().flatten().map(|cell| cell.input.clone()));
            }
        }
        out
    }

    /// The block carrying a template cursor precedes other inputs within the
    /// body's Tab group. Without a template, document order remains unchanged.
    fn input_tab_index(&self, input: &Entity<InputState>) -> isize {
        if self
            .template_cursor
            .is_some_and(|(target, _)| target == input.entity_id())
        {
            -1
        } else {
            0
        }
    }

    /// Leaves the document keyboard group, skipping its other inputs and
    /// internal buttons. A message body therefore forms a single stop
    /// in the form, even when it contains multiple blocks.
    fn focus_outside_editor(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let max_steps = self
            .all_inputs()
            .len()
            .saturating_add(self.blocks.len().saturating_mul(6))
            .saturating_add(8);
        for _ in 0..max_steps {
            if forward {
                window.focus_next();
            } else {
                window.focus_prev();
            }
            if !self.focus_handle.contains_focused(window, cx) {
                break;
            }
        }
    }

    /// Ensures that at least one editable paragraph remains.
    fn ensure_not_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blocks.is_empty() {
            let ph = self.placeholder.clone();
            let b = self.make_text(TextStyle::Paragraph, String::new(), &ph, window, cx);
            if let EbKind::Text(tb) = &b.kind {
                tb.input.update(cx, |s, cx| s.focus(window, cx));
            }
            self.blocks.push(b);
        }
    }

    /// Removes `cid` from attachments if no longer referenced by any
    /// block (`![](cid:...)` text, Image block, or original message).
    fn prune_image(&mut self, cid: &str, cx: &App) {
        let md = self.to_markdown(cx);
        let used = md.contains(&format!("cid:{cid}"))
            || md.contains(&format!("bytes://cid-{cid}"))
            || self.blocks.iter().any(|b| match &b.kind {
                EbKind::Original {
                    kind:
                        BlockKind::RawHtml { html }
                        | BlockKind::Signature { html, .. }
                        | BlockKind::OriginalMessage { html, .. },
                } => html.contains(cid),
                _ => false,
            });
        if !used {
            self.images.retain(|i| i.cid != cid);
        }
    }

    // ------------------------------------------------------------------
    // Multi-block selection (drag, Shift-click, Escape, Ctrl+A twice)
    // ------------------------------------------------------------------

    /// Normalized index bounds of the current selection.
    fn sel_range(&self) -> Option<(usize, usize)> {
        let (a, b) = self.sel?;
        let ia = self.block_ix(a)?;
        let ib = self.block_ix(b)?;
        Some((ia.min(ib), ia.max(ib)))
    }

    /// Activates the `anchor..head` selection by ID and focuses the container
    /// to route Ctrl+C/X, Delete, and Escape to the editor. Collapses text
    /// selection in every editor input except
    /// belonging to `keep`, the input that just gained focus.
    fn unselect_others(&self, keep: Option<EntityId>, window: &mut Window, cx: &mut Context<Self>) {
        for input in self.all_inputs() {
            if Some(input.entity_id()) != keep {
                input.update(cx, |s, cx| s.unselect(window, cx));
            }
        }
    }

    fn select_blocks(
        &mut self,
        anchor: u64,
        head: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sel = Some((anchor, head));
        // Block selection replaces any text selection still visible in inputs.
        self.unselect_others(None, window, cx);
        self.focus_handle.focus(window);
        cx.notify();
    }

    /// Shift-click: extends from the existing anchor, otherwise from the focused
    /// block, or selects only the clicked block.
    fn shift_select(&mut self, bid: u64, window: &mut Window, cx: &mut Context<Self>) {
        let anchor = self
            .sel
            .map(|(a, _)| a)
            .or_else(|| self.focused_ix(window, cx).map(|ix| self.blocks[ix].id))
            .unwrap_or(bid);
        self.select_blocks(anchor, bid, window, cx);
    }

    fn on_cancel_selection(
        &mut self,
        _: &CancelSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, head)) = self.sel.take() {
            // Return focus to the head block to continue typing.
            if let Some(input) = self
                .input_for(head, None)
                .or_else(|| self.input_for(head, Some(0)))
            {
                let end = input.read(cx).text().len();
                Self::focus_at(&input, end, window, cx);
            }
            cx.notify();
        }
    }

    fn on_select_all_blocks(
        &mut self,
        _: &SelectAllBlocks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let (Some(first), Some(last)) = (self.blocks.first(), self.blocks.last()) {
            let (a, b) = (first.id, last.id);
            self.select_blocks(a, b, window, cx);
        }
    }

    fn on_delete_selection(
        &mut self,
        _: &DeleteSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_selection(window, cx);
    }

    /// Deletes selected blocks and focuses the nearest neighbor.
    fn remove_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((lo, hi)) = self.sel_range() else {
            return;
        };
        self.push_undo(cx);
        self.sel = None;
        let cids: Vec<String> = self.blocks[lo..=hi]
            .iter()
            .filter_map(|b| match &b.kind {
                EbKind::Image { cid, .. } => Some(cid.clone()),
                _ => None,
            })
            .collect();
        self.blocks.drain(lo..=hi);
        for cid in cids {
            self.prune_image(&cid, cx);
        }
        self.ensure_not_empty(window, cx);
        // Focus the first text input starting at the deleted location
        // otherwise the final one before it.
        let inputs = self.nav_inputs();
        let target = inputs
            .iter()
            .find(|(bix, _, _)| *bix >= lo)
            .or_else(|| inputs.last());
        if let Some((_, _, input)) = target {
            Self::focus_at(input, 0, window, cx);
        }
        cx.notify();
    }

    /// Ctrl+A in an input: the first press lets the input select its text; the
    /// second selects all blocks.
    fn on_select_all_input(
        &mut self,
        bid: u64,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        let input = match (&self.blocks[ix].kind, row) {
            (EbKind::Text(tb), _) => tb.input.clone(),
            (EbKind::List(lb), Some(rx)) if rx < lb.rows.len() => lb.rows[rx].input.clone(),
            _ => return,
        };
        if self.select_all_armed == Some(input.entity_id()) {
            self.select_all_armed = None;
            cx.stop_propagation();
            self.on_select_all_blocks(&SelectAllBlocks, window, cx);
        } else {
            self.select_all_armed = Some(input.entity_id());
            // Let the input perform its own select-all.
        }
    }

    /// Escape in an input selects its block, as in Notion.
    fn on_escape_input(&mut self, bid: u64, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.select_blocks(bid, bid, window, cx);
    }

    // ------------------------------------------------------------------
    // Images: insertion and resizing
    // ------------------------------------------------------------------

    /// Creates a new inline image (attachment and block) from raw bytes. The
    /// CID includes the editor scope to remain unique across composer windows;
    /// copy/paste carries CIDs.
    fn next_image_cid(&mut self) -> String {
        let id = self.alloc_id();
        format!("{}-img-{id}", self.scope)
    }

    fn import_image_bytes(&mut self, mime: String, bytes: Vec<u8>) -> EbBlock {
        let cid = self.next_image_cid();
        self.images.push(InlineImage {
            cid: cid.clone(),
            mime,
            bytes,
        });
        self.make_image(cid, None)
    }

    /// Opens a file picker and inserts selected images after the focused block,
    /// or at the end. Used by the composer's Image button
    /// composer.
    pub fn prompt_insert_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = self
            .focused_ix(window, cx)
            .map(|ix| ix + 1)
            .unwrap_or(self.blocks.len());
        self.prompt_insert_image_at(at, window, cx);
    }

    /// Variant with an explicit position, used by the block insert-image menu.
    fn prompt_insert_image_at(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    let mut at = at.min(this.blocks.len());
                    let mut pushed = false;
                    let mut inserted = false;
                    for path in paths {
                        let Some(mime) = image_mime_for_path(&path) else {
                            continue; // pas une image : ignorer
                        };
                        if !pushed {
                            this.push_undo(cx);
                            pushed = true;
                        }
                        if let Ok(bytes) = std::fs::read(&path) {
                            let b = this.import_image_bytes(mime.to_string(), bytes);
                            this.blocks.insert(at, b);
                            at += 1;
                            inserted = true;
                        }
                    }
                    if inserted {
                        let tail =
                            this.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
                        if let EbKind::Text(tb) = &tail.kind {
                            tb.input.update(cx, |state, cx| state.focus(window, cx));
                        }
                        this.blocks.insert(at, tail);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Resize starting width: fixed width when present, otherwise displayed
    /// width measured at the latest render, otherwise a safe fallback.
    fn resize_start_width(&self, bid: u64) -> f32 {
        self.blocks
            .iter()
            .find(|b| b.id == bid)
            .and_then(|b| match &b.kind {
                EbKind::Image { width, .. } => width.map(|w| w as f32),
                _ => None,
            })
            .or_else(|| self.measured.get(&bid).copied())
            .unwrap_or(320.)
    }

    // ------------------------------------------------------------------
    // Enter: split or exit
    // ------------------------------------------------------------------

    fn on_enter(
        &mut self,
        bid: u64,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };

        // Extract what is needed before mutating self.blocks again.
        enum Plan {
            SplitText {
                input: Entity<InputState>,
                style: TextStyle,
                text: String,
                cur: usize,
            },
            ExitQuote {
                input: Entity<InputState>,
                text: String,
            },
            SplitRow {
                input: Entity<InputState>,
                indent: u8,
                text: String,
                cur: usize,
                rx: usize,
            },
            ExitRow {
                rx: usize,
            },
            Nothing,
        }

        let plan = match (&self.blocks[ix].kind, row) {
            (EbKind::Text(tb), _) => {
                let st = tb.input.read(cx);
                let text = st.text().to_string();
                let cur = st.cursor().min(text.len());
                match tb.style {
                    TextStyle::Paragraph | TextStyle::Heading(_) => Plan::SplitText {
                        input: tb.input.clone(),
                        style: tb.style,
                        text,
                        cur,
                    },
                    TextStyle::Quote | TextStyle::Code => {
                        // Enter on a final empty line exits the block; otherwise
                        // let the input insert the new line.
                        if cur == text.len() && text.ends_with('\n') {
                            Plan::ExitQuote {
                                input: tb.input.clone(),
                                text,
                            }
                        } else {
                            Plan::Nothing
                        }
                    }
                }
            }
            (EbKind::List(lb), Some(rx)) if rx < lb.rows.len() => {
                let r = &lb.rows[rx];
                let st = r.input.read(cx);
                let text = st.text().to_string();
                let cur = st.cursor().min(text.len());
                if text.is_empty() {
                    Plan::ExitRow { rx }
                } else {
                    Plan::SplitRow {
                        input: r.input.clone(),
                        indent: r.indent,
                        text,
                        cur,
                        rx,
                    }
                }
            }
            _ => Plan::Nothing,
        };

        if !matches!(plan, Plan::Nothing) {
            self.push_undo(cx);
        }
        match plan {
            Plan::SplitText {
                input,
                style,
                text,
                cur,
            } => {
                cx.stop_propagation();
                let before = text[..cur].to_string();
                let after = text[cur..].to_string();
                // Splitting a heading at end of line produces a paragraph, as
                // in Notion; in the middle, the remainder stays a heading.
                let new_style = if after.is_empty() {
                    TextStyle::Paragraph
                } else {
                    style
                };
                input.update(cx, |s, cx| s.set_value(before, window, cx));
                let nb = self.make_text(new_style, after, "", window, cx);
                if let EbKind::Text(ntb) = &nb.kind {
                    Self::focus_at(&ntb.input, 0, window, cx);
                }
                self.blocks.insert(ix + 1, nb);
                cx.notify();
            }
            Plan::ExitQuote { input, text } => {
                cx.stop_propagation();
                let trimmed = text.trim_end_matches('\n').to_string();
                input.update(cx, |s, cx| s.set_value(trimmed, window, cx));
                let nb = self.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
                if let EbKind::Text(ntb) = &nb.kind {
                    ntb.input.update(cx, |s, cx| s.focus(window, cx));
                }
                self.blocks.insert(ix + 1, nb);
                cx.notify();
            }
            Plan::SplitRow {
                input,
                indent,
                text,
                cur,
                rx,
            } => {
                cx.stop_propagation();
                let before = text[..cur].to_string();
                let after = text[cur..].to_string();
                input.update(cx, |s, cx| s.set_value(before, window, cx));
                let nr = self.make_row(indent, &after, window, cx);
                Self::focus_at(&nr.input, 0, window, cx);
                if let EbKind::List(lb) = &mut self.blocks[ix].kind {
                    lb.rows.insert(rx + 1, nr);
                }
                cx.notify();
            }
            Plan::ExitRow { rx } => {
                cx.stop_propagation();
                self.exit_list_row(ix, rx, window, cx);
            }
            Plan::Nothing => {}
        }
    }

    /// Removes item `rx` from list `ix`: it becomes a paragraph, and the
    /// list is split if the item was in the middle.
    fn exit_list_row(&mut self, ix: usize, rx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (ordered, row, tail, empty_head) = {
            let EbKind::List(lb) = &mut self.blocks[ix].kind else {
                return;
            };
            if rx >= lb.rows.len() {
                return;
            }
            let ordered = lb.ordered;
            let row = lb.rows.remove(rx);
            let tail: Vec<ListRow> = lb.rows.drain(rx..).collect();
            (ordered, row, tail, lb.rows.is_empty())
        };
        let text = row.input.read(cx).value().to_string();

        let para = self.make_text(TextStyle::Paragraph, text, "", window, cx);
        if let EbKind::Text(tb) = &para.kind {
            Self::focus_at(&tb.input, 0, window, cx);
        }
        let mut insert_at = ix + 1;
        if empty_head {
            self.blocks.remove(ix);
            insert_at = ix;
        }
        self.blocks.insert(insert_at, para);
        if !tail.is_empty() {
            let id = self.alloc_id();
            self.blocks.insert(
                insert_at + 1,
                EbBlock {
                    id,
                    kind: EbKind::List(ListBlock {
                        ordered,
                        rows: tail,
                    }),
                },
            );
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Backspace at offset 0: remove style or merge
    // ------------------------------------------------------------------

    fn on_backspace(
        &mut self,
        bid: u64,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        match (&self.blocks[ix].kind, row) {
            (EbKind::Text(tb), _) => {
                if tb.input.read(cx).cursor() != 0 {
                    return; // effacement normal dans l'input
                }
                let is_paragraph = tb.style == TextStyle::Paragraph;
                if !is_paragraph {
                    // Titre/citation/code → redevient un paragraphe.
                    cx.stop_propagation();
                    self.push_undo(cx);
                    if let EbKind::Text(tb) = &mut self.blocks[ix].kind {
                        tb.style = TextStyle::Paragraph;
                    }
                    cx.notify();
                    return;
                }
                if ix == 0 {
                    return;
                }
                cx.stop_propagation();
                self.push_undo(cx);
                self.merge_into_previous(ix, window, cx);
            }
            (EbKind::List(lb), Some(rx)) if rx < lb.rows.len() => {
                if lb.rows[rx].input.read(cx).cursor() != 0 {
                    return;
                }
                let indented = lb.rows[rx].indent > 0;
                cx.stop_propagation();
                self.push_undo(cx);
                if indented {
                    if let EbKind::List(lb) = &mut self.blocks[ix].kind {
                        lb.rows[rx].indent -= 1;
                    }
                    cx.notify();
                } else {
                    self.exit_list_row(ix, rx, window, cx);
                }
            }
            _ => {}
        }
    }

    /// Merges paragraph `ix` with the preceding block, or removes the preceding
    /// non-text block, as in Notion.
    fn merge_into_previous(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = match &self.blocks[ix].kind {
            EbKind::Text(tb) => tb.input.read(cx).value().to_string(),
            _ => return,
        };

        enum Prev {
            MergeInto(Entity<InputState>),
            Remove,
            RemoveImage(String),
        }
        let prev = match &self.blocks[ix - 1].kind {
            EbKind::Text(p) => Prev::MergeInto(p.input.clone()),
            EbKind::List(p) => match p.rows.last() {
                Some(last) => Prev::MergeInto(last.input.clone()),
                None => Prev::Remove,
            },
            EbKind::Table(_) | EbKind::Divider | EbKind::Original { .. } => Prev::Remove,
            EbKind::Image { cid, .. } => Prev::RemoveImage(cid.clone()),
        };

        match prev {
            Prev::MergeInto(input) => {
                input.update(cx, |s, cx| {
                    let junction = s.text().len();
                    let merged = format!("{}{}", s.value(), text);
                    s.set_value(merged, window, cx);
                    s.set_cursor_offset(junction, window, cx);
                });
                self.blocks.remove(ix);
            }
            Prev::Remove => {
                self.blocks.remove(ix - 1);
            }
            Prev::RemoveImage(cid) => {
                self.blocks.remove(ix - 1);
                self.prune_image(&cid, cx);
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Up/Down navigation between blocks
    // ------------------------------------------------------------------

    /// Input for a text block or list item.
    fn input_for(&self, bid: u64, row: Option<usize>) -> Option<Entity<InputState>> {
        let ix = self.block_ix(bid)?;
        match (&self.blocks[ix].kind, row) {
            (EbKind::Text(tb), _) => Some(tb.input.clone()),
            (EbKind::List(lb), Some(rx)) => lb.rows.get(rx).map(|r| r.input.clone()),
            _ => None,
        }
    }

    /// Captured Up/Down: first let the input move its cursor (it
    /// moves through visual lines, which cannot be measured from here),
    /// then check later: if the cursor did not move, it was at the boundary, so
    /// move to the neighboring block.
    fn on_arrow(
        &mut self,
        bid: u64,
        row: Option<usize>,
        dir: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.input_for(bid, row) else {
            return;
        };
        let before = input.read(cx).cursor();
        let preferred_column = input.read(cx).preferred_cursor_column();
        cx.defer_in(window, move |this, window, cx| {
            if input.read(cx).cursor() == before {
                this.jump_adjacent(bid, row, dir, preferred_column, window, cx);
            }
        });
    }

    /// Focuses the neighbor (`dir` -1 = previous, +1 = next): the neighboring
    /// text-block/list-item input, or block selection for non-text blocks
    /// (image, separator, or original message).
    fn jump_adjacent(
        &mut self,
        bid: u64,
        row: Option<usize>,
        dir: i32,
        preferred_column: Option<(Pixels, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        enum Nav {
            Input(Entity<InputState>),
            Block(u64),
        }
        let mut entries: Vec<(usize, Option<usize>, Nav)> = Vec::new();
        for (ix, b) in self.blocks.iter().enumerate() {
            match &b.kind {
                EbKind::Text(tb) => entries.push((ix, None, Nav::Input(tb.input.clone()))),
                EbKind::List(lb) => {
                    for (rx, r) in lb.rows.iter().enumerate() {
                        entries.push((ix, Some(rx), Nav::Input(r.input.clone())));
                    }
                }
                _ => entries.push((ix, None, Nav::Block(b.id))),
            }
        }
        let Some(ix) = self.block_ix(bid) else { return };
        let Some(pos) = entries
            .iter()
            .position(|(bix, rix, _)| *bix == ix && *rix == row)
        else {
            return;
        };
        let target = if dir < 0 {
            pos.checked_sub(1).map(|p| &entries[p])
        } else {
            entries.get(pos + 1)
        };
        let Some((_, _, target)) = target else { return };
        match target {
            Nav::Input(input) => {
                input.update(cx, |state, cx| {
                    state.move_to_visual_edge(dir < 0, preferred_column, window, cx);
                });
                cx.notify();
            }
            Nav::Block(tbid) => {
                let tbid = *tbid;
                self.select_blocks(tbid, tbid, window, cx);
            }
        }
    }

    /// Up/Down while a block is selected moves the selection.
    fn move_selection(&mut self, dir: i32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, head)) = self.sel else { return };
        let Some(ix) = self.block_ix(head) else {
            return;
        };
        let nix = if dir < 0 {
            ix.saturating_sub(1)
        } else {
            (ix + 1).min(self.blocks.len().saturating_sub(1))
        };
        let bid = self.blocks[nix].id;
        self.select_blocks(bid, bid, window, cx);
    }

    fn on_select_prev(&mut self, _: &SelectPrevBlock, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, window, cx);
    }

    fn on_select_next(&mut self, _: &SelectNextBlock, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, window, cx);
    }

    /// Enter on a selected block resumes editing. For an image or another
    /// non-text block, opens the next editable block or creates
    /// un paragraphe juste en dessous.
    fn on_focus_selected(
        &mut self,
        _: &FocusSelectedBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, head)) = self.sel else { return };
        let Some(ix) = self.block_ix(head) else {
            return;
        };
        let textual = matches!(
            self.blocks[ix].kind,
            EbKind::Text(_) | EbKind::List(_) | EbKind::Table(_)
        );
        let input = match &self.blocks[ix].kind {
            EbKind::Text(tb) => Some(tb.input.clone()),
            EbKind::List(lb) => lb.rows.first().map(|r| r.input.clone()),
            EbKind::Table(table) => table
                .rows
                .first()
                .and_then(|row| row.first())
                .map(|cell| cell.input.clone()),
            EbKind::Image { .. } | EbKind::Divider | EbKind::Original { .. } => {
                self.blocks.get(ix + 1).and_then(|block| match &block.kind {
                    EbKind::Text(tb) => Some(tb.input.clone()),
                    EbKind::List(lb) => lb.rows.first().map(|row| row.input.clone()),
                    EbKind::Table(table) => table
                        .rows
                        .first()
                        .and_then(|row| row.first())
                        .map(|cell| cell.input.clone()),
                    _ => None,
                })
            }
        };
        if let Some(input) = input {
            self.sel = None;
            let offset = if textual {
                input.read(cx).text().len()
            } else {
                0
            };
            Self::focus_at(&input, offset, window, cx);
            cx.notify();
        } else if !textual {
            self.sel = None;
            self.insert_paragraph_after(head, window, cx);
        }
    }

    // ------------------------------------------------------------------
    // Tab within lists
    // ------------------------------------------------------------------

    fn on_indent(&mut self, bid: u64, rx: usize, delta: i8, cx: &mut Context<Self>) {
        let Some(ix) = self.block_ix(bid) else { return };
        cx.stop_propagation();
        let new_indent = match &self.blocks[ix].kind {
            EbKind::List(lb) if rx < lb.rows.len() => {
                let cap = if rx == 0 {
                    0
                } else {
                    lb.rows[rx - 1].indent + 1
                };
                let cur = lb.rows[rx].indent;
                let new = if delta > 0 {
                    (cur + 1).min(cap)
                } else {
                    cur.saturating_sub(1)
                };
                if new == cur {
                    return; // already at the boundary: nothing to undo
                }
                new
            }
            _ => return,
        };
        self.push_undo(cx);
        if let EbKind::List(lb) = &mut self.blocks[ix].kind {
            lb.rows[rx].indent = new_indent;
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // Markdown prefixes while typing
    // ------------------------------------------------------------------

    fn on_input_event(
        &mut self,
        input: &Entity<InputState>,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            InputEvent::Change => {
                self.apply_input_highlights(input, cx);
                if self
                    .spellcheck_inputs()
                    .iter()
                    .any(|candidate| candidate.entity_id() == input.entity_id())
                {
                    self.schedule_spellcheck(
                        input.clone(),
                        std::time::Duration::from_millis(240),
                        cx,
                    );
                    if self.languagetool_settings.automatic_check {
                        if let Some((block_id, _)) = self
                            .proofreading_inputs()
                            .into_iter()
                            .find(|(_, candidate)| candidate.entity_id() == input.entity_id())
                        {
                            self.schedule_languagetool(
                                block_id,
                                input.clone(),
                                std::time::Duration::from_millis(700),
                                cx,
                            );
                        }
                    }
                }
                if self.ignored_input_changes.remove(&input.entity_id()) {
                    return;
                }
            }
            InputEvent::Focus => {
                // gpui-component deliberately retains the selection of a
                // input on blur; with one input per block, those highlights
                // would accumulate, so collapse them in other blocks.
                self.unselect_others(Some(input.entity_id()), window, cx);
                return;
            }
            _ => return,
        }
        self.select_all_armed = None;
        self.note_text_change(input, cx);
        let iid = input.entity_id();
        let Some(ix) = self
            .blocks
            .iter()
            .position(|b| matches!(&b.kind, EbKind::Text(tb) if tb.input.entity_id() == iid))
        else {
            return;
        };
        {
            let EbKind::Text(tb) = &self.blocks[ix].kind else {
                return;
            };
            if tb.style != TextStyle::Paragraph {
                return;
            }
        }
        let (v, cur) = {
            let st = input.read(cx);
            (st.value().to_string(), st.cursor())
        };
        // A prefix transforms the block only when the cursor is immediately
        // after it, meaning it was just typed.
        let bid = self.blocks[ix].id;
        if v == "---" && cur == 3 {
            self.push_undo(cx);
            self.blocks[ix].kind = EbKind::Divider;
            let nb = self.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
            if let EbKind::Text(ntb) = &nb.kind {
                ntb.input.update(cx, |s, cx| s.focus(window, cx));
            }
            self.blocks.insert(ix + 1, nb);
            cx.notify();
            return;
        }
        if v == "```" && cur == 3 {
            self.restyle_text(bid, TextStyle::Code, String::new(), window, cx);
            return;
        }
        let prefixes: &[(&str, TextStyle)] = &[
            ("# ", TextStyle::Heading(1)),
            ("## ", TextStyle::Heading(2)),
            ("### ", TextStyle::Heading(3)),
            ("> ", TextStyle::Quote),
        ];
        for (prefix, style) in prefixes {
            if cur == prefix.len() {
                if let Some(rest) = v.strip_prefix(prefix) {
                    self.restyle_text(bid, *style, rest.to_string(), window, cx);
                    return;
                }
            }
        }
        for prefix in ["- ", "* ", "+ "] {
            if cur == prefix.len() && v.starts_with(prefix) {
                self.convert_to_list(ix, false, v[prefix.len()..].to_string(), window, cx);
                return;
            }
        }
        // `1.`, `12.`, etc. followed by a space, with the cursor just after it.
        if let Some(dot) = v.find(". ") {
            if dot > 0 && cur == dot + 2 && v[..dot].chars().all(|c| c.is_ascii_digit()) {
                self.convert_to_list(ix, true, v[dot + 2..].to_string(), window, cx);
            }
        }
    }

    /// Restyles a text block in place using the same input and new content.
    fn restyle_text(
        &mut self,
        bid: u64,
        style: TextStyle,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        if !matches!(&self.blocks[ix].kind, EbKind::Text(_)) {
            return;
        }
        self.push_undo(cx);
        if let EbKind::Text(tb) = &mut self.blocks[ix].kind {
            tb.style = style;
            let input = tb.input.clone();
            input.update(cx, |s, cx| {
                s.set_value(text, window, cx);
                let end = s.text().len();
                s.set_cursor_offset(end, window, cx);
            });
            cx.notify();
        }
    }

    /// Replaces text block `ix` with a one-item list.
    fn convert_to_list(
        &mut self,
        ix: usize,
        ordered: bool,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_undo(cx);
        let row = self.make_row(0, &text, window, cx);
        Self::focus_at(&row.input, text.len(), window, cx);
        self.blocks[ix].kind = EbKind::List(ListBlock {
            ordered,
            rows: vec![row],
        });
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Block-menu actions
    // ------------------------------------------------------------------

    fn set_style(
        &mut self,
        bid: u64,
        target: StyleTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };

        enum Plan {
            Restyle(TextStyle),
            TextToList(bool),
            ListToText(TextStyle),
            SetOrdered(bool),
            Nothing,
        }
        let plan = match (&self.blocks[ix].kind, target) {
            (EbKind::Text(_), StyleTarget::Bullets) => Plan::TextToList(false),
            (EbKind::Text(_), StyleTarget::Numbered) => Plan::TextToList(true),
            (EbKind::Text(_), t) => t.text_style().map(Plan::Restyle).unwrap_or(Plan::Nothing),
            (EbKind::List(_), StyleTarget::Bullets) => Plan::SetOrdered(false),
            (EbKind::List(_), StyleTarget::Numbered) => Plan::SetOrdered(true),
            (EbKind::List(_), t) => t
                .text_style()
                .map(Plan::ListToText)
                .unwrap_or(Plan::Nothing),
            _ => Plan::Nothing,
        };

        if !matches!(plan, Plan::Nothing) {
            self.push_undo(cx);
        }
        match plan {
            Plan::Restyle(style) => {
                if let EbKind::Text(tb) = &mut self.blocks[ix].kind {
                    tb.style = style;
                }
            }
            Plan::TextToList(ordered) => {
                let text = match &self.blocks[ix].kind {
                    EbKind::Text(tb) => tb.input.read(cx).value().to_string(),
                    _ => return,
                };
                let mut rows: Vec<ListRow> = Vec::new();
                for line in text.lines() {
                    let r = self.make_row(0, line, window, cx);
                    rows.push(r);
                }
                if rows.is_empty() {
                    rows.push(self.make_row(0, "", window, cx));
                }
                self.blocks[ix].kind = EbKind::List(ListBlock { ordered, rows });
            }
            Plan::ListToText(style) => {
                let text = match &self.blocks[ix].kind {
                    EbKind::List(lb) => lb
                        .rows
                        .iter()
                        .map(|r| r.input.read(cx).value().to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => return,
                };
                let nb = self.make_text(style, text, "", window, cx);
                self.blocks[ix] = nb;
            }
            Plan::SetOrdered(ordered) => {
                if let EbKind::List(lb) = &mut self.blocks[ix].kind {
                    lb.ordered = ordered;
                }
            }
            Plan::Nothing => return,
        }
        cx.notify();
    }

    fn delete_block(&mut self, bid: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.block_ix(bid) else { return };
        self.push_undo(cx);
        let cid = match &self.blocks[ix].kind {
            EbKind::Image { cid, .. } => Some(cid.clone()),
            _ => None,
        };
        self.blocks.remove(ix);
        if let Some(cid) = cid {
            self.prune_image(&cid, cx);
        }
        self.ensure_not_empty(window, cx);
        cx.notify();
    }

    fn move_block(&mut self, bid: u64, dir: i32, cx: &mut Context<Self>) {
        let Some(ix) = self.block_ix(bid) else { return };
        let to = if dir < 0 {
            ix.checked_sub(1)
        } else if ix + 1 < self.blocks.len() {
            Some(ix + 1)
        } else {
            None
        };
        if let Some(to) = to {
            self.push_undo(cx);
            self.blocks.swap(ix, to);
            cx.notify();
        }
    }

    fn insert_paragraph_after(&mut self, bid: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.block_ix(bid) else { return };
        self.push_undo(cx);
        let nb = self.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
        if let EbKind::Text(tb) = &nb.kind {
            tb.input.update(cx, |s, cx| s.focus(window, cx));
        }
        self.blocks.insert(ix + 1, nb);
        cx.notify();
    }

    /// Click below the final block: focus a trailing paragraph, creating it if
    /// needed, like clicking empty space on a Notion page.
    fn focus_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(EbBlock {
            kind: EbKind::Text(tb),
            ..
        }) = self.blocks.last()
        {
            if tb.input.read(cx).value().is_empty() {
                tb.input.update(cx, |s, cx| s.focus(window, cx));
                return;
            }
        }
        self.push_undo(cx);
        let nb = self.make_text(TextStyle::Paragraph, String::new(), "", window, cx);
        if let EbKind::Text(tb) = &nb.kind {
            tb.input.update(cx, |s, cx| s.focus(window, cx));
        }
        self.blocks.push(nb);
        cx.notify();
    }
}

#[cfg(test)]
mod template_cursor_tests {
    use super::history::unchanged_edges;
    use super::{
        fitted_image_width, inline_format_highlights, readable_link_color, strip_template_cursor,
        InlineColors, INLINE_SYNTAX_FADE_OUT, LINK_MAX_LIGHTNESS, LINK_MIN_LIGHTNESS,
        LINK_MIN_SATURATION,
    };
    use crate::blocks::BlockKind;
    use gpui::{FontStyle, FontWeight};

    #[test]
    fn removes_marker_and_preserves_offset() {
        let (clean, offset) = strip_template_cursor("Hello {{cursor}}signature")
            .expect("marker should be recognized");

        assert_eq!(clean, "Hello signature");
        assert_eq!(offset, "Hello ".len());
        assert!(strip_template_cursor("No marker").is_none());
    }

    #[test]
    fn undoing_split_preserves_signature_suffix() {
        let current = vec![
            BlockKind::Paragraph("Bon".to_string()),
            BlockKind::Paragraph("jour".to_string()),
            BlockKind::Paragraph("Signature".to_string()),
            BlockKind::Divider,
        ];
        let restored = vec![
            BlockKind::Paragraph("Hello".to_string()),
            BlockKind::Paragraph("Signature".to_string()),
            BlockKind::Divider,
        ];

        assert_eq!(unchanged_edges(&current, &restored), (0, 2));
    }

    /// Stand-ins for the theme colours these pure helpers receive rather than
    /// read from a context.
    fn sample_link_color() -> gpui::Hsla {
        gpui::hsla(0.6, 1., 0.5, 1.)
    }

    fn sample_destination_color() -> gpui::Hsla {
        gpui::hsla(0., 0., 0.55, 1.)
    }

    fn sample_colors() -> InlineColors {
        InlineColors {
            link: sample_link_color(),
            destination: sample_destination_color(),
        }
    }

    /// A link carries the same lightness as the words around it — the case that
    /// started this is OneDark, whose `primary` is dimmer than its body text.
    #[test]
    fn a_link_matches_the_lightness_of_the_body_text() {
        // OneDark as shipped: text #abb2bf, primary #61afef.
        let dark_text = gpui::hsla(0.61, 0.14, 0.71, 1.);
        let one_dark_blue = gpui::hsla(0.58, 0.82, 0.659, 1.);

        let corrected = readable_link_color(one_dark_blue, dark_text);

        assert_eq!(corrected.l, dark_text.l, "aligned with the text");
        assert_eq!(corrected.h, one_dark_blue.h, "the palette keeps its hue");
        assert_eq!(corrected.s, one_dark_blue.s);
    }

    /// The bounds exist so the hue survives: text that is nearly black or nearly
    /// white would otherwise drag the blue with it.
    #[test]
    fn link_lightness_stays_within_bounds_that_keep_the_hue() {
        let blue = gpui::hsla(0.61, 0.9, 0.5, 1.);
        let near_black_text = gpui::hsla(0., 0., 0.08, 1.);
        let near_white_text = gpui::hsla(0., 0., 0.97, 1.);

        assert_eq!(
            readable_link_color(blue, near_black_text).l,
            LINK_MIN_LIGHTNESS
        );
        assert_eq!(
            readable_link_color(blue, near_white_text).l,
            LINK_MAX_LIGHTNESS
        );

        // A near-grey primary is saturated enough to read as a link.
        let grey = gpui::hsla(0.61, 0.05, 0.5, 1.);
        assert!(readable_link_color(grey, near_white_text).s >= LINK_MIN_SATURATION);
    }

    #[test]
    fn inline_marks_become_visual_styles() {
        let value = "**bold** _italic_ <u>underlined</u> ~~struck~~";
        let highlights = inline_format_highlights(value, sample_colors());

        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "bold" && style.font_weight == Some(FontWeight::BOLD)
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "italic" && style.font_style == Some(FontStyle::Italic)
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "underlined" && style.underline.is_some()
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "struck" && style.strikethrough.is_some()
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "**" && style.fade_out == Some(INLINE_SYNTAX_FADE_OUT)
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "<u>" && style.fade_out == Some(INLINE_SYNTAX_FADE_OUT)
        }));
    }

    /// Both link forms colour what the reader sees and fade the rest, so the
    /// markdown stays editable without shouting.
    #[test]
    fn links_are_coloured_and_their_syntax_faded() {
        let value = "voir [Contact A](https://example.test/a) et <https://example.test/b>";
        let highlights = inline_format_highlights(value, sample_colors());

        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "Contact A"
                && style.color == Some(sample_link_color())
                && style.underline.is_some()
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "https://example.test/b"
                && style.color == Some(sample_link_color())
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "]" && style.fade_out == Some(INLINE_SYNTAX_FADE_OUT)
        }));
        // The destination is dimmed with a colour, not faded: `fade_out` blends
        // toward the background, and it is only on screen while the user is
        // reading or editing it.
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "(https://example.test/a)"
                && style.color == Some(sample_destination_color())
                && style.fade_out.is_none()
        }));
    }

    /// gpui's `compute_runs` walks highlights in the order given and turns each
    /// range into a run *length*, so an unsorted range makes its cursor regress
    /// and shifts every run after it — colouring the text that follows a link.
    /// The marks here are deliberately unsorted (a link emits its label before
    /// its brackets), which is why `BlockInput` sends everything through
    /// `combine_highlights` first. This pins the property that makes that safe.
    #[test]
    fn combined_highlights_are_sorted_and_disjoint() {
        let value = "voir [Testing](http://localhost) fin";
        let combined: Vec<_> =
            gpui::combine_highlights(inline_format_highlights(value, sample_colors()), [])
                .collect();

        let mut previous_end = 0;
        for (range, _) in &combined {
            assert!(
                range.start >= previous_end,
                "runs must never regress: {range:?} after {previous_end}"
            );
            previous_end = range.end;
        }
        assert!(
            combined.iter().any(|(range, style)| {
                &value[range.clone()] == "Testing" && style.color == Some(sample_link_color())
            }),
            "the label alone carries the link colour"
        );
        // What follows the link keeps the default style — the visible symptom of
        // the ordering bug was this text turning grey.
        let link_end = value.find(") fin").expect("fixture") + 1;
        assert!(
            combined
                .iter()
                .all(|(range, _)| range.start >= link_end || range.end <= link_end),
            "no run may straddle the end of the link"
        );
        assert!(
            !combined.iter().any(
                |(range, _)| range.start >= link_end && !value[range.clone()].trim().is_empty()
            ),
            "nothing past the link is styled"
        );
    }

    #[test]
    fn nested_inline_styles_are_combined() {
        let value = "**bold and _italic_**";
        let highlights = inline_format_highlights(value, sample_colors());

        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "bold and _italic_"
                && style.font_weight == Some(FontWeight::BOLD)
        }));
        assert!(highlights.iter().any(|(range, style)| {
            &value[range.clone()] == "italic" && style.font_style == Some(FontStyle::Italic)
        }));
    }

    #[test]
    fn pasted_image_width_keeps_aspect_ratio_and_caps_large_images() {
        assert_eq!(fitted_image_width(800, 400), Some(720));
        assert_eq!(fitted_image_width(2_000, 200), Some(1_600));
        assert_eq!(fitted_image_width(200, 800), Some(90));
        assert_eq!(fitted_image_width(0, 800), None);
    }
}

#[cfg(test)]
mod proofreading_tests {
    use super::proofreading::{languagetool_result_is_current, should_use_hunspell};
    use crate::proofreading::{LanguageToolCoverage, LanguageToolMode, LanguageToolSettings};

    #[test]
    fn mode_controls_hunspell_fallback() {
        let mut settings = LanguageToolSettings::default();
        assert!(should_use_hunspell(&settings, false));
        settings.mode = LanguageToolMode::ExternalUrl;
        settings.coverage = LanguageToolCoverage::GrammarOnly;
        assert!(should_use_hunspell(&settings, true));
        settings.coverage = LanguageToolCoverage::SpellingAndGrammar;
        assert!(!should_use_hunspell(&settings, true));
        assert!(should_use_hunspell(&settings, false));
    }

    #[test]
    fn stale_revisions_and_sources_are_rejected() {
        assert!(languagetool_result_is_current(
            Some(7),
            7,
            "current",
            "current"
        ));
        assert!(!languagetool_result_is_current(
            Some(8),
            7,
            "current",
            "current"
        ));
        assert!(!languagetool_result_is_current(
            Some(7),
            7,
            "changed",
            "current"
        ));
    }
}
