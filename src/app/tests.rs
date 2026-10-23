//! Drives the real [`App`] against a temporary SQLite database and renders
//! it headlessly, checking editing and read-only behaviour end to end.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::*;
use crate::config::{Config, SslMode};
use crate::db::Transport;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

async fn wait_for(app: &mut App, rx: &mut UnboundedReceiver<AppEvent>, cond: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !cond(app) {
        let ev = tokio::time::timeout_at(deadline, rx.recv()).await.expect("timed out waiting for app state");
        app.on_app_event(ev.expect("channel closed"));
    }
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

async fn open_people(keymap: KeymapMode, read_only: bool) -> (tempfile::TempDir, App, UnboundedReceiver<AppEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    std::fs::File::create(&path).unwrap();
    let mut profile = ConnectionProfile::new(Driver::Sqlite);
    profile.name = "test".into();
    profile.path = path.to_string_lossy().into_owned();
    let db = Database::connect(&profile, None).await.unwrap();
    db.run_query(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO people VALUES (1, 'Ann'), (2, 'Bob');",
        false,
    )
    .await
    .unwrap();
    db.close().await;
    profile.read_only = read_only;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = Config { keymap, ..Config::default() };
    let mut app = App::new(config, tx);
    app.start(profile);
    wait_for(&mut app, &mut rx, |a| a.browser.is_some()).await;
    app.on_key(key(KeyCode::Enter)); // open the selected table
    wait_for(&mut app, &mut rx, |a| a.browser.as_ref().unwrap().table.as_ref().is_some_and(|t| t.loaded)).await;
    (dir, app, rx)
}

fn table(app: &App) -> &OpenTable {
    app.browser.as_ref().unwrap().table.as_ref().unwrap()
}

#[tokio::test]
async fn edit_and_commit_with_standard_keys() {
    let (dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("READ-WRITE"), "{screen}");
    assert!(screen.contains("Ann"));

    app.on_key(key(KeyCode::Right)); // name column
    app.on_key(key(KeyCode::Enter)); // edit
    assert!(matches!(app.popup, Some(Popup::CellEditor(_))));
    for _ in 0..3 {
        app.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut app, "Zed");
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Delete));
    assert_eq!(table(&app).grid.pending_count(), 2);
    assert!(render(&mut app, 120, 30).contains("2 pending"));

    app.on_key(ctrl('s'));
    assert!(matches!(app.popup, Some(Popup::Commit { .. })));
    app.on_key(key(KeyCode::Char('y')));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none() && !table(a).loading && !table(a).grid.has_pending()).await;
    wait_for(&mut app, &mut rx, |a| table(a).grid.rows.len() == 1).await;
    assert_eq!(table(&app).grid.rows[0][1].as_deref(), Some("Zed"));
    drop(dir);
}

#[tokio::test]
async fn read_only_session_never_opens_editors() {
    let (_dir, mut app, _rx) = open_people(KeymapMode::Standard, true).await;
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("READ-ONLY"), "{screen}");
    // Editing hints are hidden for read-only tables.
    assert!(!screen.contains("commit"), "{screen}");

    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::F(2)));
    assert!(app.popup.is_none());
    assert!(app.toast.as_ref().unwrap().text.contains("Read-only"));
    app.on_key(key(KeyCode::Enter)); // falls back to viewing the value
    assert!(matches!(app.popup, Some(Popup::CellView { .. })));
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Delete));
    app.on_key(key(KeyCode::Insert));
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(table(&app).grid.pending_count(), 0);

    // The SQL editor refuses writes before they reach the database.
    app.on_key(key(KeyCode::Char('3')));
    type_str(&mut app, "delete from people");
    app.on_key(key(KeyCode::F(5)));
    let b = app.browser.as_ref().unwrap();
    assert!(!b.query.running);
    assert!(b.query.message.as_ref().unwrap().0.contains("Refused DELETE"));
}

#[tokio::test]
async fn vim_keys_and_command_line() {
    let (_dir, mut app, mut rx) = open_people(KeymapMode::Vim, false).await;
    type_str(&mut app, "jl"); // row 2, name column
    assert_eq!((table(&app).grid.row, table(&app).grid.col), (1, 1));
    type_str(&mut app, "dd");
    assert!(table(&app).grid.edits.deleted.contains(&1));
    type_str(&mut app, "u");
    assert!(!table(&app).grid.has_pending());
    type_str(&mut app, "gg");
    assert_eq!(table(&app).grid.row, 0);
    type_str(&mut app, "o");
    assert_eq!(table(&app).grid.total_rows(), 3);
    type_str(&mut app, "u:filter id = 2");
    app.on_key(key(KeyCode::Enter));
    wait_for(&mut app, &mut rx, |a| !table(a).loading).await;
    assert_eq!(table(&app).grid.rows.len(), 1);
    type_str(&mut app, ":set");
    app.on_key(key(KeyCode::Esc));
    assert!(app.command.is_none());
}

