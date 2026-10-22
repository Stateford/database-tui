//! Main browsing screen: explorer sidebar plus Data / Structure / SQL tabs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, Wrap};

use super::grid::{self, GridView};
use super::theme::*;
use super::{panel, privilege_spans, spinner};
use crate::app::App;
use crate::app::browser::{Browser, Focus, Tab, TreeItem};
use crate::app::query::{EditorFocus, VimMode};
use crate::config::KeymapMode;
use crate::db::TableKind;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let mode = app.keymap_mode();
    let spin = spinner(app);
    let Some(b) = app.browser.as_mut() else { return };
    let sidebar_w = (area.width / 4).clamp(24, 40);
    let [side, main] = Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(20)]).areas(area);
    draw_sidebar(frame, b, side);

    let [tabs, content] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(main);
    draw_tabs(frame, b, tabs, spin);
    match b.tab {
        Tab::Data => draw_data(frame, b, content, spin),
        Tab::Structure => draw_structure(frame, b, content),
        Tab::Query => draw_query(frame, b, content, mode, spin),
    }
}

fn draw_sidebar(frame: &mut Frame, b: &mut Browser, area: Rect) {
    let focused = b.focus == Focus::Sidebar;
    let mut title = vec![Span::raw(" Explorer ")];
    if !b.tree_filter.is_empty() {
        title.push(Span::styled(format!("/{} ", b.tree_filter), Style::default().fg(YELLOW)));
    }
    let block = panel(Line::from(title), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items = b.tree_items();
    let height = inner.height as usize;
    if b.tree_selected < b.tree_offset {
        b.tree_offset = b.tree_selected;
    } else if height > 0 && b.tree_selected >= b.tree_offset + height {
        b.tree_offset = b.tree_selected + 1 - height;
    }
    let open = b.table.as_ref().map(|t| t.info.table.clone());
    let width = inner.width as usize;
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(b.tree_offset)
        .take(height)
        .map(|(idx, item)| {
            let selected = idx == b.tree_selected;
            let bg = match (selected, focused) {
                (true, true) => SELECTION,
                (true, false) => HIGHLIGHT,
                _ => BG,
            };
            let mut spans = match *item {
                TreeItem::Schema(s) => {
                    let schema = &b.schemas[s];
                    let expanded = !b.tree_filter.is_empty() || b.expanded.contains(&schema.name);
                    vec![
                        Span::styled(if expanded { " ▾ " } else { " ▸ " }, Style::default().fg(MUTED)),
                        Span::styled(schema.name.clone(), Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {}", schema.tables.len()), dim()),
                    ]
                }
                TreeItem::Table(s, t) => {
                    let info = &b.schemas[s].tables[t];
                    let (icon, color) = match info.kind {
                        TableKind::Table => ("▦", BLUE),
                        TableKind::View => ("◇", TEAL),
                        TableKind::MaterializedView => ("◆", TEAL),
                        TableKind::Foreign => ("◌", ORANGE),
                    };
                    let is_open = open.as_ref() == Some(&info.table);
                    let name_style = if is_open {
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FG)
                    };
                    let writable = b.effective(info.privileges).can_write();
                    let tag = if writable { "" } else { " ro" };
                    let max = width.saturating_sub(6 + tag.len());
                    let mut spans = vec![
                        Span::styled(format!("   {icon} "), Style::default().fg(color)),
                        Span::styled(crate::util::truncate(&info.table.name, max), name_style),
                    ];
                    if !writable {
                        spans.push(Span::styled(tag, Style::default().fg(YELLOW)));
                    }
                    spans
                }
            };
            for s in &mut spans {
                s.style = s.style.bg(bg);
            }
            let used: usize = spans.iter().map(|s| s.width()).sum();
            spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), Style::default().bg(bg)));
            Line::from(spans)
        })
        .collect();
    if items.is_empty() {
        let msg = if b.tree_filter.is_empty() { "No tables" } else { "No matching tables" };
        frame.render_widget(Paragraph::new(Line::styled(format!(" {msg}"), dim())), inner);
    } else {
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn draw_tabs(frame: &mut Frame, b: &Browser, area: Rect, spin: &str) {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, tab.label());
        if *tab == b.tab {
            spans.push(Span::styled(label, pill(BLUE)));
        } else {
            spans.push(Span::styled(label, Style::default().fg(FG_DIM).bg(SURFACE)));
        }
        spans.push(Span::raw(" "));
    }
    let tabs = Line::from(spans);
    let tabs_width = tabs.width() as u16;
    frame.render_widget(Paragraph::new(tabs), area);

    let mut right = Vec::new();
    if let Some(t) = &b.table {
        if t.loading {
            right.push(Span::styled(format!("{spin} loading "), Style::default().fg(CYAN)));
        }
        if let Some(f) = &t.filter {
            right.push(Span::styled(
                format!(" where {} ", crate::util::truncate(&crate::util::one_line(f), 40)),
                Style::default().fg(YELLOW),
            ));
        }
        if let Some((c, asc)) = &t.order_by {
            right.push(Span::styled(
                format!(" sort {c} {} ", if *asc { "▲" } else { "▼" }),
                Style::default().fg(MAGENTA),
            ));
        }
        right.push(Span::styled(format!(" {} ", t.info.table), Style::default().fg(CYAN)));
        right.push(Span::styled(format!("{} ", t.info.kind.label()), dim()));
    }
    // Drop leading parts until the info fits next to the tabs.
    let avail = area.width.saturating_sub(tabs_width + 1) as usize;
    while right.len() > 1 && right.iter().map(|s| s.width()).sum::<usize>() > avail {
        right.remove(0);
    }
    let rect = Rect { x: area.x + tabs_width + 1, width: avail as u16, ..area };
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), rect);
}

