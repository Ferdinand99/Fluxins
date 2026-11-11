// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tables, drawn like Fluxer's `.table`: a 1 px border with 6 px corners, a header row
//! on `--bg-table-header`, alternating row colours, cells 8 × 12 px padded at 14 px
//! text, columns as wide as their content. A table is cut to a size a message list can
//! draw; a note says so.

use fluxer_markdown_parser::ast::TableAlignment;
use fluxer_markdown_parser::Node;
use gpui::{div, prelude::*, px, AnyElement, FontWeight, SharedString};

use super::blocks::Draw;
use super::inline::{self, inline_of, Look};
use super::{Align, Inline, Table};
use crate::theme;

// Deviation: Fluxer draws every cell the parser gives (only its limit of 10 000 nodes
// applies). Inlet cuts a table to 20 columns and 400 cells, with a note, so one message
// cannot make the list lay out thousands of cells.

/// Most columns drawn.
pub(super) const MAX_COLUMNS: usize = 20;
/// Most cells drawn (header included); rows past it are left out.
pub(super) const MAX_CELLS: usize = 400;

fn cells_of(row: &Node, columns: usize) -> (Vec<Inline>, bool) {
    let Node::TableRow { cells } = row else {
        return (Vec::new(), false);
    };
    let cut = cells.len() > columns;
    let out = cells
        .iter()
        .take(columns)
        .map(|cell| match cell {
            Node::TableCell { children } => inline_of(children),
            other => inline_of(std::slice::from_ref(other)),
        })
        .collect();
    (out, cut)
}

/// Parsing: a table from the parser's header, alignments and rows, cut to the limits.
pub(super) fn convert(header: &Node, alignments: &[TableAlignment], rows: &[Node]) -> Table {
    let header_cells = match header {
        Node::TableRow { cells } => cells.len(),
        _ => 0,
    };
    let columns = header_cells.clamp(1, MAX_COLUMNS);
    let (header, mut cut) = cells_of(header, columns);
    let max_rows = (MAX_CELLS / columns).saturating_sub(1);
    let mut out_rows = Vec::with_capacity(rows.len().min(max_rows));
    for row in rows.iter().take(max_rows) {
        let (mut cells, wide) = cells_of(row, columns);
        cut |= wide;
        cells.resize_with(columns, Inline::default);
        out_rows.push(cells);
    }
    cut |= rows.len() > max_rows;
    let align = (0..columns)
        .map(|i| match alignments.get(i) {
            Some(TableAlignment::Center) => Align::Center,
            Some(TableAlignment::Right) => Align::Right,
            Some(TableAlignment::Left | TableAlignment::None) | None => Align::Left,
        })
        .collect();
    Table {
        header,
        align,
        rows: out_rows,
        cut,
    }
}

fn aligned(cell: gpui::Div, align: Align) -> gpui::Div {
    match align {
        Align::Left => cell.text_left(),
        Align::Center => cell.text_center(),
        Align::Right => cell.text_right(),
    }
}

