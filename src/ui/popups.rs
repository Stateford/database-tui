//! Modal dialogs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::theme::*;
use super::{centered, help, modal, safe_line, safe_text};
use crate::app::form::{ConnectionForm, Field};
use crate::app::input::Input;
use crate::app::{App, Popup, commit_needs_force};
use crate::config::SecretBackend;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let mode = app.keymap_mode();
    let backend = app.config.secret_backend;
    let Some(popup) = app.popup.as_mut() else { return };
    match popup {
        Popup::Help { scroll } => {
            let r = centered(area, 86, area.height.saturating_sub(4));
            let inner =
                modal(frame, r, &format!("Keys · {}", mode.label()), BLUE).inner(Margin { horizontal: 2, vertical: 1 });
            let lines = help::lines(mode);
            *scroll = (*scroll).min(lines.len().saturating_sub(inner.height as usize) as u16);
            frame.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), inner);
        }
        Popup::Message { title, body, error, scroll } => {
            let color = if *error { RED } else { BLUE };
            let r = centered(area, 80, 14);
            let inner = modal(frame, r, title, color).inner(Margin { horizontal: 2, vertical: 1 });
            let [text, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            frame.render_widget(Paragraph::new(safe_text(body)).wrap(Wrap { trim: false }).scroll((*scroll, 0)), text);
            frame.render_widget(footer(&[("Enter", "close"), ("↑↓", "scroll")]), foot);
        }
        Popup::Confirm { title, message, action } => {
            // Tall enough for the message, wrapped between words: a line
            // holds 50 characters or more.
            let message = safe_text(message);
            let lines: usize = message.lines().map(|l| l.chars().count().max(1).div_ceil(50)).sum();
            let r = centered(area, 72, (lines as u16).saturating_add(5).max(8));
            let inner = modal(frame, r, title, YELLOW).inner(Margin { horizontal: 2, vertical: 1 });
            let [text, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), text);
            let yes = if action.enter_confirms() { "y / Enter" } else { "y" };
            frame.render_widget(footer(&[(yes, "yes"), ("n / Esc", "no")]), foot);
        }
        Popup::Commit { statements, previews, hidden, scroll, non_transactional, .. } => {
            let force = commit_needs_force(statements, non_transactional.as_ref());
            let mut head = match non_transactional {
                None => vec![Line::styled("These statements run in a single transaction:", dim())],
                Some(engine) => vec![
                    Line::styled(
                        format!("⚠ {} tables do not support transactions.", safe_line(engine)),
                        Style::default().fg(RED).add_modifier(Modifier::BOLD),
                    ),
                    Line::styled(
                        "If a statement fails, the ones before it stay applied and cannot be rolled back.",
                        Style::default().fg(YELLOW),
                    ),
                ],
            };
            if *hidden {
                head.push(Line::styled(
                    "⚠ Names or values hold characters that would not show: they are shown as ␛, ⟨U+200B⟩…",
                    Style::default().fg(ORANGE),
                ));
            }
            if force {
                head.push(Line::styled(
                    "Press ! to commit anyway, or Esc to commit fewer changes at a time.",
                    Style::default().fg(YELLOW),
                ));
            }
            let head_height = head.len() as u16 + 1;
            // Each statement wraps over several lines, plus a blank separator.
            let text_width = (area.width.min(100).saturating_sub(8)).max(10) as usize;
            let body: usize = previews.iter().map(|p| (p.chars().count() + 2).div_ceil(text_width) + 1).sum();
            let r = centered(area, 100, (body as u16 + head_height + 5).min(area.height.saturating_sub(4)));
            let inner = modal(frame, r, &format!("Commit {} change(s)", statements.len()), ORANGE)
                .inner(Margin { horizontal: 2, vertical: 1 });
            let [head_area, text, foot] =
                Layout::vertical([Constraint::Length(head_height), Constraint::Min(1), Constraint::Length(1)])
                    .areas(inner);
            frame.render_widget(Paragraph::new(head).wrap(Wrap { trim: false }), head_area);
            let lines: Vec<Line> = statements
                .iter()
                .zip(previews.iter())
                .flat_map(|(s, preview)| {
                    let color = if s.sql.starts_with("DELETE") {
                        RED
                    } else if s.sql.starts_with("INSERT") {
                        GREEN
                    } else {
                        YELLOW
                    };
                    vec![
                        Line::from(vec![Span::styled("› ", Style::default().fg(color)), Span::raw(preview.as_str())]),
                        Line::from(""),
                    ]
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((*scroll, 0)), text);
            let keys: &[(&str, &str)] = if force {
                &[("!", "commit anyway"), ("Esc", "back to editing")]
            } else {
                &[("y / Enter", "commit"), ("Esc", "back to editing")]
            };
            frame.render_widget(footer(keys), foot);
        }
        Popup::CellView { title, text, value, cut, scroll, wrapped, .. } => {
            let r = centered(area, 90, area.height.saturating_sub(6));
            let inner = modal(frame, r, title, CYAN).inner(Margin { horizontal: 2, vertical: 1 });
            let notice = if cut.is_some() { 1 } else { 0 };
            let [notice_area, body, foot] =
                Layout::vertical([Constraint::Length(notice), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            if let (Some(full), Some(v)) = (*cut, value.as_ref()) {
                let notice = format!(
                    "⚠ Too large to load in full: only the first {} of {} are shown",
                    crate::util::size(v.len() as u64),
                    crate::util::size(full)
                );
                frame.render_widget(Paragraph::new(Line::styled(notice, Style::default().fg(ORANGE))), notice_area);
            }
            // Wrapped once per width: wrapping a large value on every frame
            // would freeze the screen. Only the lines in view are drawn.
            let width = body.width as usize;
            if wrapped.as_ref().is_none_or(|(w, _)| *w != width) {
                *wrapped = Some((width, crate::util::wrap(text, width)));
            }
            let lines = wrapped.as_ref().map(|(_, lines)| lines.as_slice()).unwrap_or_default();
            // A cut value ends with a line that says so.
            let marker = cut.map(|_| Line::styled("… (not loaded)", Style::default().fg(ORANGE)));
            let total = lines.len() + usize::from(marker.is_some());
            *scroll = (*scroll).min(total.saturating_sub(body.height as usize));
            let end = (*scroll + body.height as usize).min(total);
            let mut shown: Vec<Line> =
                lines[(*scroll).min(lines.len())..end.min(lines.len())].iter().map(|l| Line::raw(l.as_str())).collect();
            if let Some(marker) = marker.filter(|_| end > lines.len()) {
                shown.push(marker);
            }
            // A stored text 'NULL' is not shown as SQL NULL.
            let style = if value.is_none() { null() } else { Style::default().fg(FG) };
            frame.render_widget(Paragraph::new(shown).style(style), body);
            frame.render_widget(
                footer(&[("Esc", "close"), ("y", "copy"), ("↑↓ PgUp PgDn", "scroll"), ("Home End", "top / end")]),
                foot,
            );
        }
        Popup::Prompt(p) => {
            let r = centered(area, 72, 7);
            let inner = modal(frame, r, &p.title, BLUE).inner(Margin { horizontal: 2, vertical: 1 });
            let [field, hint, _] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
            draw_input(frame, field, &p.input, true, false);
            frame.render_widget(Paragraph::new(Line::styled(safe_line(&p.hint), dim())), hint);
        }
        Popup::Password(p) => {
            // Room for the error (such as why the saved password is not used):
            // wrapped between long words, a line holds 40 characters or more.
            let error = p.error.as_deref().map(safe_line);
            let error_lines = error.as_ref().map_or(1, |e| e.chars().count().div_ceil(40).clamp(1, 6)) as u16;
            let r = centered(area, 64, 9 + error_lines);
            let inner = modal(frame, r, "Password", BLUE).inner(Margin { horizontal: 2, vertical: 1 });
            let [who, field, remember, err, foot] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(inner);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(safe_line(&p.profile.name), Style::default().fg(FG).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {}", safe_line(&p.profile.target())), dim()),
                ])),
                who,
            );
            draw_input(frame, field, &p.input, !p.focus_remember, true);
            let check = if p.remember { "[✓]" } else { "[ ]" };
            let style = if p.focus_remember {
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG_DIM)
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{check} Remember securely"), style),
                    Span::styled(format!("  ({})", backend_label(backend)), dim()),
                ])),
                remember,
            );
            if let Some(e) = error {
                frame.render_widget(
                    Paragraph::new(Line::styled(e, Style::default().fg(RED))).wrap(Wrap { trim: true }),
                    err,
                );
            }
            frame.render_widget(footer(&[("Enter", "connect"), ("Tab", "toggle remember"), ("Esc", "cancel")]), foot);
        }
        Popup::Vault(v) => {
            let r = centered(area, 70, if v.create { 12 } else { 10 });
            let title = if v.create { "Create password vault" } else { "Unlock password vault" };
            let inner = modal(frame, r, title, TEAL).inner(Margin { horizontal: 2, vertical: 1 });
            let [info, a, b, err, foot] = Layout::vertical([
                Constraint::Length(if v.create { 3 } else { 2 }),
                Constraint::Length(1),
                Constraint::Length(if v.create { 1 } else { 0 }),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(inner);
            let text = if v.create {
                "No OS keyring is available, so saved passwords go to an encrypted vault. Choose a master password (8+ characters); it is never stored."
            } else {
                "Enter the master password of your encrypted password vault."
            };
            frame.render_widget(
                Paragraph::new(Line::styled(text, Style::default().fg(FG_DIM))).wrap(Wrap { trim: true }),
                info,
            );
            draw_labeled_input(frame, a, "Master password", &v.input, v.focus == 0, true);
            if v.create {
                draw_labeled_input(frame, b, "Confirm", &v.confirm, v.focus == 1, true);
            }
            if v.working.is_some() {
                let text = if v.create { "Creating the vault…" } else { "Checking the master password…" };
                frame.render_widget(Paragraph::new(Line::styled(text, Style::default().fg(CYAN))), err);
                frame.render_widget(footer(&[("Esc", "cancel")]), foot);
            } else {
                if let Some(e) = &v.error {
                    frame.render_widget(
                        Paragraph::new(Line::styled(safe_line(e), Style::default().fg(RED))).wrap(Wrap { trim: true }),
                        err,
                    );
                }
                frame.render_widget(footer(&[("Enter", "ok"), ("Esc", "cancel")]), foot);
            }
        }
        Popup::CellEditor(c) => {
            let r = centered(area, 80, 14);
            let title = format!("Edit {}", c.column);
            let inner = modal(frame, r, &title, YELLOW).inner(Margin { horizontal: 1, vertical: 0 });
            let [meta, editor, foot] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(inner);
            let mut info = vec![Span::styled(safe_line(&c.data_type), Style::default().fg(CYAN))];
            if !c.nullable {
                info.push(Span::styled("  NOT NULL", Style::default().fg(ORANGE)));
            }
            if c.new_row {
                info.push(Span::styled("  new row", Style::default().fg(GREEN)));
            }
            if c.hidden {
                info.push(Span::styled(
                    "  ⚠ holds characters not shown here: see the viewer",
                    Style::default().fg(ORANGE),
                ));
            }
            frame.render_widget(Paragraph::new(Line::from(info)), meta);
            c.editor.set_block(
                Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER))
                    .style(base()),
            );
            c.editor.set_style(base());
            c.editor.set_cursor_style(Style::default().fg(BG).bg(YELLOW));
            c.editor.set_cursor_line_style(Style::default());
            frame.render_widget(&c.editor, editor);
            let mut keys = vec![("Enter", "stage change"), ("Alt-Enter", "newline")];
            if c.nullable {
                keys.push(("Ctrl-N", "NULL"));
            }
            if c.new_row {
                keys.push(("Ctrl-D", "default"));
            }
            keys.push(("Esc", "cancel"));
            frame.render_widget(footer(&keys), foot);
        }
        Popup::Form(form) => draw_form(frame, area, form, backend),
    }
}