fn empty_state(frame: &mut Frame, area: Rect, lines: Vec<Line>) {
    let h = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(h) / 2;
    frame.render_widget(Paragraph::new(lines).centered(), Rect { y, height: h.min(area.height), ..area });
}

fn draw_data(frame: &mut Frame, b: &mut Browser, area: Rect, spin: &str) {
    let focused = b.focus == Focus::Main;
    let Some(t) = b.table.as_mut() else {
        let block = panel(" Data ", focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        empty_state(
            frame,
            inner,
            vec![
                Line::styled("Pick a table in the explorer", Style::default().fg(FG)),
                Line::from(vec![Span::styled("Enter", key()), Span::styled(" opens it here", dim())]),
            ],
        );
        return;
    };

    let total_label = match t.total {
        Some(n) => format!("{n}"),
        None if t.loading => spin.to_string(),
        None => "?".into(),
    };
    let shown = t.grid.rows.len();
    let first = t.first_row_number();
    let range = if shown == 0 { "0".to_string() } else { format!("{}–{}", first + 1, first + shown) };
    let pages = t.total.map(|n| (n as usize).div_ceil(t.page_size).max(1));
    let mut bottom = vec![Span::styled(format!(" rows {range} of {total_label} "), dim())];
    if let Some(p) = pages {
        bottom.push(Span::styled(format!("· page {}/{} ", t.page + 1, p), dim()));
    }
    if !t.can_write() {
        bottom.push(Span::styled("· 🔒 read-only ", Style::default().fg(YELLOW)));
    } else if !t.has_primary_key() && t.structure.is_some() {
        bottom.push(Span::styled("· no primary key: insert only ", Style::default().fg(ORANGE)));
    }

    let title = Line::from(vec![Span::raw(format!(" {} ", t.info.table.name))]);
    let block = panel(title, focused).title_bottom(Line::from(bottom));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner = match &t.error {
        Some(err) if t.loaded => {
            let [msg, rest] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
            frame.render_widget(
                Paragraph::new(Line::styled(format!(" ✖ {}", crate::util::one_line(err)), Style::default().fg(RED)))
                    .wrap(Wrap { trim: true }),
                msg,
            );
            rest
        }
        _ => inner,
    };
    if let Some(err) = &t.error
        && !t.loaded
    {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled("Could not load data", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                Line::from(""),
                Line::styled(err.clone(), Style::default().fg(FG)),
            ])
            .wrap(Wrap { trim: false }),
            inner.inner(Margin { horizontal: 1, vertical: 1 }),
        );
        return;
    }
    if !t.loaded {
        empty_state(frame, inner, vec![Line::styled(format!("{spin} Loading…"), Style::default().fg(CYAN))]);
        return;
    }
    let pks: Vec<String> = t
        .structure
        .as_ref()
        .map(|s| s.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    let sort = t.order_by.as_ref().map(|(c, asc)| (c.as_str(), *asc));
    let view = GridView { focused, first_row: first, primary_keys: &pks, sort };
    grid::render(frame.buffer_mut(), inner, &mut t.grid, &view);
}

