//! Connection manager screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::theme::*;
use super::{panel, spinner};
use crate::app::App;
use crate::config::CredentialStore;

const LOGO: [&str; 4] = ["╺┳┓┏┓ ╺┳╸╻ ╻╻", " ┃┃┣┻┓ ┃ ┃ ┃┃", "╺┻┛┗━┛ ╹ ┗━┛╹", ""];

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let area = area.inner(Margin { horizontal: 1, vertical: 0 });
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).spacing(1).areas(area);

    // Saved connections.
    let block = panel(" Saved connections ", true);
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);

    if app.config.connections.is_empty() {
        let mut lines: Vec<Line> = LOGO.iter().map(|l| Line::styled(*l, Style::default().fg(MAGENTA))).collect();
        lines.push(Line::styled("No saved connections yet.", Style::default().fg(FG)));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Press ", dim()),
            Span::styled("n", key()),
            Span::styled(" to add PostgreSQL, MySQL/MariaDB or SQLite.", dim()),
        ]));
        let h = lines.len() as u16;
        let y = inner.y + inner.height.saturating_sub(h) / 2;
        frame.render_widget(Paragraph::new(lines).centered(), Rect { y, height: h.min(inner.height), ..inner });
    } else {
        // Three lines per entry; scroll so the selection stays visible.
        let per_item = 3usize;
        let visible = (inner.height as usize / per_item).max(1);
        let first = app.selected_conn.saturating_sub(visible - 1);
        let width = inner.width as usize;
        let mut lines = Vec::new();
        for (i, p) in app.config.connections.iter().enumerate().skip(first).take(visible) {
            let selected = i == app.selected_conn;
            let bg = if selected { HIGHLIGHT } else { BG };
            let bar = Span::styled(if selected { "▌" } else { " " }, Style::default().fg(BLUE).bg(bg));
            let mut top = vec![
                bar.clone(),
                Span::styled(format!(" {} ", p.driver.short()), pill(driver_color(p.driver))),
                Span::styled(format!(" {}", p.name), Style::default().fg(FG).bg(bg).add_modifier(Modifier::BOLD)),
            ];
            if p.read_only {
                top.push(Span::styled("  read-only", Style::default().fg(YELLOW).bg(bg)));
            }
            top.push(match p.credential {
                CredentialStore::Keyring => Span::styled("  ● keyring", Style::default().fg(GREEN).bg(bg)),
                CredentialStore::Vault => Span::styled("  ● vault", Style::default().fg(TEAL).bg(bg)),
                CredentialStore::None if p.needs_password() => Span::styled("  ○ ask", dim().bg(bg)),
                CredentialStore::None => Span::raw(""),
            });
            let target = crate::util::truncate(&p.target(), width.saturating_sub(6));
            let mut bottom = vec![bar, Span::styled(format!("     {target}"), dim().bg(bg))];
            for spans in [&mut top, &mut bottom] {
                let used: usize = spans.iter().map(|s| s.width()).sum();
                spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), Style::default().bg(bg)));
            }
            lines.push(Line::from(top));
            lines.push(Line::from(bottom));
            lines.push(Line::from(""));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // Details of the selected connection.
    let block = panel(" Details ", false);
    let inner = block.inner(detail_area).inner(Margin { horizontal: 1, vertical: 0 });
    frame.render_widget(block, detail_area);
    let Some(p) = app.config.connections.get(app.selected_conn) else {
        let text = vec![
            Line::from(""),
            Line::styled("Credentials are stored securely:", Style::default().fg(FG)),
            Line::from(""),
            Line::from(vec![
                Span::styled(" ● ", Style::default().fg(GREEN)),
                Span::raw("OS keyring (Keychain, Credential Manager, Secret Service)"),
            ]),
            Line::from(vec![
                Span::styled(" ● ", Style::default().fg(TEAL)),
                Span::raw("or an encrypted vault (Argon2id + XChaCha20-Poly1305)"),
            ]),
            Line::from(""),
            Line::styled("Passwords are never written to the config file.", dim()),
        ];
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        return;
    };
    let row = |label: &str, value: String| {
        Line::from(vec![Span::styled(format!("{label:<11}"), dim()), Span::styled(value, Style::default().fg(FG))])
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {} ", p.driver.label()), pill(driver_color(p.driver))),
            Span::styled(format!("  {}", p.name), Style::default().fg(FG).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];
    if p.driver.is_file_based() {
        lines.push(row("File", p.path.clone()));
    } else {
        lines.push(row("Host", format!("{}:{}", p.host, p.port.map(|p| p.to_string()).unwrap_or_default())));
        lines.push(row("User", p.user.clone()));
        lines.push(row("Database", if p.database.is_empty() { "(default)".into() } else { p.database.clone() }));
        lines.push(row("SSL", p.ssl_mode.label().to_string()));
        if !p.ssl_ca.is_empty() {
            lines.push(row("CA file", p.ssl_ca.clone()));
        }
        lines.push(row("Password", p.credential.label().to_string()));
    }
    lines.push(row("Access", if p.read_only { "forced read-only".into() } else { "from database privileges".into() }));
    if let Some(warning) = p.tls_warning() {
        lines.push(Line::from(""));
        lines.push(Line::styled(format!("⚠ {}: {warning}", p.ssl_mode.label()), Style::default().fg(YELLOW)));
    }
    lines.push(Line::from(""));
    if app.busy.is_some() {
        lines.push(Line::styled(
            format!("{} {}…", spinner(app), app.busy.as_deref().unwrap_or("")),
            Style::default().fg(CYAN),
        ));
    } else {
        lines.push(Line::from(vec![Span::styled("Enter", key()), Span::styled(" to connect", dim())]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
