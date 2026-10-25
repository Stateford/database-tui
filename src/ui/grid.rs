//! Spreadsheet-like rendering of a [`Grid`] with horizontal scrolling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::theme::*;
use crate::app::grid::{CellState, CellValue, Grid};
use crate::util::{fit, fit_cut, has_hidden, ident};

pub struct GridView<'a> {
    pub focused: bool,
    /// Row number of the first row (for paging).
    pub first_row: usize,
    pub primary_keys: &'a [String],
    pub sort: Option<(&'a str, bool)>,
}

pub fn render(buf: &mut Buffer, area: Rect, grid: &mut Grid, view: &GridView) {
    if area.height < 2 || area.width < 8 {
        return;
    }
    let total = grid.total_rows();
    let gutter = ((view.first_row + total).max(1).to_string().len() + 2).max(4) as u16;
    let body_height = (area.height - 1) as usize;
    grid.viewport_rows = body_height;

    // Keep the cursor visible vertically.
    if grid.row < grid.row_offset {
        grid.row_offset = grid.row;
    } else if grid.row >= grid.row_offset + body_height {
        grid.row_offset = grid.row + 1 - body_height;
    }
    grid.row_offset = grid.row_offset.min(total.saturating_sub(1));

    // ... and horizontally.
    let avail = area.width.saturating_sub(gutter) as usize;
    let col_w = |i: usize| grid.widths.get(i).copied().unwrap_or(8) + 3;
    if grid.col < grid.col_offset {
        grid.col_offset = grid.col;
    }
    loop {
        let used: usize = (grid.col_offset..=grid.col.min(grid.columns.len().saturating_sub(1))).map(col_w).sum();
        if used <= avail || grid.col_offset >= grid.col {
            break;
        }
        grid.col_offset += 1;
    }

    let header_style = Style::default().fg(FG).bg(SURFACE).add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(BORDER);
    buf.set_style(Rect { height: 1, ..area }, header_style);
    buf.set_stringn(
        area.x,
        area.y,
        format!("{:>w$} ", "#", w = gutter as usize - 1),
        gutter as usize,
        header_style.fg(MUTED),
    );

    // Header.
    let mut x = area.x + gutter;
    let right = area.x + area.width;
    let mut visible = Vec::new();
    for c in grid.col_offset..grid.columns.len() {
        if x >= right {
            break;
        }
        let w = grid.widths[c];
        visible.push((c, x, w));
        let name = &grid.columns[c];
        let is_pk = view.primary_keys.iter().any(|k| k == name);
        let arrow = match view.sort {
            Some((s, asc)) if s == name => {
                if asc {
                    " ▲"
                } else {
                    " ▼"
                }
            }
            _ => "",
        };
        let label = format!("{}{}{arrow}", if is_pk { "◆ " } else { "" }, ident(name));
        let mut style = header_style;
        if is_pk {
            style = style.fg(YELLOW);
        }
        if has_hidden(name) {
            style = style.fg(ORANGE);
        }
        if c == grid.col && view.focused {
            style = style.fg(CYAN);
        }
        let space = (right - x) as usize;
        buf.set_stringn(x, area.y, format!(" {:<w$} ", fit(&label, w), w = w), space, style);
        let sx = x + w as u16 + 2;
        if sx < right {
            buf.set_stringn(sx, area.y, "│", 1, header_style.fg(BORDER));
        }
        x = x.saturating_add(w as u16 + 3);
    }

    // Body.
    for (i, row) in (grid.row_offset..total).take(body_height).enumerate() {
        let y = area.y + 1 + i as u16;
        let selected_row = row == grid.row;
        let zebra = if row % 2 == 1 { BG } else { BG_DARK };
        let row_bg = if selected_row { HIGHLIGHT } else { zebra };
        buf.set_style(Rect { x: area.x, y, width: area.width, height: 1 }, Style::default().bg(row_bg));

        let marker = if grid.is_inserted(row) {
            Style::default().fg(GREEN)
        } else if grid.edits.deleted.contains(&row) {
            Style::default().fg(RED)
        } else if selected_row {
            Style::default().fg(BLUE)
        } else {
            Style::default().fg(MUTED)
        };
        let label = if grid.is_inserted(row) { "+".to_string() } else { (view.first_row + row + 1).to_string() };
        buf.set_stringn(
            area.x,
            y,
            format!("{:>w$} ", label, w = gutter as usize - 1),
            gutter as usize,
            marker.bg(row_bg),
        );

        for &(c, x, w) in &visible {
            let (value, state) = grid.cell(row, c);
            // Only as much of the value as the column shows is read: values
            // can be large. One cut short when loaded always ends in `…`.
            let (shown, mut style) = match value {
                CellValue::Value(v) => (fit_cut(v, w, grid.cut(row, c).is_some()), Style::default().fg(FG)),
                CellValue::Null => (fit("NULL", w), null()),
                CellValue::Default => (fit("DEFAULT", w), null()),
            };
            let cell_bg = match state {
                CellState::Clean => row_bg,
                CellState::Edited => EDITED_BG,
                CellState::Deleted => DELETED_BG,
                CellState::Inserted => INSERTED_BG,
            };
            style = style.bg(cell_bg);
            match state {
                CellState::Edited => style = style.fg(YELLOW),
                CellState::Deleted => style = style.fg(RED).add_modifier(Modifier::CROSSED_OUT),
                _ => {}
            }
            if selected_row && c == grid.col {
                style = if view.focused { style.bg(SELECTION).add_modifier(Modifier::BOLD) } else { style.bg(BORDER) };
            }
            let numeric = matches!(value, CellValue::Value(v) if looks_numeric(v));
            let padded = if numeric { format!(" {shown:>w$} ") } else { format!(" {shown:<w$} ") };
            let space = (right - x) as usize;
            buf.set_stringn(x, y, padded, space, style);
            let sx = x + w as u16 + 2;
            if sx < right {
                buf.set_stringn(sx, y, "│", 1, sep_style.bg(row_bg));
            }
        }
    }

    if total == 0 {
        let msg = "No rows";
        if area.height > 2 {
            buf.set_stringn(area.x + gutter + 1, area.y + 2, msg, area.width as usize, dim());
        }
    }
}

fn looks_numeric(v: &str) -> bool {
    !v.is_empty() && v.len() < 40 && v.parse::<f64>().is_ok() && !v.starts_with('+')
}