fn backend_label(backend: SecretBackend) -> &'static str {
    match backend {
        SecretBackend::Auto => "OS keyring, or encrypted vault",
        SecretBackend::Keyring => "OS keyring",
        SecretBackend::Vault => "encrypted vault",
    }
}

fn footer<'a>(keys: &[(&'a str, &'a str)]) -> Paragraph<'a> {
    let mut spans = Vec::new();
    for (k, l) in keys {
        spans.push(Span::styled(*k, key()));
        spans.push(Span::styled(format!(" {l}   "), dim()));
    }
    Paragraph::new(Line::from(spans))
}

/// Draws a text input; masks it for secrets and places the terminal cursor.
fn draw_input(frame: &mut Frame, area: Rect, input: &Input, focused: bool, secret: bool) {
    let text: String = if secret { "•".repeat(input.value().chars().count()) } else { input.value().to_string() };
    let style = if focused { Style::default().fg(FG).bg(BG) } else { Style::default().fg(FG_DIM).bg(BG_DARK) };
    let width = area.width as usize;
    // Scroll horizontally so the cursor stays visible.
    let cursor = input.cursor();
    let start = cursor.saturating_sub(width.saturating_sub(2));
    let visible: String = text.chars().skip(start).take(width.saturating_sub(1)).collect();
    frame.render_widget(Paragraph::new(format!(" {visible}")).style(style), area);
    if focused {
        frame.set_cursor_position((area.x + 1 + (cursor - start) as u16, area.y));
    }
}

