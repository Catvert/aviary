//! Aviary-owned multiline input used by the block editor.
//!
//! The component deliberately depends only on GPUI's public text and IME
//! primitives. `gpui-component` still supplies the shared input actions and
//! visual theme, but no private editor state is extended or patched.

use super::display_map::{DisplayMap, FoldableRange};
use super::overlay_popover::{OverlayPopover, OverlayPopoverScroll};
use gpui::{
    actions, combine_highlights, div, fill, point, prelude::*, px, size, App, Bounds,
    ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity, EntityId,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, HighlightStyle,
    InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, RenderOnce, SharedString,
    StyleRefinement, Styled, StyledText, Subscription, TextLayout, UTF16Selection, Window,
};
use gpui_component::{
    menu::{ContextMenuExt as _, PopupMenuItem},
    ActiveTheme, StyledExt as _,
};
use std::{ops::Range, rc::Rc};

pub(crate) const INPUT_CONTEXT: &str = "BlockInput";

/// Cheap identity of a value, used to reject foldable ranges computed against
/// an earlier revision of the text.
fn value_fingerprint(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

const COMPLETION_WIDTH: Pixels = px(320.);
const COMPLETION_MAX_HEIGHT: Pixels = px(280.);

actions!(
    block_input,
    [
        Backspace,
        Delete,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine,
        DeleteToPreviousWordStart,
        DeleteToNextWordEnd,
        Enter,
        Escape,
        IndentInline,
        OutdentInline,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        MoveHome,
        MoveEnd,
        MoveToStart,
        MoveToEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        SelectAll,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        SelectToStartOfLine,
        SelectToEndOfLine,
        SelectToStart,
        SelectToEnd,
        SelectToPreviousWordStart,
        SelectToNextWordEnd,
        ShowCharacterPalette,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(INPUT_CONTEXT)),
        KeyBinding::new("delete", Delete, Some(INPUT_CONTEXT)),
        KeyBinding::new("enter", Enter, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-enter", Enter, Some(INPUT_CONTEXT)),
        KeyBinding::new("escape", Escape, Some(INPUT_CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(INPUT_CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(INPUT_CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(INPUT_CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(INPUT_CONTEXT)),
        KeyBinding::new("home", MoveHome, Some(INPUT_CONTEXT)),
        KeyBinding::new("end", MoveEnd, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-home", SelectToStartOfLine, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-end", SelectToEndOfLine, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-up", MoveToStart, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-down", MoveToEnd, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-shift-up", SelectToStart, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-shift-down", SelectToEnd, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-a", SelectAll, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-c", Copy, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-x", Cut, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-v", Paste, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-z", Undo, Some(INPUT_CONTEXT)),
        KeyBinding::new("secondary-shift-z", Redo, Some(INPUT_CONTEXT)),
        KeyBinding::new("tab", IndentInline, Some(INPUT_CONTEXT)),
        KeyBinding::new("shift-tab", OutdentInline, Some(INPUT_CONTEXT)),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(INPUT_CONTEXT)),
        KeyBinding::new(
            "cmd-backspace",
            DeleteToBeginningOfLine,
            Some(INPUT_CONTEXT),
        ),
        KeyBinding::new("cmd-delete", DeleteToEndOfLine, Some(INPUT_CONTEXT)),
        KeyBinding::new(
            "alt-backspace",
            DeleteToPreviousWordStart,
            Some(INPUT_CONTEXT),
        ),
        KeyBinding::new("alt-delete", DeleteToNextWordEnd, Some(INPUT_CONTEXT)),
        KeyBinding::new("alt-left", MoveToPreviousWord, Some(INPUT_CONTEXT)),
        KeyBinding::new("alt-right", MoveToNextWord, Some(INPUT_CONTEXT)),
        KeyBinding::new(
            "alt-shift-left",
            SelectToPreviousWordStart,
            Some(INPUT_CONTEXT),
        ),
        KeyBinding::new("alt-shift-right", SelectToNextWordEnd, Some(INPUT_CONTEXT)),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-space", ShowCharacterPalette, Some(INPUT_CONTEXT)),
        KeyBinding::new(
            "ctrl-backspace",
            DeleteToPreviousWordStart,
            Some(INPUT_CONTEXT),
        ),
        KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(INPUT_CONTEXT)),
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(INPUT_CONTEXT)),
        KeyBinding::new("ctrl-right", MoveToNextWord, Some(INPUT_CONTEXT)),
        KeyBinding::new(
            "ctrl-shift-left",
            SelectToPreviousWordStart,
            Some(INPUT_CONTEXT),
        ),
        KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(INPUT_CONTEXT)),
    ]);
}

#[derive(Clone)]
pub(crate) enum BlockInputEvent {
    Change,
    Focus,
    Blur,
}

impl EventEmitter<BlockInputEvent> for BlockInputState {}

#[derive(Clone)]
pub(crate) struct BlockCompletionItem {
    pub range: Range<usize>,
    pub label: SharedString,
    pub detail: SharedString,
    pub replacement: SharedString,
    /// Optional side effect associated with accepting the textual completion.
    /// Contact mentions use it to add the mentioned person to the recipients.
    pub on_accept: Option<BlockCompletionCallback>,
}

pub(crate) type BlockCompletionCallback = Rc<dyn Fn(&mut App) + 'static>;
pub(crate) type BlockCompletionProvider =
    Rc<dyn Fn(&str, usize) -> Vec<BlockCompletionItem> + 'static>;

pub(crate) type MouseContextMenuBuilder =
    Rc<dyn Fn(EntityId, &str, usize, &mut Window, &mut App) -> Vec<PopupMenuItem> + 'static>;

#[derive(Clone)]
struct CompletionMenu {
    items: Vec<BlockCompletionItem>,
    selected: usize,
}

pub(crate) struct BlockInputState {
    focus_handle: FocusHandle,
    value: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    selecting: bool,
    text_highlights: Vec<(Range<usize>, HighlightStyle)>,
    /// Ranges the owner allows hiding — a link's brackets and destination. The
    /// input decides *when* to hide them (the caret must be elsewhere); the
    /// owner decides *what* may be hidden, since it is the one that knows the
    /// markdown. See [`DisplayMap`].
    foldable: Vec<FoldableRange>,
    /// Fingerprint of the value `foldable` was computed against. The owner
    /// recomputes those ranges one frame after an edit, and applying stale
    /// offsets would hide whatever now sits there, so they are dropped instead.
    foldable_for: u64,
    last_layout: Option<TextLayout>,
    mouse_context_offset: usize,
    mouse_context_menu_builder: Option<MouseContextMenuBuilder>,
    completion_provider: Option<BlockCompletionProvider>,
    completion: Option<CompletionMenu>,
    completion_scroll: OverlayPopoverScroll,
    preferred_x: Option<Pixels>,
    /// Tracked rather than queried: an unfocused block keeps its caret where it
    /// was, so folds must ignore the selection once focus is gone — otherwise
    /// leaving a block by its link leaves that link expanded for good.
    focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl BlockInputState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.focused = true;
                cx.emit(BlockInputEvent::Focus);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.focused = false;
                this.selecting = false;
                this.completion = None;
                cx.emit(BlockInputEvent::Blur);
                cx.notify();
            }),
        ];
        Self {
            focus_handle,
            value: String::new(),
            placeholder: SharedString::default(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            selecting: false,
            text_highlights: Vec::new(),
            foldable: Vec::new(),
            foldable_for: 0,
            last_layout: None,
            mouse_context_offset: 0,
            mouse_context_menu_builder: None,
            completion_provider: None,
            completion: None,
            completion_scroll: OverlayPopoverScroll::default(),
            preferred_x: None,
            focused: false,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn auto_grow(self, _min_rows: usize, _max_rows: usize) -> Self {
        self
    }

    pub(crate) fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub(crate) fn default_value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self.selected_range = 0..0;
        self
    }

    pub(crate) fn value(&self) -> SharedString {
        self.value.clone().into()
    }

    pub(crate) fn text(&self) -> &str {
        &self.value
    }

    pub(crate) fn cursor(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn selection_range(&self) -> Range<usize> {
        self.selected_range.clone()
    }

    pub(crate) fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub(crate) fn set_value(
        &mut self,
        value: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.value = value.into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.preferred_x = None;
        self.refresh_completions();
        cx.emit(BlockInputEvent::Change);
        cx.notify();
    }

    pub(crate) fn insert(
        &mut self,
        value: impl AsRef<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_selection(value.as_ref(), window, cx);
    }

    pub(crate) fn set_cursor_offset(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.clip_offset(offset), cx);
        self.focus(window, cx);
    }

    /// Declares which ranges may be folded away. Recomputed by the owner
    /// whenever the text changes, alongside the highlights.
    pub(crate) fn set_foldable_ranges(
        &mut self,
        ranges: Vec<FoldableRange>,
        cx: &mut Context<Self>,
    ) {
        let fingerprint = value_fingerprint(&self.value);
        if self.foldable == ranges && self.foldable_for == fingerprint {
            return;
        }
        self.foldable = ranges;
        self.foldable_for = fingerprint;
        cx.notify();
    }

    /// The fold state this frame. Recomputed on demand rather than cached: it
    /// derives entirely from the value, the foldable ranges and the selection,
    /// so every caller necessarily agrees with the layout of the same frame.
    fn display_map(&self) -> DisplayMap {
        if self.foldable.is_empty() || self.foldable_for != value_fingerprint(&self.value) {
            return DisplayMap::default();
        }
        // Without focus there is no caret on screen, so nothing justifies
        // showing the markdown: fold everything, wherever the selection sits.
        let selection = self.focused.then(|| self.selected_range.clone());
        DisplayMap::new(&self.value, &self.foldable, selection.as_ref())
    }

    pub(crate) fn set_text_highlights(
        &mut self,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        cx: &mut Context<Self>,
    ) {
        if self.text_highlights != highlights {
            self.text_highlights = highlights;
            cx.notify();
        }
    }

    pub(crate) fn set_mouse_context_menu_builder(&mut self, builder: MouseContextMenuBuilder) {
        self.mouse_context_menu_builder = Some(builder);
    }

    pub(crate) fn set_completion_provider(&mut self, provider: BlockCompletionProvider) {
        self.completion_provider = Some(provider);
        self.refresh_completions();
    }

    pub(crate) fn focus(&self, window: &mut Window, _cx: &mut Context<Self>) {
        self.focus_handle.focus(window);
    }

    pub(crate) fn unselect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        cx.notify();
    }

    pub(crate) fn preferred_cursor_column(&self) -> Option<(Pixels, usize)> {
        let logical = self.logical_column(self.cursor());
        if let Some(x) = self.preferred_x {
            return Some((x, logical));
        }
        let layout = self.last_layout.as_ref()?;
        let position = layout.position_for_index(self.display_map().to_display(self.cursor()))?;
        Some((position.x - layout.bounds().left(), logical))
    }

    pub(crate) fn move_to_visual_edge(
        &mut self,
        at_end: bool,
        preferred_column: Option<(Pixels, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback = if at_end { self.value.len() } else { 0 };
        let Some(layout) = self.last_layout.as_ref() else {
            self.move_to(fallback, cx);
            self.focus(window, cx);
            return;
        };
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let x = bounds.left() + preferred_column.map(|(x, _)| x).unwrap_or_else(|| px(0.));
        let y = if at_end {
            bounds.bottom() - line_height / 2.
        } else {
            bounds.top() + line_height / 2.
        };
        let offset = layout
            .index_for_position(point(x, y))
            .unwrap_or_else(|offset| offset);
        let offset = self.display_map().to_source(offset).min(self.value.len());
        self.move_to(offset, cx);
        self.preferred_x = preferred_column.map(|(x, _)| x);
        self.focus(window, cx);
    }

    pub(crate) fn handle_completion_action(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.completion.is_none() {
            return false;
        }
        if action.as_any().is::<MoveUp>() {
            let menu = self.completion.as_mut().expect("checked above");
            menu.selected = menu.selected.saturating_sub(1);
            cx.notify();
            return true;
        }
        if action.as_any().is::<MoveDown>() {
            let menu = self.completion.as_mut().expect("checked above");
            menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
            cx.notify();
            return true;
        }
        if action.as_any().is::<Enter>() {
            let selected = self
                .completion
                .as_ref()
                .map(|menu| menu.selected)
                .unwrap_or(0);
            self.accept_completion(selected, window, cx);
            return true;
        }
        if action.as_any().is::<Escape>() {
            self.completion = None;
            cx.notify();
            return true;
        }
        false
    }

    fn refresh_completions(&mut self) {
        let Some(provider) = self.completion_provider.as_ref() else {
            self.completion = None;
            return;
        };
        let items = provider(&self.value, self.cursor());
        if items.is_empty() {
            self.completion = None;
        } else {
            let selected = self
                .completion
                .as_ref()
                .map(|menu| menu.selected.min(items.len().saturating_sub(1)))
                .unwrap_or(0);
            self.completion = Some(CompletionMenu { items, selected });
        }
    }

    fn accept_completion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self
            .completion
            .as_ref()
            .and_then(|menu| menu.items.get(index))
            .cloned()
        else {
            return;
        };
        self.selected_range = item.range;
        self.selection_reversed = false;
        self.replace_selection(item.replacement.as_ref(), window, cx);
        self.completion = None;
        if let Some(on_accept) = item.on_accept {
            on_accept(cx);
        }
    }

    fn clip_offset(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.value.len());
        while !self.value.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        offset
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset);
        self.value[..offset]
            .char_indices()
            .next_back()
            .map(|(ix, _)| ix)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset);
        self.value[offset..]
            .char_indices()
            .nth(1)
            .map(|(ix, _)| offset + ix)
            .unwrap_or(self.value.len())
    }

    fn word_class(ch: char) -> u8 {
        if ch.is_alphanumeric() || ch == '_' {
            2
        } else if ch.is_whitespace() {
            0
        } else {
            1
        }
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        let mut chars = self.value[..self.clip_offset(offset)].char_indices().rev();
        let Some((mut result, ch)) = chars.next() else {
            return 0;
        };
        let mut class = Self::word_class(ch);
        for (ix, ch) in chars {
            let next = Self::word_class(ch);
            if class == 0 {
                class = next;
                result = ix;
            } else if next == class {
                result = ix;
            } else {
                break;
            }
        }
        result
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset);
        let mut chars = self.value[offset..].char_indices();
        let Some((_, ch)) = chars.next() else {
            return self.value.len();
        };
        let mut class = Self::word_class(ch);
        let mut result = self.next_boundary(offset);
        for (ix, ch) in chars {
            let next = Self::word_class(ch);
            if next == class || class == 0 {
                class = next;
                result = offset + ix + ch.len_utf8();
            } else {
                return offset + ix;
            }
        }
        result.min(self.value.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.value[..self.clip_offset(offset)]
            .rfind('\n')
            .map(|ix| ix + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        let offset = self.clip_offset(offset);
        self.value[offset..]
            .find('\n')
            .map(|ix| offset + ix)
            .unwrap_or(self.value.len())
    }

    fn logical_column(&self, offset: usize) -> usize {
        self.value[self.line_start(offset)..self.clip_offset(offset)]
            .chars()
            .count()
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clip_offset(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.marked_range = None;
        self.preferred_x = None;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clip_offset(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.preferred_x = None;
        cx.notify();
    }

    /// Byte offset in the *source* for a point on screen. The layout only knows
    /// display offsets, so a folded link is crossed here.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        self.last_layout
            .as_ref()
            .map(|layout| {
                let display = layout
                    .index_for_position(position)
                    .unwrap_or_else(|offset| offset);
                self.clip_offset(self.display_map().to_source(display))
            })
            .unwrap_or(0)
    }

    fn word_range_at(&self, offset: usize) -> Range<usize> {
        if self.value.is_empty() {
            return 0..0;
        }
        let offset = self.clip_offset(offset.min(self.value.len().saturating_sub(1)));
        let ch = self.value[offset..].chars().next().unwrap_or(' ');
        let class = Self::word_class(ch);
        let mut start = offset;
        for (ix, ch) in self.value[..offset].char_indices().rev() {
            if Self::word_class(ch) != class {
                break;
            }
            start = ix;
        }
        let mut end = offset + ch.len_utf8();
        let tail_start = end;
        for (ix, ch) in self.value[tail_start..].char_indices() {
            if Self::word_class(ch) != class {
                break;
            }
            end = tail_start + ix + ch.len_utf8();
        }
        start..end.min(self.value.len())
    }

    fn replace_selection(&mut self, new_text: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let range = self
            .marked_range
            .take()
            .unwrap_or_else(|| self.selected_range.clone());
        self.value.replace_range(range.clone(), new_text);
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.preferred_x = None;
        self.refresh_completions();
        cx.emit(BlockInputEvent::Change);
        cx.notify();
    }

    fn delete_range(&mut self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = range;
        self.selection_reversed = false;
        self.replace_selection("", window, cx);
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn move_vertical(&mut self, direction: f32, select: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let map = self.display_map();
        let Some(position) = layout.position_for_index(map.to_display(self.cursor())) else {
            return;
        };
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let preferred_x = self.preferred_x.unwrap_or(position.x - bounds.left());
        let target_y = if direction < 0. {
            position.y - line_height / 2.
        } else {
            position.y + line_height * 1.5
        };
        // Keep the cursor unchanged at a visual boundary. The block editor
        // observes that unchanged offset after propagation and transfers focus
        // to the adjacent block.
        if target_y < bounds.top() || target_y >= bounds.bottom() {
            self.preferred_x = Some(preferred_x);
            return;
        }
        let target = point(bounds.left() + preferred_x, target_y);
        let offset = layout
            .index_for_position(target)
            .unwrap_or_else(|offset| offset);
        let offset = map.to_source(offset).min(self.value.len());
        if select {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
        self.preferred_x = Some(preferred_x);
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1., false, cx);
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1., false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1., true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1., true, cx);
    }

    fn move_home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_start(self.cursor()), cx);
    }

    fn move_end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_end(self.cursor()), cx);
    }

    fn move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.value.len(), cx);
    }

    fn move_previous_word(
        &mut self,
        _: &MoveToPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to(self.previous_word_boundary(self.cursor()), cx);
    }

    fn move_next_word(&mut self, _: &MoveToNextWord, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.value.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_line_start(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.line_start(self.cursor()), cx);
    }

    fn select_line_end(&mut self, _: &SelectToEndOfLine, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_end(self.cursor()), cx);
    }

    fn select_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.value.len(), cx);
    }

    fn select_previous_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.previous_word_boundary(self.cursor()), cx);
    }

    fn select_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(self.next_word_boundary(self.cursor()), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            self.previous_boundary(self.cursor())..self.cursor()
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            self.cursor()..self.next_boundary(self.cursor())
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn delete_line_start(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = if self.selected_range.is_empty() {
            self.line_start(self.cursor())..self.cursor()
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn delete_line_end(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = if self.selected_range.is_empty() {
            self.cursor()..self.line_end(self.cursor())
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn delete_previous_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = if self.selected_range.is_empty() {
            self.previous_word_boundary(self.cursor())..self.cursor()
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = if self.selected_range.is_empty() {
            self.cursor()..self.next_word_boundary(self.cursor())
        } else {
            self.selected_range.clone()
        };
        self.delete_range(range, window, cx);
    }

    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_selection("\n", window, cx);
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.value.get(self.selected_range.clone()) {
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            }
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_selection("", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_selection(&text, window, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.index_for_position(event.position);
        self.mouse_context_offset = offset;
        if event.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window);
        self.selecting = true;
        match event.click_count {
            2 => {
                self.selected_range = self.word_range_at(offset);
                self.selection_reversed = false;
            }
            count if count >= 3 => {
                self.selected_range = 0..self.value.len();
                self.selection_reversed = false;
            }
            _ if event.modifiers.shift => self.select_to(offset, cx),
            _ => self.move_to(offset, cx),
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_for_position(event.position), cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.value.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.value[..self.clip_offset(offset)]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }
}

impl EntityInputHandler for BlockInputState {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(range_utf16);
        adjusted_range.replace(self.range_to_utf16(range.clone()));
        self.value.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(self.selected_range.clone()),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .clone()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(range) = range_utf16 {
            self.selected_range = self.range_from_utf16(range);
            self.selection_reversed = false;
        }
        self.replace_selection(new_text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.value.replace_range(range.clone(), new_text);
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .map(|selection| self.range_from_utf16(selection))
            .map(|selection| range.start + selection.start..range.start + selection.end)
            .unwrap_or_else(|| {
                let cursor = range.start + new_text.len();
                cursor..cursor
            });
        self.selection_reversed = false;
        self.refresh_completions();
        cx.emit(BlockInputEvent::Change);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(range_utf16);
        let map = self.display_map();
        let start = layout.position_for_index(map.to_display(range.start))?;
        let end = layout.position_for_index(map.to_display(range.end))?;
        Some(Bounds::from_corners(
            start,
            point(end.x.max(start.x + px(1.)), end.y + layout.line_height()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_position(point)))
    }
}

impl Focusable for BlockInputState {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BlockInputState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        BlockInput::new(&cx.entity()).appearance(false)
    }
}

struct BlockTextElement {
    state: Entity<BlockInputState>,
    text: StyledText,
    layout: TextLayout,
}

struct BlockTextPrepaint {
    caret: Option<PaintQuad>,
}

impl IntoElement for BlockTextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for BlockTextElement {
    type RequestLayoutState = ();
    type PrepaintState = BlockTextPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.text
            .prepaint(None, inspector_id, bounds, state, window, cx);
        let input = self.state.read(cx);
        let caret = input
            .selected_range
            .is_empty()
            .then(|| {
                self.layout
                    .position_for_index(input.display_map().to_display(input.cursor()))
            })
            .flatten()
            .map(|position| {
                fill(
                    Bounds::new(position, size(px(1.5), self.layout.line_height())),
                    cx.theme().caret,
                )
            });
        BlockTextPrepaint { caret }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(None, inspector_id, bounds, state, &mut (), window, cx);
        let focus_handle = self.state.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.state.clone()),
            cx,
        );
        if focus_handle.is_focused(window) {
            if let Some(caret) = prepaint.caret.take() {
                window.paint_quad(caret);
            }
        }
        self.state.update(cx, |input, _| {
            input.last_layout = Some(self.layout.clone());
        });
    }
}

#[derive(IntoElement)]
pub(crate) struct BlockInput {
    state: Entity<BlockInputState>,
    style: StyleRefinement,
    appearance: bool,
    tab_index: isize,
}

impl BlockInput {
    pub(crate) fn new(state: &Entity<BlockInputState>) -> Self {
        Self {
            state: state.clone(),
            style: StyleRefinement::default(),
            appearance: true,
            tab_index: 0,
        }
    }

    pub(crate) fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }

    pub(crate) fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }
}

