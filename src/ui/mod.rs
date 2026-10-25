//! Rendering. Everything here reads application state and draws it; the
//! only mutation is scroll bookkeeping that depends on the terminal size.

mod browser;
mod connections;
mod grid;
mod help;
mod popups;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::browser::{Focus, Tab};
use crate::app::query::{EditorFocus, VimMode};
use crate::app::{App, Level, Screen};
use crate::config::KeymapMode;
use crate::db::{AccessLevel, Privileges, TableRef, TransportTrust};
use crate::keymap::{self, Context};
use theme::*;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(base()), area);
    let [header, body, footer, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)])
            .areas(area);

    draw_header(frame, app, header);
    match app.screen {
        Screen::Connections => connections::draw(frame, app, body),
        Screen::Browser => browser::draw(frame, app, body),
    }
    draw_footer(frame, app, footer);
    draw_status(frame, app, status);
    popups::draw(frame, app, area);
}

pub fn spinner(app: &App) -> &'static str {
    SPINNER[app.tick % SPINNER.len()]
}

/// Most bytes of a message (an error from the server, say) that are drawn.
const MESSAGE_BYTES: usize = 64 * 1024;

/// Text from the database or a server, safe to draw on one line (see
/// [`crate::util::display_safe`]); a long one is clipped first.
pub fn safe_line(s: &str) -> String {
    crate::util::display_safe(crate::util::clip(s, MESSAGE_BYTES).0)
}

/// As [`safe_line`], keeping line breaks.
pub fn safe_text(s: &str) -> String {
    match crate::util::clip(s, MESSAGE_BYTES) {
        (all, false) => crate::util::display_safe_lines(all),
        (start, true) => format!("{}…", crate::util::display_safe_lines(start)),
    }
}

/// A table's `schema.name` as shown, behind a `⚠` when either part holds
/// hidden characters (see [`crate::util::ident`]).
pub fn table_label(table: &TableRef) -> String {
    use crate::util::{display_safe, has_hidden};
    let flag = if has_hidden(&table.schema) || has_hidden(&table.name) { "⚠ " } else { "" };
    format!("{flag}{}.{}", display_safe(&table.schema), display_safe(&table.name))
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG_DARK)), area);
    let mut left = vec![Span::styled(" ◆ dbtui ", pill(MAGENTA)), Span::raw(" ")];
    match (&app.browser, app.screen) {
        (Some(b), Screen::Browser) => {
            left.push(Span::styled(format!(" {} ", b.profile.driver.short()), pill(driver_color(b.profile.driver))));
            left.push(Span::styled(
                format!(" {} ", safe_line(&b.profile.name)),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ));
            left.push(Span::styled(format!("{} ", safe_line(&b.profile.target())), dim()));
            left.push(Span::styled(format!("· {}", safe_line(&b.session.server_version)), dim()));
        }
        _ => left.push(Span::styled("Connections", Style::default().fg(FG).add_modifier(Modifier::BOLD))),
    }
    let mut right = Vec::new();
    if let Some(reason) =
        app.browser.as_ref().filter(|_| app.screen == Screen::Browser).and_then(|b| b.session.read_only_reason.as_ref())
    {
        right.push(Span::styled(format!("⚠ read-only: {} ", safe_line(reason)), Style::default().fg(YELLOW)));
    }
    let keys = match app.keymap_mode() {
        KeymapMode::Vim => Span::styled(" VIM ", pill(GREEN)),
        KeymapMode::Standard => Span::styled(" STD ", pill(BLUE)),
    };
    right.push(Span::styled("keys ", dim()));
    right.push(keys);
    let right = Line::from(right);
    // The left part yields to the right part on narrow terminals.
    let left_width = area.width.saturating_sub(right.width() as u16 + 2);
    frame.render_widget(Paragraph::new(Line::from(left)), Rect { width: left_width, ..area });
    frame.render_widget(Paragraph::new(right).right_aligned(), area);
}