fn draw_labeled_input(frame: &mut Frame, area: Rect, label: &str, input: &Input, focused: bool, secret: bool) {
    let [l, f] = Layout::horizontal([Constraint::Length(17), Constraint::Min(5)]).areas(area);
    let style = if focused { Style::default().fg(CYAN).add_modifier(Modifier::BOLD) } else { dim() };
    frame.render_widget(Paragraph::new(Line::styled(format!("{label:>15} "), style)), l);
    draw_input(frame, f, input, focused, secret);
}

fn draw_form(frame: &mut Frame, area: Rect, form: &ConnectionForm, backend: SecretBackend) {
    let fields = form.fields();
    // One spacer row under each field, unless the terminal is too short.
    let step = if area.height >= fields.len() as u16 * 2 + 9 { 2 } else { 1 };
    let height = fields.len() as u16 * step as u16 + 7;
    let r = centered(area, 76, height);
    let title = if form.is_new { "New connection" } else { "Edit connection" };
    let inner = modal(frame, r, title, MAGENTA).inner(Margin { horizontal: 2, vertical: 1 });
    let mut constraints: Vec<Constraint> = fields.iter().flat_map(|_| vec![Constraint::Length(1); step]).collect();
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(inner);
    let focused = form.focused();

    for (i, field) in fields.iter().enumerate() {
        let area = rows[i * step];
        let is_focused = *field == focused;
        let [label_area, value_area] = Layout::horizontal([Constraint::Length(17), Constraint::Min(10)]).areas(area);
        let label_style = if is_focused { Style::default().fg(CYAN).add_modifier(Modifier::BOLD) } else { dim() };
        let marker = if is_focused { "› " } else { "  " };
        frame.render_widget(
            Paragraph::new(Line::styled(format!("{marker}{:>13} ", field.label()), label_style)),
            label_area,
        );

        let selector = |text: String| {
            let style = if is_focused {
                Style::default().fg(FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG_DIM)
            };
            Paragraph::new(Line::from(vec![
                Span::styled("◂ ", Style::default().fg(if is_focused { CYAN } else { MUTED })),
                Span::styled(text, style),
                Span::styled(" ▸", Style::default().fg(if is_focused { CYAN } else { MUTED })),
            ]))
        };
        let checkbox = |on: bool, text: &str| {
            let style = if is_focused {
                Style::default().fg(FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(FG_DIM)
            };
            Paragraph::new(Line::from(vec![
                Span::styled(if on { "[✓] " } else { "[ ] " }, Style::default().fg(if on { GREEN } else { MUTED })),
                Span::styled(text.to_string(), style),
            ]))
        };
        match field {
            Field::Driver => frame.render_widget(selector(form.driver.label().to_string()), value_area),
            Field::Ssl => frame.render_widget(selector(form.ssl.label().to_string()), value_area),
            Field::SavePassword => frame.render_widget(
                checkbox(form.save_password, &format!("store securely ({})", backend_label(backend))),
                value_area,
            ),
            Field::ReadOnly => {
                frame.render_widget(checkbox(form.read_only, "never allow changes through this connection"), value_area)
            }
            Field::Password => {
                let input = form.input(*field).expect("text field");
                if input.value().is_empty() && !is_focused {
                    let hint = if let Some(saved) = form.saved_password_elsewhere() {
                        format!(" saved for {} only", saved.address())
                    } else if form.has_stored_password() {
                        format!(" saved in {} — leave empty to keep", form.original.credential.label())
                    } else {
                        " asked on connect unless saved".to_string()
                    };
                    frame.render_widget(
                        Paragraph::new(Line::styled(hint, dim())).style(Style::default().bg(BG_DARK)),
                        value_area,
                    );
                } else {
                    draw_input(frame, value_area, input, is_focused, true);
                }
            }
            _ => {
                let input = form.input(*field).expect("text field");
                if input.value().is_empty() && !is_focused {
                    let hint = match field {
                        Field::Path => " path to the .db / .sqlite file",
                        Field::Database => " optional",
                        Field::Port => " default",
                        Field::SslCa => " optional: PEM file of a private CA",
                        _ => "",
                    };
                    frame.render_widget(
                        Paragraph::new(Line::styled(hint, dim())).style(Style::default().bg(BG_DARK)),
                        value_area,
                    );
                } else {
                    draw_input(frame, value_area, input, is_focused, false);
                }
            }
        }
    }
    // Why the SSL mode does not protect the password: under the field, or
    // above the error when there is no room.
    let mut messages = Vec::new();
    if let (Some(i), Some(warning)) = (fields.iter().position(|f| *f == Field::Ssl), form.tls_warning()) {
        let warning = Line::styled(format!("⚠ {warning}"), Style::default().fg(YELLOW));
        if step == 2 {
            let [_, value_area] =
                Layout::horizontal([Constraint::Length(17), Constraint::Min(10)]).areas(rows[i * 2 + 1]);
            frame.render_widget(Paragraph::new(warning), value_area);
        } else {
            messages.push(warning);
        }
    }
    if let Some(saved) = form.saved_password_elsewhere().filter(|_| form.password.value().is_empty()) {
        messages.push(Line::styled(
            format!(
                "Saving removes the password saved for {}: it is asked for on connect",
                safe_line(&saved.address())
            ),
            dim(),
        ));
    }
    if let Some(e) = &form.error {
        messages.push(Line::styled(format!("✖ {}", safe_line(e)), Style::default().fg(RED)));
    }
    frame.render_widget(Paragraph::new(messages).wrap(Wrap { trim: true }), rows[fields.len() * step]);
    frame.render_widget(
        footer(&[
            ("Tab/↑↓", "move"),
            ("←→/Space", "change"),
            ("Ctrl-T", "test"),
            ("Ctrl-S", "save"),
            ("Esc", "cancel"),
        ]),
        rows[fields.len() * step + 1],
    );
}