fn draw_structure(frame: &mut Frame, b: &mut Browser, area: Rect) {
    let focused = b.focus == Focus::Main;
    let block = panel(" Structure ", focused);
    let inner = block.inner(area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, area);
    let Some(t) = b.table.as_ref() else {
        empty_state(frame, inner, vec![Line::styled("Open a table to see its columns and indexes", dim())]);
        return;
    };
    let Some(s) = &t.structure else {
        let msg = t.error.clone().unwrap_or_else(|| "Loading…".into());
        empty_state(frame, inner, vec![Line::styled(msg, dim())]);
        return;
    };

    let mut privs = vec![Span::styled("Your privileges  ", dim())];
    privs.extend(privilege_spans(t.privileges()));
    if let Some(reason) = &b.session.read_only_reason {
        privs.push(Span::styled(format!("  (session read-only: {reason})"), Style::default().fg(YELLOW)));
    }
    let header_style = Style::default().fg(FG_DIM).bg(SURFACE).add_modifier(Modifier::BOLD);
    let rows: Vec<Row> = s
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let key = if c.primary_key {
                Span::styled("PK", Style::default().fg(YELLOW).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("")
            };
            Row::new(vec![
                Cell::from(Span::styled(format!("{}", i + 1), dim())),
                Cell::from(Span::styled(c.name.clone(), Style::default().fg(FG).add_modifier(Modifier::BOLD))),
                Cell::from(Span::styled(c.data_type.clone(), Style::default().fg(CYAN))),
                Cell::from(if c.nullable {
                    Span::styled("yes", dim())
                } else {
                    Span::styled("NOT NULL", Style::default().fg(ORANGE))
                }),
                Cell::from(key),
                Cell::from(Span::styled(c.default.clone().unwrap_or_default(), Style::default().fg(GREEN))),
                Cell::from(Span::styled(c.extra.clone().unwrap_or_default(), dim())),
            ])
        })
        .collect();
    let index_rows: Vec<Row> = s
        .indexes
        .iter()
        .map(|ix| {
            let kind = if ix.primary {
                Span::styled("PRIMARY", Style::default().fg(YELLOW))
            } else if ix.unique {
                Span::styled("UNIQUE", Style::default().fg(MAGENTA))
            } else {
                Span::styled("INDEX", dim())
            };
            Row::new(vec![
                Cell::from(Span::styled(ix.name.clone(), Style::default().fg(FG))),
                Cell::from(kind),
                Cell::from(Span::styled(ix.definition.clone(), dim())),
            ])
        })
        .collect();

    let cols_h = (rows.len() as u16 + 1).min(inner.height.saturating_sub(4));
    let [priv_area, _, cols_area, _, idx_title, idx_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(cols_h.max(2)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(Line::from(privs)), priv_area);

    let skip = (t.structure_scroll as usize).min(rows.len().saturating_sub(1));
    let table = Table::new(
        rows.into_iter().skip(skip),
        [
            Constraint::Length(3),
            Constraint::Fill(2),
            Constraint::Fill(2),
            Constraint::Length(9),
            Constraint::Length(3),
            Constraint::Fill(2),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["#", "Column", "Type", "Nullable", "Key", "Default", "Extra"]).style(header_style))
    .column_spacing(2)
    .style(base());
    frame.render_widget(table, cols_area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("Indexes ({})", s.indexes.len()),
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        )])),
        idx_title,
    );
    let table = Table::new(index_rows, [Constraint::Fill(1), Constraint::Length(8), Constraint::Fill(3)])
        .header(Row::new(["Name", "Kind", "Definition"]).style(header_style))
        .column_spacing(2)
        .style(base());
    frame.render_widget(table, idx_area);
}

