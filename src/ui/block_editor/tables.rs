//! Tables in the block editor.
//!
//! A table is a block holding one input per cell. Adding or removing a row or a
//! column therefore means creating or dropping those inputs, which is why the
//! operations live next to the constructors that build them.

use super::*;

impl BlockEditor {
    pub(super) fn insert_table(
        &mut self,
        row_count: usize,
        column_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row_count = row_count.max(1);
        let column_count = column_count.max(1);
        let at = self
            .focused_ix(window, cx)
            .map(|ix| ix + 1)
            .unwrap_or(self.blocks.len());
        self.push_undo(cx);
        let block = self.make_table(
            vec![vec![String::new(); column_count]; row_count],
            window,
            cx,
        );
        let first = match &block.kind {
            EbKind::Table(table) => table
                .rows
                .first()
                .and_then(|row| row.first())
                .map(|cell| cell.input.clone()),
            _ => None,
        };
        self.blocks.insert(at, block);
        if let Some(input) = first {
            Self::focus_at(&input, 0, window, cx);
        }
        cx.notify();
    }

    pub(super) fn focused_table_cell(
        &self,
        bid: u64,
        window: &Window,
        cx: &App,
    ) -> Option<(usize, usize)> {
        let ix = self.block_ix(bid)?;
        let EbKind::Table(table) = &self.blocks[ix].kind else {
            return None;
        };
        table.rows.iter().enumerate().find_map(|(row, cells)| {
            cells.iter().enumerate().find_map(|(column, cell)| {
                cell.input
                    .focus_handle(cx)
                    .is_focused(window)
                    .then_some((row, column))
            })
        })
    }

    pub(super) fn table_add_row(&mut self, bid: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.block_ix(bid) else { return };
        let (rows, columns) = match &self.blocks[ix].kind {
            EbKind::Table(table) => (
                table.rows.len(),
                table.rows.first().map(Vec::len).unwrap_or(1).max(1),
            ),
            _ => return,
        };
        let at = self
            .focused_table_cell(bid, window, cx)
            .map(|(row, _)| row + 1)
            .unwrap_or(rows);
        self.push_undo(cx);
        let new_row = (0..columns)
            .map(|_| self.make_table_cell("", window, cx))
            .collect::<Vec<_>>();
        let focus = new_row.first().map(|cell| cell.input.clone());
        if let EbKind::Table(table) = &mut self.blocks[ix].kind {
            table.rows.insert(at.min(table.rows.len()), new_row);
        }
        if let Some(input) = focus {
            Self::focus_at(&input, 0, window, cx);
        }
        cx.notify();
    }

    pub(super) fn table_remove_row(
        &mut self,
        bid: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        let rows = match &self.blocks[ix].kind {
            EbKind::Table(table) if table.rows.len() > 1 => table.rows.len(),
            _ => return,
        };
        let remove = self
            .focused_table_cell(bid, window, cx)
            .map(|(row, _)| row)
            .unwrap_or(rows - 1);
        self.push_undo(cx);
        let focus = if let EbKind::Table(table) = &mut self.blocks[ix].kind {
            table.rows.remove(remove.min(table.rows.len() - 1));
            table
                .rows
                .get(remove.min(table.rows.len() - 1))
                .and_then(|row| row.first())
                .map(|cell| cell.input.clone())
        } else {
            None
        };
        if let Some(input) = focus {
            Self::focus_at(&input, 0, window, cx);
        }
        cx.notify();
    }

    pub(super) fn table_add_column(
        &mut self,
        bid: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        let (rows, columns) = match &self.blocks[ix].kind {
            EbKind::Table(table) => (
                table.rows.len(),
                table.rows.first().map(Vec::len).unwrap_or(0),
            ),
            _ => return,
        };
        let at = self
            .focused_table_cell(bid, window, cx)
            .map(|(_, column)| column + 1)
            .unwrap_or(columns);
        self.push_undo(cx);
        let cells = (0..rows)
            .map(|_| self.make_table_cell("", window, cx))
            .collect::<Vec<_>>();
        let focus = cells.first().map(|cell| cell.input.clone());
        if let EbKind::Table(table) = &mut self.blocks[ix].kind {
            for (row, cell) in table.rows.iter_mut().zip(cells) {
                row.insert(at.min(row.len()), cell);
            }
        }
        if let Some(input) = focus {
            Self::focus_at(&input, 0, window, cx);
        }
        cx.notify();
    }

    pub(super) fn table_remove_column(
        &mut self,
        bid: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.block_ix(bid) else { return };
        let columns = match &self.blocks[ix].kind {
            EbKind::Table(table) if table.rows.first().is_some_and(|row| row.len() > 1) => {
                table.rows[0].len()
            }
            _ => return,
        };
        let remove = self
            .focused_table_cell(bid, window, cx)
            .map(|(_, column)| column)
            .unwrap_or(columns - 1);
        self.push_undo(cx);
        let focus = if let EbKind::Table(table) = &mut self.blocks[ix].kind {
            for row in &mut table.rows {
                row.remove(remove.min(row.len() - 1));
            }
            table
                .rows
                .first()
                .and_then(|row| row.get(remove.min(row.len() - 1)))
                .map(|cell| cell.input.clone())
        } else {
            None
        };
        if let Some(input) = focus {
            Self::focus_at(&input, 0, window, cx);
        }
        cx.notify();
    }

    pub(super) fn make_table_cell(
        &self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TableCell {
        let (input, _sub) = self.new_input(text, "", window, cx);
        TableCell { input, _sub }
    }

    pub(super) fn make_table(
        &mut self,
        mut rows: Vec<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EbBlock {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
        if rows.is_empty() {
            rows.push(vec![String::new(); columns]);
        }
        for row in &mut rows {
            row.resize(columns, String::new());
        }
        let rows = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|text| self.make_table_cell(&text, window, cx))
                    .collect()
            })
            .collect();
        EbBlock {
            id: self.alloc_id(),
            kind: EbKind::Table(TableBlock { rows }),
        }
    }
}