#[tokio::test]
async fn renders_at_any_size() {
    let (_dir, mut app, _rx) = open_people(KeymapMode::Standard, false).await;
    for (w, h) in [(200, 60), (80, 24), (40, 12), (20, 6), (8, 4), (1, 1)] {
        render(&mut app, w, h);
        app.on_key(key(KeyCode::Char('?')));
        render(&mut app, w, h);
        app.on_key(key(KeyCode::Esc));
    }
    for tab in ['2', '3', '1'] {
        app.on_key(key(KeyCode::Char(tab)));
        render(&mut app, 100, 30);
        render(&mut app, 12, 5);
    }
}

/// Audit H1: text that ends a string early on the server must not get
/// past the filter validator or the read-only SQL editor.
#[tokio::test]
async fn hidden_statements_are_refused() {
    let (_dir, mut app, _rx) = open_people(KeymapMode::Standard, true).await;
    app.on_key(key(KeyCode::Char('/')));
    assert!(matches!(app.popup, Some(Popup::Prompt(_))));
    type_str(&mut app, r"name='\'); DELETE FROM people; --");
    app.on_key(key(KeyCode::Enter));
    assert!(app.toast.as_ref().unwrap().text.contains("Invalid filter"));
    assert!(table(&app).filter.is_none());

    app.on_key(key(KeyCode::Char('3')));
    type_str(&mut app, r"SELECT 'x\'; COMMIT; DELETE FROM people; --'");
    app.on_key(key(KeyCode::F(5)));
    let b = app.browser.as_ref().unwrap();
    assert!(!b.query.running);
    assert!(b.query.message.as_ref().unwrap().0.contains("Refused COMMIT"), "{:?}", b.query.message);
}

/// Audit L10: MyISAM and similar engines cannot roll back, so committing
/// several statements needs an explicit `!` and failures say what was kept.
#[tokio::test]
async fn non_transactional_tables_need_explicit_confirmation() {
    let (_dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    let t = app.browser.as_mut().unwrap().table.as_mut().unwrap();
    t.structure.as_mut().unwrap().non_transactional_engine = Some("MyISAM".into());
    app.on_key(key(KeyCode::Char('2')));
    assert!(render(&mut app, 120, 30).contains("MyISAM: no transactions"));
    app.on_key(key(KeyCode::Char('1')));

    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Backspace)); // name of row 1 -> NULL
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Delete)); // delete row 2
    app.on_key(ctrl('s'));
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("MyISAM tables do not support transactions"), "{screen}");
    assert!(screen.contains("Press ! to commit anyway"), "{screen}");
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(app.popup, Some(Popup::Commit { .. })), "Enter alone must not commit");
    assert!(app.busy.is_none());
    assert!(app.toast.as_ref().unwrap().text.contains("press !"));

    // Row 2 disappears before the commit: the delete matches nothing.
    let db = app.browser.as_ref().unwrap().db.clone();
    db.run_query("DELETE FROM people WHERE id = 2", false).await.unwrap();
    app.on_key(key(KeyCode::Char('!')));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none()).await;
    let Some(Popup::Message { body, .. }) = &app.popup else { panic!("expected an error") };
    assert!(body.contains("MyISAM tables do not support transactions"), "{body}");
    app.on_key(key(KeyCode::Esc));

    // A single statement cannot be half applied: plain `y` commits it.
    app.on_key(ctrl('z'));
    assert_eq!(table(&app).grid.pending_count(), 1);
    app.on_key(ctrl('s'));
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("MyISAM tables do not support transactions") && !screen.contains("Press !"), "{screen}");
    app.on_key(key(KeyCode::Char('y')));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none() && !table(a).loading && !table(a).grid.has_pending()).await;
    let rs = db.run_query("SELECT name FROM people ORDER BY id", false).await.unwrap();
    assert_eq!(rs.rows, vec![vec![None]]);
}

fn form(app: &App) -> &ConnectionForm {
    match &app.popup {
        Some(Popup::Form(f)) => f,
        _ => panic!("expected the connection form"),
    }
}

/// Moves the form's focus to `field`.
fn focus(app: &mut App, field: form::Field) {
    while form(app).focused() != field {
        app.on_key(key(KeyCode::Tab));
    }
}