fn current_context(app: &App) -> Option<Context> {
    match (app.screen, &app.browser) {
        (Screen::Connections, _) => Some(Context::Connections),
        (Screen::Browser, Some(b)) => Some(match (b.focus, b.tab) {
            (Focus::Sidebar, _) => Context::Sidebar,
            (Focus::Main, Tab::Structure) => Context::Structure,
            (Focus::Main, Tab::Query) if b.query.focus == EditorFocus::Editor => return None,
            (Focus::Main, _) => Context::Grid,
        }),
        _ => None,
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(cmd) = &app.command {
        let line = Line::from(vec![
            Span::styled(":", Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD)),
            Span::raw(cmd.value().to_string()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        frame.set_cursor_position((area.x + 1 + cmd.cursor() as u16, area.y));
        return;
    }
    if let Some(toast) = &app.toast {
        let (color, icon) = match toast.level {
            Level::Info => (BLUE, "ℹ"),
            Level::Success => (GREEN, "✔"),
            Level::Warning => (YELLOW, "⚠"),
            Level::Error => (RED, "✖"),
        };
        let line = Line::from(vec![
            Span::styled(format!(" {icon} "), pill(color)),
            Span::styled(format!(" {}", safe_line(&toast.text)), Style::default().fg(color)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let mode = app.keymap_mode();
    let mut spans = Vec::new();
    if let Some(ctx) = current_context(app) {
        let privileges = app
            .browser
            .as_ref()
            .filter(|b| b.tab == Tab::Data)
            .and_then(|b| b.table.as_ref())
            .map(|t| {
                let mut p = t.privileges();
                // Without a primary key existing rows cannot be targeted.
                if t.structure.is_some() && !t.has_primary_key() {
                    p.update = false;
                    p.delete = false;
                }
                p
            })
            .unwrap_or(Privileges::READ);
        for hint in keymap::hints(mode, ctx, privileges) {
            spans.push(Span::styled(format!(" {}", hint.keys), key()));
            spans.push(Span::styled(format!(" {} ", hint.label), dim()));
        }
    } else if let Some(b) = &app.browser {
        // SQL editor hints.
        let items: &[(&str, &str)] = match (mode, b.query.vim_mode) {
            (KeymapMode::Standard, _) => &[("F5/^E", "run"), ("Tab", "results"), ("Esc", "leave"), ("^K", "vim keys")],
            (KeymapMode::Vim, VimMode::Insert) => &[("Esc", "normal mode"), ("F5/^E", "run")],
            (KeymapMode::Vim, _) => {
                &[("i", "insert"), ("v", "select"), ("F5/^E", "run"), ("Tab", "results"), (":", "command")]
            }
        };
        for (k, l) in items {
            spans.push(Span::styled(format!(" {k}"), key()));
            spans.push(Span::styled(format!(" {l} "), dim()));
        }
    }
    if let Some(p) = app.keymap.pending() {
        spans.insert(0, Span::styled(format!(" {p}… "), pill(YELLOW)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn privilege_spans(p: Privileges) -> Vec<Span<'static>> {
    [("SEL", p.select), ("INS", p.insert), ("UPD", p.update), ("DEL", p.delete)]
        .into_iter()
        .map(|(label, on)| {
            let style = if on {
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED).add_modifier(Modifier::CROSSED_OUT)
            };
            Span::styled(format!("{label} "), style)
        })
        .collect()
}

fn mode_pill(app: &App) -> Span<'static> {
    if app.command.is_some() {
        return Span::styled(" COMMAND ", pill(MAGENTA));
    }
    match app.keymap_mode() {
        KeymapMode::Standard => Span::styled(" STANDARD ", pill(BLUE)),
        KeymapMode::Vim => {
            let in_editor = app
                .browser
                .as_ref()
                .is_some_and(|b| b.focus == Focus::Main && b.tab == Tab::Query && b.query.focus == EditorFocus::Editor);
            match app.browser.as_ref().map(|b| b.query.vim_mode) {
                Some(VimMode::Insert) if in_editor => Span::styled(" INSERT ", pill(GREEN)),
                Some(VimMode::Visual) if in_editor => Span::styled(" VISUAL ", pill(ORANGE)),
                _ => Span::styled(" NORMAL ", pill(BLUE)),
            }
        }
    }
}

/// A part of the status bar. When the bar is too narrow, parts go in order
/// of `rank`, highest first; one with a `short` form is shortened first.
struct Part {
    spans: Vec<Span<'static>>,
    rank: u8,
    /// The rank and spans of the shorter form.
    short: Option<(u8, Vec<Span<'static>>)>,
    /// Drawn right-aligned, after the others.
    right: bool,
}

impl Part {
    fn new(rank: u8, spans: Vec<Span<'static>>) -> Part {
        Part { spans, rank, short: None, right: false }
    }

    fn right(rank: u8, spans: Vec<Span<'static>>) -> Part {
        Part { right: true, ..Part::new(rank, spans) }
    }

    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

/// Shortens or drops parts, least important first, until they fit `width`.
fn fit_parts(parts: &mut Vec<Part>, width: usize) {
    while parts.iter().map(Part::width).sum::<usize>() > width {
        let Some(i) = (0..parts.len()).max_by_key(|&i| (parts[i].rank, i)) else { return };
        match parts[i].short.take() {
            Some((rank, spans)) => (parts[i].rank, parts[i].spans) = (rank, spans),
            None => drop(parts.remove(i)),
        }
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG_DARK)), area);
    // What matters most when the bar is narrow: the access level, whether
    // work is running (a commit, say), whether the server is authenticated,
    // and how many changes are staged.
    let mut parts = vec![Part::new(4, vec![mode_pill(app), Span::raw(" ")])];
    if let Some(busy) = &app.busy {
        let text = format!("{} {}… ", spinner(app), safe_line(busy));
        parts.push(Part::right(1, vec![Span::styled(text, Style::default().fg(CYAN))]));
    }

    if let (Some(b), Screen::Browser) = (&app.browser, app.screen) {
        let level = b.access_level();
        let color = match level {
            AccessLevel::ReadOnly => YELLOW,
            AccessLevel::ReadWrite => GREEN,
            AccessLevel::Superuser => RED,
        };
        let icon = if level == AccessLevel::ReadOnly { "🔒" } else { "●" };
        parts.push(Part::new(1, vec![Span::styled(format!(" {icon} {} ", level.label()), pill(color))]));
        parts.push(Part::new(
            7,
            vec![Span::styled(format!(" {} ", safe_line(&b.session.user)), Style::default().fg(FG))],
        ));
        if let Some((label, trust)) = b.session.transport.summary(&b.profile) {
            // The TLS version is as the server reports it.
            let label = safe_line(&label);
            let warn = |color| Span::styled(format!(" ⚠ {label} "), pill(color));
            let mut part = match trust {
                TransportTrust::Local => Part::new(7, vec![Span::styled(format!("{label} "), dim())]),
                TransportTrust::Verified => {
                    let style = Style::default().fg(GREEN);
                    let mut part = Part::new(5, vec![Span::styled(format!("✓ {label} "), style)]);
                    part.short = Some((5, vec![Span::styled("✓ TLS ", style)]));
                    part
                }
                TransportTrust::Unverified => {
                    let mut part = Part::new(5, vec![warn(YELLOW)]);
                    part.short = Some((2, vec![Span::styled(" ⚠ unverified ", pill(YELLOW))]));
                    part
                }
                TransportTrust::Exposed => Part::new(2, vec![warn(RED)]),
            };
            part.spans.push(Span::raw(" "));
            if let Some((_, short)) = &mut part.short {
                short.push(Span::raw(" "));
            }
            parts.push(part);
        }
        if let Some(t) = &b.table {
            parts.push(Part::new(
                6,
                vec![
                    Span::styled("│ ", Style::default().fg(BORDER)),
                    Span::styled(format!("{} ", crate::util::ident(&t.info.table.name)), Style::default().fg(CYAN)),
                ],
            ));
            parts.push(Part::new(8, privilege_spans(t.privileges())));
            let pending = t.grid.pending_count();
            if pending > 0 {
                parts.push(Part::new(2, vec![Span::styled(format!(" ✎ {pending} pending "), pill(ORANGE))]));
            }
        }
        let grid = match b.tab {
            Tab::Data => b.table.as_ref().map(|t| (&t.grid, t.first_row_number())),
            Tab::Query => b.query.result.as_ref().map(|g| (g, 0)),
            Tab::Structure => None,
        };
        if let Some((g, first)) = grid
            && !g.is_empty()
        {
            let position =
                format!("row {}/{}  col {}/{} ", first + g.row + 1, first + g.total_rows(), g.col + 1, g.columns.len());
            parts.push(Part::right(9, vec![Span::styled(position, dim())]));
        }
    } else {
        let n = app.config.connections.len();
        let count = format!(" {n} saved connection{} ", if n == 1 { "" } else { "s" });
        parts.push(Part::new(6, vec![Span::styled(count, Style::default().fg(FG_DIM))]));
        if app.read_only_run {
            parts.push(Part::new(1, vec![Span::styled(" 🔒 READ-ONLY RUN ", pill(YELLOW)), Span::raw(" ")]));
        }
        let vault = if app.vault.is_some() { " · vault unlocked" } else { "" };
        let backend = format!("passwords: {:?}{vault} ", app.config.secret_backend).to_lowercase();
        parts.push(Part::right(9, vec![Span::styled(backend, dim())]));
    }
    fit_parts(&mut parts, area.width as usize);
    let (right, left): (Vec<Part>, Vec<Part>) = parts.into_iter().partition(|p| p.right);
    let left = Line::from(left.into_iter().flat_map(|p| p.spans).collect::<Vec<_>>());
    let left_width = (left.width() as u16).min(area.width);
    frame.render_widget(Paragraph::new(left), area);
    let right_area = Rect { x: area.x + left_width, width: area.width - left_width, ..area };
    let right = Line::from(right.into_iter().flat_map(|p| p.spans).collect::<Vec<_>>());
    frame.render_widget(Paragraph::new(right).right_aligned(), right_area);
}

/// Rounded panel with a title that lights up when focused.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border(focused))
        .title(title.into().style(theme::title(focused)))
        .style(base())
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Clears an area and draws a modal frame; returns the inner area. The
/// title may hold names from the database: it is made safe to draw.
pub fn modal(frame: &mut Frame, area: Rect, title: &str, color: ratatui::style::Color) -> Rect {
    frame.render_widget(Clear, area);
    let title = crate::util::fit(title, area.width.saturating_sub(4) as usize);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Line::from(vec![Span::styled(format!(" {title} "), pill(color))]))
        .style(Style::default().fg(FG).bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}