fn draw_query(frame: &mut Frame, b: &mut Browser, area: Rect, mode: KeymapMode, spin: &str) {
    let focused_main = b.focus == Focus::Main;
    let editor_focused = focused_main && b.query.focus == EditorFocus::Editor;
    let results_focused = focused_main && b.query.focus == EditorFocus::Results;
    let editor_h = (area.height * 2 / 5).max(5);
    let [editor_area, msg_area, results_area] =
        Layout::vertical([Constraint::Length(editor_h), Constraint::Length(1), Constraint::Min(3)]).areas(area);

    let mut title = vec![Span::raw(" SQL editor ")];
    if mode == KeymapMode::Vim && editor_focused {
        let (label, color) = match b.query.vim_mode {
            VimMode::Normal => ("NORMAL", BLUE),
            VimMode::Insert => ("INSERT", GREEN),
            VimMode::Visual => ("VISUAL", ORANGE),
        };
        title.push(Span::styled(format!(" {label} "), pill(color)));
        title.push(Span::raw(" "));
    }
    if b.fully_read_only() {
        title.push(Span::styled("🔒 read-only: only SELECT/SHOW/EXPLAIN ", Style::default().fg(YELLOW)));
    }
    let block = panel(Line::from(title), editor_focused)
        .title_bottom(Line::from(Span::styled(" F5 / Ctrl-E run · selection runs alone ", dim())));
    let cursor_style = if !editor_focused {
        Style::default()
    } else if mode == KeymapMode::Vim && b.query.vim_mode != VimMode::Insert {
        Style::default().fg(BG).bg(BLUE)
    } else {
        Style::default().fg(BG).bg(GREEN)
    };
    let ed = &mut b.query.editor;
    ed.set_block(block);
    ed.set_style(base());
    ed.set_cursor_style(cursor_style);
    ed.set_cursor_line_style(if editor_focused { Style::default().bg(BG_DARK) } else { Style::default() });
    ed.set_line_number_style(dim());
    ed.set_selection_style(Style::default().bg(SELECTION));
    ed.set_placeholder_style(dim());
    frame.render_widget(&*ed, editor_area);

    let msg = match (&b.query.message, b.query.running) {
        (_, true) => Line::styled(format!(" {spin} Running…"), Style::default().fg(CYAN)),
        (Some((m, true)), _) => Line::styled(format!(" {}", crate::util::one_line(m)), Style::default().fg(RED)),
        (Some((m, false)), _) => Line::styled(format!(" {m}"), Style::default().fg(GREEN)),
        (None, _) => Line::styled(" Results appear below", dim()),
    };
    frame.render_widget(Paragraph::new(msg), msg_area);

    let block = panel(" Results ", results_focused);
    let inner = block.inner(results_area);
    frame.render_widget(block, results_area);
    match &mut b.query.result {
        Some(grid) => {
            let view = GridView { focused: results_focused, first_row: 0, primary_keys: &[], sort: None };
            grid::render(frame.buffer_mut(), inner, grid, &view);
        }
        None => {
            frame.render_widget(Block::default().style(base()), inner);
        }
    }
}
