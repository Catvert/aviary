//! Document-level undo and redo.
//!
//! History is a stack of whole-document snapshots (block kinds plus inline
//! images), pushed before every structural operation and once per burst of
//! typing. Restoring one reuses the blocks that did not change
//! ([`unchanged_edges`]) so the inputs the user is not looking at keep their
//! entities, and focus lands on the first block that actually moved.

use super::*;

/// Window for coalescing keystrokes into one undo step.
pub(super) const UNDO_COALESCE_MS: u128 = 1200;
/// Maximum history depth.
pub(super) const UNDO_CAP: usize = 100;

/// Strictly unchanged portions on either side of a mutation. The bound prevents
/// the suffix from overlapping the prefix when identical blocks repeat.
pub(super) fn unchanged_edges(current: &[BlockKind], restored: &[BlockKind]) -> (usize, usize) {
    let prefix = current
        .iter()
        .zip(restored)
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = current
        .len()
        .saturating_sub(prefix)
        .min(restored.len().saturating_sub(prefix));
    let suffix = current
        .iter()
        .rev()
        .zip(restored.iter().rev())
        .take(max_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    (prefix, suffix)
}

/// Complete document state at one instant (structure, text, and inline
/// attachments), sufficient to reconstruct the editor.
#[derive(Clone, Default, PartialEq)]
pub(super) struct Snapshot {
    pub(super) kinds: Vec<BlockKind>,
    pub(super) images: Vec<InlineImage>,
}

impl BlockEditor {
    /// Faithful document snapshot used by undo/redo, including editor-owned
    /// images and the exact block structure.
    pub(super) fn exact_snapshot(&self, cx: &App) -> Snapshot {
        let kinds = self
            .blocks
            .iter()
            .map(|b| match self.export_block(b, cx) {
                Some(block) => block.kind,
                None => BlockKind::Paragraph(match &b.kind {
                    EbKind::Text(tb) => tb.input.read(cx).value().to_string(),
                    _ => String::new(),
                }),
            })
            .collect();
        Snapshot {
            kinds,
            images: self.images.clone(),
        }
    }

    /// Call before every structural mutation: pushes the
    /// current state and ends active keystroke coalescing.
    pub(super) fn push_undo(&mut self, cx: &App) {
        let snap = self.exact_snapshot(cx);
        self.push_snap(snap);
        self.last_edit = None;
    }

    pub(super) fn push_snap(&mut self, snap: Snapshot) {
        if self.undo.last() == Some(&snap) {
            return; // duplicate, e.g. deferred Change from a programmatic set_value
        }
        self.undo.push(snap);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Keystroke in an input: pushes the state from before the burst (the `mirror`
    /// from the latest render), unless it extends the active burst.
    pub(super) fn note_text_change(&mut self, input: &Entity<InputState>, cx: &App) {
        let id = input.entity_id();
        let now = std::time::Instant::now();
        let coalesce = self
            .last_edit
            .as_ref()
            .is_some_and(|(last, t)| *last == id && t.elapsed().as_millis() < UNDO_COALESCE_MS);
        if !coalesce && self.mirror != self.exact_snapshot(cx) {
            let snap = self.mirror.clone();
            self.push_snap(snap);
        }
        self.last_edit = Some((id, now));
    }

    pub(super) fn undo_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snap) = self.undo.pop() else { return };
        let cur = self.exact_snapshot(cx);
        self.redo.push(cur);
        self.apply_snapshot(snap, window, cx);
    }

    pub(super) fn redo_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snap) = self.redo.pop() else { return };
        let cur = self.exact_snapshot(cx);
        self.undo.push(cur);
        self.apply_snapshot(snap, window, cx);
    }

    /// Restores the editor from a snapshot and focuses the first block that
    /// differs from previous state. Unchanged blocks are preserved to
    /// avoid unnecessarily invalidating signature/quote renders.
    pub(super) fn apply_snapshot(
        &mut self,
        snap: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = self.exact_snapshot(cx);
        let first_diff = snap
            .kinds
            .iter()
            .zip(prev.kinds.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| prev.kinds.len().min(snap.kinds.len().saturating_sub(1)));

        if !self.apply_snapshot_in_place(&snap, window, cx)
            && !self.apply_snapshot_reusing_edges(&prev, &snap, window, cx)
        {
            self.images = snap.images.clone();
            self.blocks.clear();
            let mut ebs = Vec::new();
            for kind in snap.kinds.clone() {
                let b = self.import_kind(kind, window, cx);
                ebs.push(b);
            }
            self.blocks = ebs;
            self.template_cursor = None;
        }
        self.sel = None;
        self.drag_anchor = None;
        self.resize = None;
        self.last_edit = None;
        self.ensure_not_empty(window, cx);
        self.mirror = self.exact_snapshot(cx);

        // Focus the restored location at the end of the first changed block.
        let inputs = self.nav_inputs();
        let target = inputs
            .iter()
            .find(|(bix, _, _)| *bix >= first_diff)
            .or_else(|| inputs.last());
        if let Some((_, _, input)) = target {
            let end = input.read(cx).text().len();
            Self::focus_at(input, end, window, cx);
        }
        cx.notify();
    }

    /// A structural mutation (Enter, deletion, paste, etc.) must replace only
    /// the genuinely different region. Identical suffixes
    /// typically the signature beneath typed text, retain their IDs, inputs,
    /// and render caches.
    pub(super) fn apply_snapshot_reusing_edges(
        &mut self,
        current: &Snapshot,
        restored: &Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if current.images != restored.images || self.blocks.len() != current.kinds.len() {
            return false;
        }
        let (prefix, suffix) = unchanged_edges(&current.kinds, &restored.kinds);
        let suffix_start = self.blocks.len().saturating_sub(suffix);
        let preserved_suffix = self.blocks.split_off(suffix_start);
        self.blocks.truncate(prefix);
        self.images = restored.images.clone();

        let changed_end = restored.kinds.len().saturating_sub(suffix);
        for kind in restored.kinds[prefix..changed_end].iter().cloned() {
            let block = self.import_kind(kind, window, cx);
            self.blocks.push(block);
        }
        self.blocks.extend(preserved_suffix);

        if self.template_cursor.is_some_and(|(target, _)| {
            !self
                .all_inputs()
                .iter()
                .any(|input| input.entity_id() == target)
        }) {
            self.template_cursor = None;
        }
        true
    }

    /// Restores content without recreating entities when the document structure
    /// document is identical. Unchanged signatures, images, and HTML fragments
    /// involved retain their identity and render cache.
    pub(super) fn apply_snapshot_in_place(
        &mut self,
        snap: &Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.blocks.len() != snap.kinds.len()
            || self.images != snap.images
            || !self
                .blocks
                .iter()
                .zip(&snap.kinds)
                .all(|(block, kind)| Self::same_snapshot_shape(block, kind))
        {
            return false;
        }

        let mut input_updates = Vec::new();
        for (block, kind) in self.blocks.iter_mut().zip(&snap.kinds) {
            match (&mut block.kind, kind) {
                (EbKind::Text(text), BlockKind::Paragraph(value))
                | (EbKind::Text(text), BlockKind::Quote(value)) => {
                    input_updates.push((text.input.clone(), value.clone()));
                }
                (EbKind::Text(text), BlockKind::Heading { text: value, .. }) => {
                    input_updates.push((text.input.clone(), value.clone()));
                }
                (
                    EbKind::Text(text),
                    BlockKind::Code {
                        language,
                        text: value,
                    },
                ) => {
                    text.language.clone_from(language);
                    input_updates.push((text.input.clone(), value.clone()));
                }
                (EbKind::List(list), BlockKind::List { ordered, items }) => {
                    list.ordered = *ordered;
                    for (row, item) in list.rows.iter_mut().zip(items) {
                        row.indent = item.indent;
                        input_updates.push((row.input.clone(), item.text.clone()));
                    }
                }
                (EbKind::Table(table), BlockKind::Table { rows }) => {
                    for (cells, values) in table.rows.iter().zip(rows) {
                        for (cell, value) in cells.iter().zip(values) {
                            input_updates.push((cell.input.clone(), value.clone()));
                        }
                    }
                }
                (
                    EbKind::Image { width, .. },
                    BlockKind::Image {
                        width: restored, ..
                    },
                ) => *width = *restored,
                (EbKind::Divider, BlockKind::Divider) | (EbKind::Original { .. }, _) => {}
                _ => unreachable!("snapshot shape checked before in-place restore"),
            }
        }

        for (input, value) in input_updates {
            if input.read(cx).value().as_ref() != value {
                self.ignored_input_changes.insert(input.entity_id());
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
        }
        true
    }

    pub(super) fn same_snapshot_shape(block: &EbBlock, kind: &BlockKind) -> bool {
        match (&block.kind, kind) {
            (EbKind::Text(text), BlockKind::Paragraph(_)) => text.style == TextStyle::Paragraph,
            (EbKind::Text(text), BlockKind::Heading { level, .. }) => {
                text.style == TextStyle::Heading((*level).clamp(1, 3))
            }
            (EbKind::Text(text), BlockKind::Quote(_)) => text.style == TextStyle::Quote,
            (EbKind::Text(text), BlockKind::Code { .. }) => text.style == TextStyle::Code,
            (EbKind::List(list), BlockKind::List { items, .. }) => list.rows.len() == items.len(),
            (EbKind::Table(table), BlockKind::Table { rows }) => {
                table.rows.len() == rows.len()
                    && table
                        .rows
                        .iter()
                        .zip(rows)
                        .all(|(cells, values)| cells.len() == values.len())
            }
            (EbKind::Image { cid, .. }, BlockKind::Image { cid: restored, .. }) => cid == restored,
            (EbKind::Divider, BlockKind::Divider) => true,
            (EbKind::Original { kind: current }, restored) => current == restored,
            _ => false,
        }
    }

    pub(super) fn on_undo_blocks(
        &mut self,
        _: &UndoBlocks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undo_doc(window, cx);
    }

    pub(super) fn on_redo_blocks(
        &mut self,
        _: &RedoBlocks,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.redo_doc(window, cx);
    }
}
