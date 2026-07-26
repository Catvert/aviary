//! Captures keyboard actions around editor inputs.

use super::{BlockEditor, EbKind};
use crate::ui::components::block_input::{
    Backspace, BlockInputState, DeleteToPreviousWordStart, Enter, Escape, IndentInline, MoveDown,
    MoveUp, OutdentInline, Paste, Redo, SelectAll, Undo,
};
use gpui::{div, prelude::*, px, AnyElement, Context, Window};

impl BlockEditor {
    /// Gives an open completion popup first refusal on the keys that the block
    /// editor captures before `InputState`. Without this bridge, Enter would
    /// split a block and Up/Down would leave it instead of navigating the emoji
    /// suggestions.
    fn route_completion_action(
        input: &gpui::Entity<BlockInputState>,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let handled = input.update(cx, |state, cx| {
            state.handle_completion_action(action, window, cx)
        });
        if handled {
            cx.stop_propagation();
        }
        handled
    }

    /// Wraps a text input with structural captures. They live
    /// on an ancestor so it runs before the input's internal handlers.
    pub(super) fn wrap_text_actions(
        &self,
        content: AnyElement,
        input: &gpui::Entity<BlockInputState>,
        bid: u64,
        row: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enter_input = input.clone();
        let up_input = input.clone();
        let down_input = input.clone();
        let escape_input = input.clone();
        let delete_previous_input = input.clone();
        let mut element = div()
            .w_full()
            .capture_action(cx.listener(move |this, action: &Enter, window, cx| {
                this.select_all_armed = None;
                if Self::route_completion_action(&enter_input, Box::new(action.clone()), window, cx)
                {
                    return;
                }
                this.on_enter(bid, row, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &Backspace, window, cx| {
                this.select_all_armed = None;
                this.on_backspace(bid, row, window, cx);
            }))
            .capture_action(
                cx.listener(move |this, _: &DeleteToPreviousWordStart, window, cx| {
                    this.select_all_armed = None;
                    if delete_previous_input.read(cx).text().is_empty() {
                        this.on_backspace(bid, row, window, cx);
                    }
                }),
            )
            .capture_action(cx.listener(move |this, action: &MoveUp, window, cx| {
                if Self::route_completion_action(&up_input, Box::new(action.clone()), window, cx) {
                    return;
                }
                this.on_arrow(bid, row, -1, window, cx);
            }))
            .capture_action(cx.listener(move |this, action: &MoveDown, window, cx| {
                if Self::route_completion_action(&down_input, Box::new(action.clone()), window, cx)
                {
                    return;
                }
                this.on_arrow(bid, row, 1, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &Undo, window, cx| {
                cx.stop_propagation();
                this.undo_doc(window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &Redo, window, cx| {
                cx.stop_propagation();
                this.redo_doc(window, cx);
            }))
            .capture_action(cx.listener(move |this, action: &Escape, window, cx| {
                if Self::route_completion_action(
                    &escape_input,
                    Box::new(action.clone()),
                    window,
                    cx,
                ) {
                    return;
                }
                this.on_escape_input(bid, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &SelectAll, window, cx| {
                this.on_select_all_input(bid, row, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &Paste, window, cx| {
                this.select_all_armed = None;
                this.on_paste(bid, row, window, cx);
            }));
        if let Some(row_index) = row {
            element = element
                .capture_action(cx.listener(move |this, _: &IndentInline, _window, cx| {
                    this.on_indent(bid, row_index, 1, cx);
                }))
                .capture_action(cx.listener(move |this, _: &OutdentInline, _window, cx| {
                    this.on_indent(bid, row_index, -1, cx);
                }));
        } else {
            element = element
                .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                    cx.stop_propagation();
                    this.focus_outside_editor(true, window, cx);
                }))
                .capture_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                    cx.stop_propagation();
                    this.focus_outside_editor(false, window, cx);
                }));
        }
        element.child(content).into_any_element()
    }

    fn table_focus_adjacent(
        &mut self,
        bid: u64,
        row: usize,
        column: usize,
        direction: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(index) = self.block_ix(bid) else {
            return;
        };
        let (rows, columns) = match &self.blocks[index].kind {
            EbKind::Table(table) => (
                table.rows.len(),
                table.rows.first().map(Vec::len).unwrap_or(0),
            ),
            _ => return,
        };
        if rows == 0 || columns == 0 {
            return;
        }
        let current = row.saturating_mul(columns).saturating_add(column);
        let target = if direction < 0 {
            current.checked_sub(1)
        } else if current + 1 < rows * columns {
            Some(current + 1)
        } else {
            None
        };
        if let Some(target) = target {
            let target_row = target / columns;
            let target_column = target % columns;
            if let EbKind::Table(table) = &self.blocks[index].kind {
                if let Some(input) = table
                    .rows
                    .get(target_row)
                    .and_then(|cells| cells.get(target_column))
                    .map(|cell| cell.input.clone())
                {
                    Self::focus_at(&input, 0, window, cx);
                }
            }
        } else if direction > 0 {
            self.table_add_row(bid, window, cx);
        }
    }

    pub(super) fn wrap_table_cell_actions(
        &self,
        content: AnyElement,
        input: &gpui::Entity<BlockInputState>,
        bid: u64,
        row: usize,
        column: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let escape_input = input.clone();
        div()
            .min_w(px(120.))
            .flex_1()
            .capture_action(cx.listener(move |this, _: &Undo, window, cx| {
                cx.stop_propagation();
                this.undo_doc(window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &Redo, window, cx| {
                cx.stop_propagation();
                this.redo_doc(window, cx);
            }))
            .capture_action(cx.listener(move |this, action: &Escape, window, cx| {
                if Self::route_completion_action(
                    &escape_input,
                    Box::new(action.clone()),
                    window,
                    cx,
                ) {
                    return;
                }
                this.on_escape_input(bid, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &IndentInline, window, cx| {
                this.table_focus_adjacent(bid, row, column, 1, window, cx);
            }))
            .capture_action(cx.listener(move |this, _: &OutdentInline, window, cx| {
                this.table_focus_adjacent(bid, row, column, -1, window, cx);
            }))
            .child(content)
            .into_any_element()
    }
}