impl Styled for BlockInput {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for BlockInput {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.state.read(cx);
        let placeholder = state.value.is_empty();
        // Everything below the layout speaks display offsets: the folded ranges
        // are gone from the string, so every source range has to be mapped —
        // and the ones that vanished with them dropped.
        let map = state.display_map();
        let display: SharedString = if placeholder {
            state.placeholder.clone()
        } else {
            map.display_text(&state.value).into()
        };
        let mut highlights = if placeholder {
            vec![(
                0..display.len(),
                HighlightStyle {
                    color: Some(cx.theme().muted_foreground),
                    ..Default::default()
                },
            )]
        } else {
            state
                .text_highlights
                .iter()
                .filter_map(|(range, style)| {
                    map.range_to_display(range).map(|range| (range, *style))
                })
                .collect()
        };
        if !placeholder && !state.selected_range.is_empty() {
            if let Some(range) = map.range_to_display(&state.selected_range) {
                highlights.push((
                    range,
                    HighlightStyle {
                        background_color: Some(cx.theme().selection),
                        ..Default::default()
                    },
                ));
            }
        }
        if let Some(marked) = state
            .marked_range
            .clone()
            .and_then(|marked| map.range_to_display(&marked))
        {
            highlights.push((
                marked,
                HighlightStyle {
                    underline: Some(gpui::UnderlineStyle {
                        color: Some(cx.theme().caret),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            ));
        }
        // gpui turns highlights into consecutive run *lengths*, walking them in
        // the order given: an unsorted or overlapping range makes its internal
        // cursor regress and shifts every run after it. These are neither sorted
        // nor disjoint — inline marks emit a link's label before its brackets,
        // and a misspelling can sit inside one — so they all go through
        // `combine_highlights`, which sorts the endpoints and merges superposed
        // styles. Passing them straight to `with_highlights` mis-colours the
        // text from the first out-of-order range onward.
        let highlights: Vec<_> = combine_highlights(highlights, []).collect();
        let styled = StyledText::new(display).with_highlights(highlights);
        let layout = styled.layout().clone();
        let text_element = BlockTextElement {
            state: self.state.clone(),
            text: styled,
            layout,
        };
        let context_builder = state.mouse_context_menu_builder.clone();
        let completion = state.completion.clone();
        let completion_anchor = state.last_layout.as_ref().and_then(|layout| {
            let position = layout.position_for_index(map.to_display(state.cursor()))?;
            Some(point(
                position.x - layout.bounds().left(),
                position.y - layout.bounds().top() + layout.line_height(),
            ))
        });
        let menu_state = self.state.clone();
        let mut root = div()
            .id(("block-input", self.state.entity_id()))
            .relative()
            .flex()
            .min_w_0()
            .w_full()
            .h_auto()
            .key_context(INPUT_CONTEXT)
            .track_focus(&self.state.focus_handle(cx))
            .tab_index(self.tab_index)
            .cursor_text()
            .on_action(window.listener_for(&self.state, BlockInputState::backspace))
            .on_action(window.listener_for(&self.state, BlockInputState::delete))
            .on_action(window.listener_for(&self.state, BlockInputState::delete_line_start))
            .on_action(window.listener_for(&self.state, BlockInputState::delete_line_end))
            .on_action(window.listener_for(&self.state, BlockInputState::delete_previous_word))
            .on_action(window.listener_for(&self.state, BlockInputState::delete_next_word))
            .on_action(window.listener_for(&self.state, BlockInputState::enter))
            .on_action(window.listener_for(&self.state, BlockInputState::escape))
            .on_action(window.listener_for(&self.state, BlockInputState::move_left))
            .on_action(window.listener_for(&self.state, BlockInputState::move_right))
            .on_action(window.listener_for(&self.state, BlockInputState::move_up))
            .on_action(window.listener_for(&self.state, BlockInputState::move_down))
            .on_action(window.listener_for(&self.state, BlockInputState::select_left))
            .on_action(window.listener_for(&self.state, BlockInputState::select_right))
            .on_action(window.listener_for(&self.state, BlockInputState::select_up))
            .on_action(window.listener_for(&self.state, BlockInputState::select_down))
            .on_action(window.listener_for(&self.state, BlockInputState::move_home))
            .on_action(window.listener_for(&self.state, BlockInputState::move_end))
            .on_action(window.listener_for(&self.state, BlockInputState::move_to_start))
            .on_action(window.listener_for(&self.state, BlockInputState::move_to_end))
            .on_action(window.listener_for(&self.state, BlockInputState::move_previous_word))
            .on_action(window.listener_for(&self.state, BlockInputState::move_next_word))
            .on_action(window.listener_for(&self.state, BlockInputState::select_all))
            .on_action(window.listener_for(&self.state, BlockInputState::select_line_start))
            .on_action(window.listener_for(&self.state, BlockInputState::select_line_end))
            .on_action(window.listener_for(&self.state, BlockInputState::select_start))
            .on_action(window.listener_for(&self.state, BlockInputState::select_end))
            .on_action(window.listener_for(&self.state, BlockInputState::select_previous_word))
            .on_action(window.listener_for(&self.state, BlockInputState::select_next_word))
            .on_action(window.listener_for(&self.state, BlockInputState::copy))
            .on_action(window.listener_for(&self.state, BlockInputState::cut))
            .on_action(window.listener_for(&self.state, BlockInputState::paste))
            .on_action(window.listener_for(&self.state, BlockInputState::show_character_palette))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&self.state, BlockInputState::on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                window.listener_for(&self.state, BlockInputState::on_mouse_down),
            )
            .on_mouse_move(window.listener_for(&self.state, BlockInputState::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                window.listener_for(&self.state, BlockInputState::on_mouse_up),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                window.listener_for(&self.state, BlockInputState::on_mouse_up),
            )
            .when(self.appearance, |this| {
                this.bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().input)
                    .rounded(cx.theme().radius)
            })
            .refine_style(&self.style)
            // Keep the measured text in a width-constrained flex item. A bare
            // `StyledText` child contributes its intrinsic width to this flex
            // row, so Taffy may measure it without a definite available width
            // and GPUI cannot compute soft-wrap boundaries.
            .child(
                div()
                    .min_w_0()
                    .w_full()
                    .flex_1()
                    .whitespace_normal()
                    .child(text_element),
            )
            .context_menu(move |mut menu, window, cx| {
                let (value, offset) = {
                    let input = menu_state.read(cx);
                    (input.value.clone(), input.mouse_context_offset)
                };
                let custom = context_builder
                    .as_ref()
                    .map(|builder| builder(menu_state.entity_id(), &value, offset, window, cx))
                    .unwrap_or_default();
                let has_custom = !custom.is_empty();
                for item in custom {
                    menu = menu.item(item);
                }
                if has_custom {
                    menu = menu.separator();
                }
                menu.item(PopupMenuItem::new(tr!("block-input-cut")).action(Box::new(Cut)))
                    .item(PopupMenuItem::new(tr!("copy")).action(Box::new(Copy)))
                    .item(PopupMenuItem::new(tr!("block-input-paste")).action(Box::new(Paste)))
                    .separator()
                    .item(
                        PopupMenuItem::new(tr!("block-input-select-all"))
                            .action(Box::new(SelectAll)),
                    )
            });

        if let (Some(menu), Some(anchor)) = (completion, completion_anchor) {
            let selected = menu.selected;
            let input = self.state.clone();
            root = root.child(
                OverlayPopover::new(
                    ("block-completion-popup", self.state.entity_id()),
                    anchor.x,
                    anchor.y,
                    COMPLETION_WIDTH,
                    COMPLETION_MAX_HEIGHT,
                    state.completion_scroll.clone(),
                )
                .children(menu.items.into_iter().enumerate().map(
                    move |(index, item)| {
                        let input = input.clone();
                        div()
                            .id(("block-completion", index))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .when(index == selected, |this| {
                                this.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .hover(|this| this.bg(cx.theme().accent))
                            .child(div().text_xl().child(item.label))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_sm()
                                    .child(item.detail),
                            )
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                input.update(cx, |state, cx| {
                                    state.accept_completion(index, window, cx);
                                });
                            })
                    },
                )),
            );
        }
        root
    }
}