/// Draws a table.
pub(super) fn render(id: &str, table: &Table, d: &Draw) -> AnyElement {
    let columns = table.header.len().max(1);
    let last_row = table.rows.len();
    let cell = |row: usize, col: usize, content: &Inline| {
        let header = row == 0;
        let bg = if header {
            theme::table_header_bg()
        } else if row % 2 == 1 {
            theme::table_row_odd()
        } else {
            theme::table_row_even()
        };
        let align = table.align.get(col).copied().unwrap_or(Align::Left);
        aligned(
            div()
                .min_w(px(80.))
                .px(px(12.))
                .py(px(8.))
                .bg(bg)
                .when(row != last_row, |c| {
                    c.border_b_1().border_color(theme::table_border())
                })
                .when(header, |c| {
                    c.font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_strong())
                }),
            align,
        )
        .child(inline::element(
            SharedString::from(format!("{id}-r{row}c{col}")),
            content,
            d,
            Look::sized(14.),
        ))
    };
    let mut grid = div()
        .grid()
        .grid_cols_max_content(columns as u16)
        .min_w_0()
        .border_1()
        .border_color(theme::table_border())
        .rounded(px(6.))
        .overflow_hidden()
        .text_size(px(14.))
        .line_height(px(19.6));
    for (col, content) in table.header.iter().enumerate() {
        grid = grid.child(cell(0, col, content));
    }
    for (r, row) in table.rows.iter().enumerate() {
        for (col, content) in row.iter().enumerate() {
            grid = grid.child(cell(r + 1, col, content));
        }
    }
    let note = table.cut.then(|| {
        div()
            .text_size(px(12.))
            .text_color(theme::text_muted())
            .child(t!("markdown-table-cut"))
    });
    div()
        .flex()
        .flex_col()
        .gap_1()
        .max_w_full()
        .child(div().flex().flex_row().child(grid))
        .children(note)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: usize, text: &str) -> Node {
        Node::TableRow {
            cells: (0..cells)
                .map(|_| Node::TableCell {
                    children: vec![Node::Text {
                        content: text.to_string(),
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn table_with_10000_cells_is_cut() {
        // 100 columns by 100 rows, built directly: the parser's own node limit would
        // stop earlier, and this must hold whatever reaches the converter.
        let header = row(100, "h");
        let rows: Vec<Node> = (0..99).map(|_| row(100, "c")).collect();
        let table = convert(&header, &[TableAlignment::Center; 100], &rows);
        let cells = table.header.len() + table.rows.iter().map(Vec::len).sum::<usize>();
        assert!(table.cut);
        assert!(cells <= MAX_CELLS, "{cells} cells");
        assert_eq!(table.header.len(), MAX_COLUMNS);
        assert!(table.rows.iter().all(|r| r.len() == MAX_COLUMNS));
        assert_eq!(table.align.len(), MAX_COLUMNS);

        // The same through the parser, quickly. Whatever it gives for 10 000 cells,
        // no table is drawn whole.
        let started = std::time::Instant::now();
        let blocks = super::super::parse(&table_text(100, 100));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!blocks.is_empty());
        for t in tables(&blocks) {
            assert!(t.cut);
            let cells = t.header.len() + t.rows.iter().map(Vec::len).sum::<usize>();
            assert!(cells <= MAX_CELLS, "{cells} cells");
        }
        // A table the parser does give (30 x 30) is cut the same way.
        let blocks = super::super::parse(&table_text(30, 30));
        let found = tables(&blocks);
        assert_eq!(found.len(), 1, "the parser gives a table: {blocks:?}");
        let t = found[0];
        assert!(t.cut);
        assert_eq!(t.header.len(), MAX_COLUMNS);
        let cells = t.header.len() + t.rows.iter().map(Vec::len).sum::<usize>();
        assert!(cells <= MAX_CELLS, "{cells} cells");
        assert_eq!(t.rows.len(), MAX_CELLS / MAX_COLUMNS - 1);
    }

    /// A markdown table of `columns` x `rows` (header included).
    fn table_text(columns: usize, rows: usize) -> String {
        let line = |c: &str| format!("|{}\n", format!(" {c} |").repeat(columns));
        let mut text = line("h");
        text.push_str(&format!("|{}\n", " - |".repeat(columns)));
        for _ in 1..rows {
            text.push_str(&line("c"));
        }
        text
    }

    fn tables(blocks: &[super::super::Block]) -> Vec<&Table> {
        blocks
            .iter()
            .filter_map(|b| match b {
                super::super::Block::Table(t) => Some(&**t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn short_rows_are_padded_and_small_tables_are_whole() {
        let header = row(3, "h");
        let rows = vec![row(1, "a"), row(3, "b")];
        let table = convert(&header, &[], &rows);
        assert!(!table.cut);
        assert_eq!(table.rows[0].len(), 3);
        assert_eq!(table.align, vec![Align::Left; 3]);
    }
}