/// Audit M1/M2: a new profile for a remote host verifies the server unless
/// the user picks another mode, which the form then warns about; verify-ca
/// needs a CA file. Nothing here saves the configuration.
#[tokio::test]
async fn new_remote_profiles_verify_the_server() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(form(&app).ssl, SslMode::Prefer, "localhost");
    focus(&mut app, form::Field::Host);
    for _ in 0.."localhost".len() {
        app.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut app, "db.example.com");
    assert_eq!(form(&app).ssl, SslMode::VerifyFull);
    let screen = render(&mut app, 120, 40);
    assert!(screen.contains("verify-full") && screen.contains("CA file") && !screen.contains('⚠'), "{screen}");

    // Choosing a weaker mode by hand sticks, and is flagged.
    focus(&mut app, form::Field::Ssl);
    for _ in 0..3 {
        app.on_key(key(KeyCode::Left));
    }
    assert_eq!(form(&app).ssl, SslMode::Prefer);
    assert!(render(&mut app, 120, 40).contains("⚠ server not verified, TLS optional: use verify-full"));
    focus(&mut app, form::Field::Host);
    type_str(&mut app, "x");
    assert_eq!(form(&app).ssl, SslMode::Prefer);

    // verify-ca only with a CA file: Ctrl-S keeps the form open.
    focus(&mut app, form::Field::Name);
    type_str(&mut app, "prod");
    focus(&mut app, form::Field::Ssl);
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Right));
    assert_eq!(form(&app).ssl, SslMode::VerifyCa);
    app.on_key(ctrl('s'));
    assert!(form(&app).error.as_ref().unwrap().contains("verify-ca needs a CA file"));
    assert!(render(&mut app, 120, 40).contains("verify-ca needs a CA file"));
    assert!(app.config.connections.is_empty());
    focus(&mut app, form::Field::SslCa);
    type_str(&mut app, "/etc/ssl/db-ca.pem");
    let p = form(&app).to_profile().unwrap();
    assert_eq!(
        (p.host.as_str(), p.ssl_mode, p.ssl_ca.as_str()),
        ("db.example.comx", SslMode::VerifyCa, "/etc/ssl/db-ca.pem")
    );
    assert!(render(&mut app, 120, 40).contains("⚠ server host name not checked"));
}

/// A saved profile keeps its explicit mode, and the connection list and
/// form warn when it does not authenticate a remote server.
#[tokio::test]
async fn saved_profiles_keep_their_mode_with_a_warning() {
    let mut p = ConnectionProfile::new(Driver::Postgres);
    p.name = "legacy".into();
    p.host = "db.example.com".into();
    p.ssl_mode = SslMode::Require;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config { connections: vec![p], ..Config::default() }, tx);
    let screen = render(&mut app, 120, 40);
    assert!(screen.contains("⚠ require: server not verified"), "{screen}");

    app.on_key(key(KeyCode::Char('e')));
    focus(&mut app, form::Field::Host);
    type_str(&mut app, ".internal");
    assert_eq!(form(&app).ssl, SslMode::Require);
    assert!(render(&mut app, 120, 40).contains("⚠ server not verified: use verify-full"));
    focus(&mut app, form::Field::Ssl);
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Right));
    assert_eq!(form(&app).ssl, SslMode::VerifyFull);
    assert!(!render(&mut app, 120, 40).contains('⚠'));
}

/// The status bar shows the transport the server reports, and flags a
/// remote session whose server was not authenticated.
#[tokio::test]
async fn status_bar_shows_transport_security() {
    let (_dir, mut app, _rx) = open_people(KeymapMode::Standard, false).await;
    assert_eq!(app.browser.as_ref().unwrap().session.transport, Transport::File);
    let screen = render(&mut app, 120, 30);
    assert!(!screen.contains("TLS") && !screen.contains("ENCRYPTED"), "{screen}");

    // The same session presented as a remote PostgreSQL one.
    let b = app.browser.as_mut().unwrap();
    b.profile.driver = Driver::Postgres;
    b.profile.host = "db.example.com".into();
    let tls = Transport::Tls { version: "TLSv1.3".into(), cipher: String::new() };
    let cases = [
        (SslMode::Prefer, Transport::Plain, "⚠ NOT ENCRYPTED"),
        (SslMode::Require, tls.clone(), "⚠ TLSv1.3 unverified"),
        (SslMode::VerifyFull, tls, "✓ TLSv1.3 verified"),
    ];
    for (mode, transport, expected) in cases {
        let b = app.browser.as_mut().unwrap();
        b.profile.ssl_mode = mode;
        b.session.transport = transport;
        let screen = render(&mut app, 120, 30);
        let status = screen.lines().last().unwrap();
        assert!(status.contains(expected), "{status}");
    }
}
