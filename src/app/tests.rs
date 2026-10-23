//! Drives the real [`App`] against a temporary SQLite database and renders
//! it headlessly, checking editing and read-only behaviour end to end.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::*;
use crate::config::{Config, SslMode};
use crate::db::transport_tests::fake_pg_server;
use crate::db::{Transport, Value};
use crate::secrets::fake::FakeKeyring;

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

// ---------------------------------------------------------------------------
// Saved passwords
// ---------------------------------------------------------------------------

/// An app keeping its configuration (`config`, saved there first) and vault
/// in `dir`, with an in-memory keyring.
fn app_in(dir: &Path, config: Config) -> (App, UnboundedReceiver<AppEvent>, Arc<FakeKeyring>) {
    config.save_to(&dir.join("config.toml")).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(config, tx);
    app.config_dir = Some(dir.to_path_buf());
    let keyring = Arc::new(FakeKeyring::default());
    app.keyring = Keyring::new(keyring.clone());
    (app, rx, keyring)
}

/// Handles background results until none has come for a moment.
async fn settle(app: &mut App, rx: &mut UnboundedReceiver<AppEvent>) {
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        app.on_app_event(ev);
    }
}

fn pg_profile(host: &str, port: u16, credential: CredentialStore) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(Driver::Postgres);
    p.name = "prod".into();
    p.host = host.into();
    p.port = Some(port);
    p.user = "app".into();
    p.credential = credential;
    p
}

/// Replaces the value of a form field.
fn set_field(app: &mut App, field: form::Field, value: &str) {
    focus(app, field);
    for _ in 0..form(app).input(field).unwrap().value().chars().count() {
        app.on_key(key(KeyCode::Backspace));
    }
    type_str(app, value);
}

fn toast(app: &App) -> &str {
    app.toast.as_ref().map_or("", |t| t.text.as_str())
}

fn credential(app: &App, id: &str) -> CredentialStore {
    app.profile(id).unwrap().credential
}

/// Audit L1: a password saved in the vault only goes to the server it was
/// saved for. Pointing the profile elsewhere (in the form, or by editing
/// config.toml) asks for the password instead, and Ctrl-T on unsaved edits
/// does not use it.
#[tokio::test]
async fn saved_passwords_only_go_to_their_server() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_pg_server(false);
    let saved = pg_profile("db.internal.example", 5432, CredentialStore::Vault);
    let vault = Vault::create(&dir.path().join("vault.toml"), "master password").unwrap();
    vault.set(&saved.id, &Endpoint::of(&saved), "prod-password").unwrap();
    let (mut app, mut rx, _) = app_in(dir.path(), Config { connections: vec![saved], ..Config::default() });
    app.vault = Some(vault);
    let mismatch = format!(
        "The saved password is for postgres://app@db.internal.example:5432, not postgres://app@127.0.0.1:{port}"
    );

    app.on_key(key(KeyCode::Char('e')));
    set_field(&mut app, form::Field::Host, "127.0.0.1");
    set_field(&mut app, form::Field::Port, &port.to_string());
    let screen = render(&mut app, 120, 40);
    assert!(screen.contains("saved for app@db.internal.example:5432 only"), "{screen}");
    app.on_key(ctrl('t'));
    wait_for(&mut app, &mut rx, |a| form(a).error.is_some()).await;
    assert!(form(&app).error.as_ref().unwrap().contains(&mismatch), "{:?}", form(&app).error);
    // A password typed for that server is used.
    set_field(&mut app, form::Field::Password, "typed-password");
    app.on_key(ctrl('t'));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none()).await;
    let capture = server.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(capture.password.as_deref(), Some("typed-password"));
    app.on_key(key(KeyCode::Esc));

    // The profile itself now points there (config.toml edited, say).
    app.update_config(|c| (c.connections[0].host, c.connections[0].port) = ("127.0.0.1".into(), Some(port)));
    app.on_key(key(KeyCode::Enter));
    let Some(Popup::Password(prompt)) = &app.popup else { panic!("expected the password prompt") };
    assert!(prompt.error.as_ref().unwrap().contains(&mismatch), "{:?}", prompt.error);
    // What is typed now is saved, for this server, once it works.
    assert!(prompt.remember);
    let screen = render(&mut app, 120, 40);
    assert!(screen.contains("The saved password is for") && screen.contains("server."), "{screen}");
    assert!(server.recv_timeout(Duration::from_millis(300)).is_err(), "the saved password was sent");
}

/// The same with the OS keyring, and a password saved by an older version
/// (without its server), which counts as saved for the profile as saved.
#[tokio::test]
async fn keyring_passwords_only_go_to_their_server() {
    let dir = tempfile::tempdir().unwrap();
    let (port, server) = fake_pg_server(false);
    let profile = pg_profile("127.0.0.1", port, CredentialStore::Keyring);
    let (mut app, mut rx, keyring) =
        app_in(dir.path(), Config { connections: vec![profile.clone()], ..Config::default() });
    let elsewhere = Endpoint::new(Driver::Postgres, "db.internal.example", None, "app");
    keyring.put(&profile.id, &elsewhere, "prod-password");

    app.on_key(key(KeyCode::Enter));
    wait_for(&mut app, &mut rx, |a| matches!(a.popup, Some(Popup::Password(_)))).await;
    let Some(Popup::Password(prompt)) = &app.popup else { unreachable!() };
    assert!(prompt.error.as_ref().unwrap().contains("saved password is for postgres://app@db.internal.example:5432"));
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Char('e')));
    app.on_key(ctrl('t'));
    wait_for(&mut app, &mut rx, |a| form(a).error.is_some()).await;
    assert!(form(&app).error.as_ref().unwrap().contains("saved password is for postgres://app@db.internal.example"));
    assert!(server.recv_timeout(Duration::from_millis(300)).is_err(), "the saved password was sent");

    // An old password is only used for the profile as it was saved...
    keyring.put_raw(&profile.id, "legacy-password");
    set_field(&mut app, form::Field::User, "admin");
    app.on_key(ctrl('t'));
    wait_for(&mut app, &mut rx, |a| form(a).error.is_some()).await;
    let error = form(&app).error.clone().unwrap();
    assert!(error.contains(&format!("is for postgres://app@127.0.0.1:{port}, not postgres://admin@")), "{error}");
    assert!(server.recv_timeout(Duration::from_millis(300)).is_err(), "the saved password was sent");
    app.on_key(key(KeyCode::Esc));
    // ...for which it still works.
    app.on_key(key(KeyCode::Enter));
    wait_for(&mut app, &mut rx, |a| matches!(a.popup, Some(Popup::Message { .. }))).await;
    let capture = server.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(capture.password.as_deref(), Some("legacy-password"));
}

/// A password saved before passwords were bound is saved again, bound to
/// its server and where it was, once a connection with it has worked. A
/// password is not saved for a profile that was changed or deleted while
/// connecting.
#[tokio::test]
async fn old_passwords_are_bound_once_they_work() {
    let dir = tempfile::tempdir().unwrap();
    let in_vault = pg_profile("db.example.com", 5432, CredentialStore::Vault);
    let mut in_keyring = pg_profile("db.example.com", 5433, CredentialStore::Keyring);
    in_keyring.name = "replica".into();
    let vault = Vault::create(&dir.path().join("vault.toml"), "master password").unwrap();
    vault.put_legacy(&in_vault.id, "vault-password");
    let config = Config { connections: vec![in_vault.clone(), in_keyring.clone()], ..Config::default() };
    let (mut app, mut rx, keyring) = app_in(dir.path(), config);
    keyring.put_raw(&in_keyring.id, "keyring-password");
    app.vault = Some(vault);

    // A local session stands in for the server's.
    let (_db_dir, mut db_app, _db_rx) = open_people(KeymapMode::Standard, false).await;
    let browser = db_app.browser.take().unwrap();
    // What a connection attempt the app started brings back.
    let connected = |app: &mut App, profile: &ConnectionProfile, remember| AppEvent::Connected {
        attempt: app.begin_connect(),
        profile: profile.clone(),
        db: browser.db.clone(),
        session: Box::new(browser.session.clone()),
        schemas: browser.schemas.clone(),
        remember: Some(remember),
    };
    for (profile, password) in [(&in_vault, "vault-password"), (&in_keyring, "keyring-password")] {
        // What `connect_with_saved` asks for with an unbound password.
        let remember = app.remember(profile, Zeroizing::new(password.into()), true);
        let event = connected(&mut app, profile, remember);
        app.on_app_event(event);
    }
    settle(&mut app, &mut rx).await;
    let secret = app.vault.as_ref().unwrap().get(&in_vault.id).unwrap().unwrap();
    assert_eq!((secret.password(), secret.endpoint()), ("vault-password", Some(&Endpoint::of(&in_vault))));
    let secret = keyring.secret(&in_keyring.id).unwrap();
    assert_eq!((secret.password(), secret.endpoint()), ("keyring-password", Some(&Endpoint::of(&in_keyring))));
    assert_eq!(
        (credential(&app, &in_vault.id), credential(&app, &in_keyring.id)),
        (CredentialStore::Vault, CredentialStore::Keyring)
    );
    assert!(toast(&app).starts_with("Connected"), "re-saved quietly: {}", toast(&app));

    // The profile now points elsewhere: the password is not saved for it.
    let remember = app.remember(&in_vault, Zeroizing::new("typed".into()), false);
    app.update_config(|c| c.connections[0].host = "db2.example.com".into());
    let event = connected(&mut app, &in_vault, remember);
    app.on_app_event(event);
    // Deleted while connecting.
    let remember = app.remember(&in_keyring, Zeroizing::new("typed".into()), false);
    app.confirm(ConfirmAction::DeleteProfile(in_keyring.id.clone()));
    let event = connected(&mut app, &in_keyring, remember);
    app.on_app_event(event);
    settle(&mut app, &mut rx).await;
    assert_eq!(app.vault.as_ref().unwrap().get(&in_vault.id).unwrap().unwrap().password(), "vault-password");
    assert_eq!(keyring.raw(&in_keyring.id), None);
    assert!(!toast(&app).contains("Password saved"), "{}", toast(&app));
}

/// Audit L2: results of keyring writes that come late, for a profile that
/// was deleted or whose password is no longer to be saved, are removed
/// rather than recorded. A slow keyring is not a failure: nothing goes to
/// the vault, and the write counts when it completes.
#[tokio::test]
async fn late_keyring_results_are_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, mut rx, keyring) = app_in(dir.path(), Config::default());
    let new_profile = |app: &mut App, password: &str| {
        app.on_key(key(KeyCode::Char('n')));
        type_str(app, "prod");
        set_field(app, form::Field::Password, password);
        app.on_key(ctrl('s'));
        app.config.connections.last().unwrap().id.clone()
    };

    // Deleted while the keyring waits (for its unlock prompt, say).
    keyring.hold();
    let id = new_profile(&mut app, "s3cret");
    assert_eq!(credential(&app, &id), CredentialStore::None);
    app.on_key(key(KeyCode::Delete));
    app.on_key(key(KeyCode::Char('y')));
    assert!(app.config.connections.is_empty());
    assert!(app.keyring_busy(), "quitting now waits a moment for the keyring");
    keyring.release();
    settle(&mut app, &mut rx).await;
    assert!(!app.keyring_busy());
    assert_eq!(keyring.raw(&id), None);
    assert!(app.popup.is_none() && !dir.path().join("vault.toml").exists());
    assert!(!toast(&app).contains("Password saved"), "{}", toast(&app));

    // No longer to be saved.
    keyring.hold();
    let id = new_profile(&mut app, "s3cret");
    app.on_key(key(KeyCode::Char('e')));
    assert!(form(&app).save_password, "the form shows the save in progress");
    focus(&mut app, form::Field::SavePassword);
    app.on_key(key(KeyCode::Char(' ')));
    app.on_key(ctrl('s'));
    keyring.release();
    settle(&mut app, &mut rx).await;
    assert_eq!(keyring.raw(&id), None);
    assert_eq!(credential(&app, &id), CredentialStore::None);

    // Slow.
    keyring.hold();
    app.on_key(key(KeyCode::Char('e')));
    focus(&mut app, form::Field::SavePassword);
    app.on_key(key(KeyCode::Char(' ')));
    set_field(&mut app, form::Field::Password, "later");
    app.on_key(ctrl('s'));
    for (started, _) in app.credentials.pending.values_mut() {
        *started = started.checked_sub(KEYRING_TIMEOUT).unwrap();
    }
    app.on_tick();
    assert!(toast(&app).contains("The OS keyring is not answering"), "{}", toast(&app));
    settle(&mut app, &mut rx).await;
    assert!(app.popup.is_none() && !dir.path().join("vault.toml").exists());
    assert_eq!(credential(&app, &id), CredentialStore::None);
    keyring.release();
    settle(&mut app, &mut rx).await;
    assert_eq!(credential(&app, &id), CredentialStore::Keyring);
    assert_eq!(keyring.secret(&id).unwrap().password(), "later");
    assert_eq!(toast(&app), "Password saved in the OS keyring");
}

/// Audit L2: a password moved to the other store is removed from the one it
/// was in, and deleting a profile removes it from both.
#[tokio::test]
async fn old_copies_go_when_the_store_changes() {
    let dir = tempfile::tempdir().unwrap();
    let profile = pg_profile("db.example.com", 5432, CredentialStore::Vault);
    let id = profile.id.clone();
    let vault = Vault::create(&dir.path().join("vault.toml"), "master password").unwrap();
    vault.set(&id, &Endpoint::of(&profile), "old").unwrap();
    let (mut app, mut rx, keyring) =
        app_in(dir.path(), Config { connections: vec![profile.clone()], ..Config::default() });
    app.vault = Some(vault);
    let in_vault = |app: &App| app.vault.as_ref().unwrap().get(&id).unwrap().map(|s| s.password().to_string());
    let save = |app: &mut App, password: &str| {
        app.on_key(key(KeyCode::Char('e')));
        set_field(app, form::Field::Password, password);
        app.on_key(ctrl('s'));
    };

    // Saved again, into the keyring this time.
    save(&mut app, "new");
    settle(&mut app, &mut rx).await;
    assert_eq!(credential(&app, &id), CredentialStore::Keyring);
    assert_eq!(keyring.secret(&id).unwrap().password(), "new");
    assert_eq!(in_vault(&app), None);

    // And back into the vault.
    app.update_config(|c| c.secret_backend = SecretBackend::Vault);
    save(&mut app, "newer");
    settle(&mut app, &mut rx).await;
    assert_eq!(credential(&app, &id), CredentialStore::Vault);
    assert_eq!(in_vault(&app).as_deref(), Some("newer"));
    assert_eq!(keyring.raw(&id), None);

    // A copy left in the keyring (by an older version, say) goes as well.
    keyring.put(&id, &Endpoint::of(&profile), "stray");
    app.on_key(key(KeyCode::Delete));
    app.on_key(key(KeyCode::Char('y')));
    settle(&mut app, &mut rx).await;
    assert_eq!((keyring.raw(&id), in_vault(&app)), (None, None));
}

/// The master password is checked in the background, and a vault another
/// dbtui instance created meanwhile is unlocked, never replaced.
#[tokio::test]
async fn vault_is_opened_in_the_background_and_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.toml");
    let config = Config { secret_backend: SecretBackend::Vault, ..Config::default() };
    let (mut app, mut rx, _) = app_in(dir.path(), config);
    app.on_key(key(KeyCode::Char('n')));
    type_str(&mut app, "prod");
    set_field(&mut app, form::Field::Password, "s3cret");
    app.on_key(ctrl('s'));
    let id = app.config.connections[0].id.clone();
    assert!(matches!(&app.popup, Some(Popup::Vault(v)) if v.create));
    type_str(&mut app, "first master");
    app.on_key(key(KeyCode::Tab));
    type_str(&mut app, "first master");

    let other = Vault::create(&path, "other master").unwrap();
    other.set("x", &Endpoint::new(Driver::Postgres, "db", None, "app"), "theirs").unwrap();
    app.on_key(key(KeyCode::Enter));
    assert!(render(&mut app, 100, 30).contains("Creating the vault…"));
    type_str(&mut app, "ignored");
    assert!(matches!(&app.popup, Some(Popup::Vault(v)) if v.working.is_some() && v.confirm.value() == "first master"));
    wait_for(&mut app, &mut rx, |a| matches!(&a.popup, Some(Popup::Vault(v)) if v.working.is_none())).await;
    let Some(Popup::Vault(v)) = &app.popup else { unreachable!() };
    assert!(!v.create && v.input.value().is_empty());
    assert!(v.error.as_ref().unwrap().contains("created by another dbtui instance"), "{:?}", v.error);
    assert!(render(&mut app, 100, 30).contains("Unlock password vault"));

    type_str(&mut app, "other master");
    app.on_key(key(KeyCode::Enter));
    assert!(render(&mut app, 100, 30).contains("Checking the master password…"));
    wait_for(&mut app, &mut rx, |a| a.popup.is_none()).await;
    assert_eq!(credential(&app, &id), CredentialStore::Vault);
    let theirs = Vault::unlock(&path, "other master").unwrap();
    assert_eq!(theirs.get(&id).unwrap().unwrap().password(), "s3cret");
    assert_eq!(theirs.get("x").unwrap().unwrap().password(), "theirs");
}

/// Two instances sharing a configuration keep each other's changes.
#[tokio::test]
async fn instances_keep_each_others_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut one, _rx, _) = app_in(dir.path(), Config::default());
    let (mut two, _rx, _) = app_in(dir.path(), Config::default());
    let add = |app: &mut App, name: &str| {
        app.on_key(key(KeyCode::Char('n')));
        type_str(app, name);
        app.on_key(ctrl('s'));
    };
    let names = |config: &Config| config.connections.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    add(&mut one, "one");
    add(&mut two, "two");
    assert_eq!(names(&two.config), ["one", "two"]);
    one.on_key(ctrl('k'));
    assert_eq!((names(&one.config), one.config.keymap), (vec!["one".to_string(), "two".into()], KeymapMode::Vim));
    two.on_key(key(KeyCode::Up));
    two.on_key(key(KeyCode::Delete));
    two.on_key(key(KeyCode::Char('y')));
    let on_disk = Config::load_from(&dir.path().join("config.toml")).unwrap();
    assert_eq!((names(&on_disk), on_disk.keymap), (vec!["two".to_string()], KeymapMode::Vim));
}

// ---------------------------------------------------------------------------
// Async results and staged edits (audit M4, L8, data fidelity)
// ---------------------------------------------------------------------------

/// A SQLite database file `file` in `dir`, set up with `setup`.
async fn sqlite_db(dir: &Path, file: &str, setup: &str) -> ConnectionProfile {
    let path = dir.join(file);
    std::fs::File::create(&path).unwrap();
    let mut profile = ConnectionProfile::new(Driver::Sqlite);
    profile.name = file.into();
    profile.path = path.to_string_lossy().into_owned();
    let db = Database::connect(&profile, None).await.unwrap();
    db.run_query(setup, false).await.unwrap();
    db.close().await;
    profile
}

async fn rows_of(profile: &ConnectionProfile, sql: &str) -> Vec<Vec<Option<String>>> {
    let db = Database::connect(profile, None).await.unwrap();
    let rs = db.run_query(sql, false).await.unwrap();
    db.close().await;
    rs.rows
}

/// Connects to `profile` and opens its first table.
async fn connect_and_open(app: &mut App, rx: &mut UnboundedReceiver<AppEvent>, profile: ConnectionProfile) {
    app.start(profile);
    wait_for(app, rx, |a| a.browser.is_some()).await;
    app.on_key(key(KeyCode::Enter));
    wait_for(app, rx, |a| a.browser.as_ref().unwrap().table.as_ref().is_some_and(|t| t.loaded && !t.loading)).await;
}

fn table_mut(app: &mut App) -> &mut OpenTable {
    app.browser.as_mut().unwrap().table.as_mut().unwrap()
}

fn names(app: &App) -> Vec<Option<String>> {
    table(app).grid.rows.iter().map(|r| r[1].clone()).collect()
}

/// Holds the write lock of a SQLite file, as another program would, until
/// the transaction is ended.
async fn lock_sqlite(profile: &ConnectionProfile) -> sqlx::sqlite::SqliteConnection {
    use sqlx::Connection;
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&profile.path);
    let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&opts).await.unwrap();
    sqlx::raw_sql("BEGIN IMMEDIATE").execute(&mut conn).await.unwrap();
    conn
}

/// Audit M4: a connection that finishes after another one was asked for is
/// closed and ignored, even while a commit preview is open; and a preview
/// is only ever committed to the connection it was made on.
#[tokio::test]
async fn late_connections_are_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let setup = |v: &str| format!("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1, '{v}');");
    let a = sqlite_db(dir.path(), "a.db", &setup("a")).await;
    let b = sqlite_db(dir.path(), "b.db", &setup("b")).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    // Connecting to a, then (before it is done) to b.
    app.start(a.clone());
    app.start(b.clone());
    let mut events = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    let from_a = |e: &AppEvent| matches!(e, AppEvent::Connected { profile, .. } if profile.name == "a.db");
    let late = events.remove(events.iter().position(from_a).unwrap());
    let AppEvent::Connected { attempt: late_attempt, db: late_db, .. } = &late else { unreachable!() };
    let (late_attempt, late_db) = (*late_attempt, late_db.clone());
    app.on_app_event(events.remove(0));
    assert_eq!(app.browser.as_ref().unwrap().profile.name, "b.db");
    app.on_key(key(KeyCode::Enter));
    wait_for(&mut app, &mut rx, |a| table(a).loaded && !table(a).loading).await;
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Backspace)); // v = NULL
    app.on_key(ctrl('s'));
    assert!(matches!(app.popup, Some(Popup::Commit { .. })));

    // a comes through now: closed, and nothing changes.
    app.on_app_event(late);
    let error = "late failure".to_string();
    app.on_app_event(AppEvent::ConnectFailed {
        attempt: late_attempt,
        profile: a.clone(),
        error,
        needs_password: false,
    });
    assert_eq!(app.browser.as_ref().unwrap().profile.name, "b.db");
    assert!(matches!(app.popup, Some(Popup::Commit { .. })));
    for _ in 0..100 {
        if late_db.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(late_db.is_closed(), "the discarded connection is closed");
    app.on_key(key(KeyCode::Char('y')));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none() && !table(a).loading && !table(a).grid.has_pending()).await;
    assert_eq!(rows_of(&b, "SELECT v FROM t").await, vec![vec![None]]);
    assert_eq!(rows_of(&a, "SELECT v FROM t").await, vec![vec![Some("a".into())]]);

    // The connection is replaced while a preview is open (nothing in the
    // UI does that; it stands for any change of connection): refused.
    app.on_key(key(KeyCode::Enter));
    type_str(&mut app, "x");
    app.on_key(key(KeyCode::Enter));
    app.on_key(ctrl('s'));
    assert!(matches!(app.popup, Some(Popup::Commit { .. })));
    app.connect(a.clone());
    wait_for(&mut app, &mut rx, |a| a.browser.as_ref().unwrap().profile.name == "a.db").await;
    app.on_key(key(KeyCode::Char('y')));
    assert!(toast(&app).contains("Nothing was committed"), "{}", toast(&app));
    assert!(app.busy.is_none());
    assert_eq!(rows_of(&a, "SELECT v FROM t").await, vec![vec![Some("a".into())]]);
    assert_eq!(rows_of(&b, "SELECT v FROM t").await, vec![vec![None]]);
}

/// Audit M4: page, count, schema and query results requested on an
/// earlier connection are dropped, even when their `seq` matches.
#[tokio::test]
async fn results_from_a_previous_connection_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let a = sqlite_db(
        dir.path(),
        "a.db",
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO people VALUES (1, 'Ann-A'), (2, 'Bob-A');",
    )
    .await;
    let b = sqlite_db(
        dir.path(),
        "b.db",
        "CREATE TABLE staff (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO staff VALUES (1, 'Sam-B'), (2, 'Tia-B');",
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    app.start(a);
    wait_for(&mut app, &mut rx, |a| a.browser.is_some()).await;
    // Requests on a whose results are slow to come.
    let request_all = |app: &mut App| {
        app.on_key(key(KeyCode::Enter));
        app.refresh_schemas();
        app.on_key(key(KeyCode::Char('3')));
        type_str(app, "SELECT 'slow'");
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Esc));
    };
    request_all(&mut app);
    let mut held = Vec::new();
    while held.len() < 4 {
        held.push(rx.recv().await.unwrap());
    }
    assert_eq!(app.browser.as_ref().map(|b| (b.seq, b.schema_seq, b.query.seq)), Some((1, 1, 1)));

    app.run_command("q!");
    app.start(b);
    wait_for(&mut app, &mut rx, |a| a.browser.is_some()).await;
    request_all(&mut app);
    wait_for(&mut app, &mut rx, |a| table(a).loaded && !a.browser.as_ref().unwrap().query.running).await;
    settle(&mut app, &mut rx).await;
    let state = |app: &App| {
        let b = app.browser.as_ref().unwrap();
        let schemas: Vec<String> = b.schemas.iter().flat_map(|s| &s.tables).map(|t| t.table.name.clone()).collect();
        (names(app), table(app).total, schemas, b.query.result.as_ref().map(|g| g.rows.clone()))
    };
    let before = state(&app);
    assert_eq!(app.browser.as_ref().map(|b| (b.seq, b.schema_seq, b.query.seq)), Some((1, 1, 1)));
    assert_eq!(before.0, vec![Some("Sam-B".to_string()), Some("Tia-B".into())]);
    for event in held {
        app.on_app_event(event);
    }
    assert_eq!(state(&app), before);
}

/// Audit M4: a cell editor stages its value on the row it was opened on,
/// found by key wherever a reload has put it, and stages nothing if that
/// row is gone.
#[tokio::test]
async fn cell_editor_follows_its_row() {
    let (_dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Enter)); // Ann (id 1), on row 0
    // Reloaded meanwhile (say, after a statement run in the SQL tab), in
    // another order.
    table_mut(&mut app).order_by = Some(("id".into(), false));
    app.request_page();
    wait_for(&mut app, &mut rx, |a| !table(a).loading).await;
    assert_eq!(names(&app), vec![Some("Bob".to_string()), Some("Ann".into())]);
    assert!(matches!(app.popup, Some(Popup::CellEditor(_))), "Ann is still there");
    type_str(&mut app, "-x");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(table(&app).grid.edits.updates, [((1, 1), Some("Ann-x".to_string()))].into());
    app.on_key(ctrl('s'));
    let Some(Popup::Commit { statements, .. }) = &app.popup else { panic!("expected the commit preview") };
    assert_eq!(statements[0].params, vec![Some("Ann-x".into()), Some(Value::Integer(1))]);
    app.on_key(key(KeyCode::Esc));
    app.on_key(ctrl('z'));

    // The row is deleted elsewhere while it is being edited.
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Enter)); // Bob, on row 0
    let db = app.browser.as_ref().unwrap().db.clone();
    db.run_query("DELETE FROM people WHERE id = 2", false).await.unwrap();
    app.request_page();
    wait_for(&mut app, &mut rx, |a| !table(a).loading).await;
    assert!(app.popup.is_none());
    assert!(toast(&app).contains("no longer shown"), "{}", toast(&app));
    assert_eq!((names(&app), table(&app).grid.pending_count()), (vec![Some("Ann".to_string())], 0));
}

/// Audit M4: nothing can be staged while the rows are being replaced or
/// committed; a second commit, discarding, and switching tables wait for
/// the commit; quitting and disconnecting say it is still running.
#[tokio::test]
async fn editing_waits_for_loads_and_commits() {
    let dir = tempfile::tempdir().unwrap();
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO people VALUES (1, 'Ann'), (2, 'Bob');
         CREATE TABLE pets (id INTEGER PRIMARY KEY, name TEXT);",
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile.clone()).await;
    assert_eq!(table(&app).info.table.name, "people");

    // A reload is on its way.
    app.on_key(key(KeyCode::F(5)));
    assert!(table(&app).loading);
    app.on_key(key(KeyCode::Right));
    for k in [KeyCode::Backspace, KeyCode::Delete, KeyCode::Insert, KeyCode::Enter, KeyCode::F(2)] {
        app.on_key(key(k));
        assert!(toast(&app).contains("loading"), "{k:?}: {}", toast(&app));
    }
    assert!(app.popup.is_none());
    assert_eq!(table(&app).grid.pending_count(), 0);
    wait_for(&mut app, &mut rx, |a| !table(a).loading).await;

    // A commit is on its way (another program holds the write lock).
    app.on_key(key(KeyCode::Insert));
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Enter));
    type_str(&mut app, "Eve");
    app.on_key(key(KeyCode::Enter));
    let mut lock = lock_sqlite(&profile).await;
    app.on_key(ctrl('s'));
    app.on_key(key(KeyCode::Char('y')));
    assert!(app.browser.as_ref().unwrap().commit.is_some());
    app.on_key(ctrl('s'));
    assert!(app.popup.is_none() && toast(&app).contains("already running"), "{}", toast(&app));
    for k in [KeyCode::Backspace, KeyCode::Delete, KeyCode::Insert, KeyCode::Enter] {
        app.on_key(key(k));
        assert!(app.popup.is_none() && toast(&app).contains("commit is running"), "{k:?}: {}", toast(&app));
    }
    app.on_key(ctrl('z'));
    app.run_command("e!");
    app.on_key(ctrl('o')); // sort
    assert_eq!(table(&app).grid.pending_count(), 1);
    assert!(toast(&app).contains("commit is running"), "{}", toast(&app));
    app.on_key(key(KeyCode::Tab)); // to the tables
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(table(&app).info.table.name, "people");
    assert!(toast(&app).contains("commit is running"), "{}", toast(&app));
    app.on_key(ctrl('q'));
    let Some(Popup::Confirm { message, .. }) = &app.popup else { panic!("expected a confirmation") };
    assert!(message.contains("commit to t.db is still running"), "{message}");
    app.on_key(key(KeyCode::Esc));
    app.run_command("q");
    let Some(Popup::Confirm { message, .. }) = &app.popup else { panic!("expected a confirmation") };
    assert!(message.contains("still running and may still be applied"), "{message}");
    app.on_key(key(KeyCode::Esc));

    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    wait_for(&mut app, &mut rx, |a| a.browser.as_ref().unwrap().commit.is_none() && !table(a).loading).await;
    assert!(app.popup.is_none(), "committed");
    assert_eq!(rows_of(&profile, "SELECT name FROM people WHERE name = 'Eve'").await.len(), 1);
    assert_eq!(names(&app), vec![Some("Ann".to_string()), Some("Bob".into()), Some("Eve".into())]);
    assert_eq!(table(&app).grid.pending_count(), 0);
}

/// Audit M4: a commit whose result comes while the SQL tab is shown still
/// reloads the table, so its rows are never stale (and editable) after it.
#[tokio::test]
async fn commit_reloads_the_table_from_any_tab() {
    let (_dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Delete)); // Bob
    app.on_key(key(KeyCode::Insert));
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Enter));
    type_str(&mut app, "Eve");
    app.on_key(key(KeyCode::Enter));
    app.on_key(ctrl('s'));
    app.on_key(key(KeyCode::Char('y')));
    app.on_key(key(KeyCode::Char('3')));
    wait_for(&mut app, &mut rx, |a| a.busy.is_none() && !table(a).loading).await;
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Char('1')));
    assert_eq!((names(&app), table(&app).grid.pending_count()), (vec![Some("Ann".to_string()), Some("Eve".into())], 0));
}

/// Audit (data fidelity): the cell viewer copies the stored value, not its
/// re-indented display, and copies nothing for NULL (which used to be
/// pasted back as the text 'NULL').
#[tokio::test]
async fn viewer_copies_the_stored_value() {
    let dir = tempfile::tempdir().unwrap();
    let json = r#"{"b":1,"a":12345678901234567890123,"b":2}"#;
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, note TEXT); INSERT INTO docs VALUES (1, '{json}', NULL);"),
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile.clone()).await;
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Char(' ')));
    let Some(Popup::CellView { title, text, .. }) = &app.popup else { panic!("expected the viewer") };
    assert!(title.contains("JSON") && text.contains("\"a\": 12345678901234567890123,"), "{text}");
    assert_eq!(text.matches("\"b\"").count(), 2, "{text}");
    app.on_key(key(KeyCode::Char('y')));
    assert_eq!(app.register.as_deref(), Some(json));
    app.on_key(key(KeyCode::Esc));

    app.on_key(key(KeyCode::Right)); // note: NULL
    app.on_key(key(KeyCode::Char(' ')));
    app.on_key(key(KeyCode::Char('y')));
    assert_eq!(toast(&app), "Cell is NULL — nothing copied");
    assert_eq!(app.register.as_deref(), Some(json));
    app.on_key(key(KeyCode::Esc));
    app.on_key(ctrl('v'));
    assert_eq!(table(&app).grid.edits.updates, [((0, 2), Some(json.to_string()))].into());
}

/// Audit (data fidelity): confirming the cell editor without a change
/// stages nothing, and an edit keeps the value's line endings.
#[tokio::test]
async fn cell_editor_keeps_values_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO notes VALUES (1, 'line1' || char(13, 10) || 'line2' || char(13, 10)), (2, 'a' || char(10) || 'b' || char(10)), (3, NULL);",
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile).await;
    app.on_key(key(KeyCode::Right));
    for _ in 0..3 {
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.popup, Some(Popup::CellEditor(_))));
        app.on_key(key(KeyCode::Enter));
        assert!(app.popup.is_none());
        app.on_key(key(KeyCode::Down));
    }
    assert_eq!(table(&app).grid.pending_count(), 0);

    table_mut(&mut app).grid.row = 0;
    app.on_key(key(KeyCode::Enter));
    type_str(&mut app, "line3");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(table(&app).grid.cell_text(0, 1).as_deref(), Some("line1\r\nline2\r\nline3"));
}

/// Audit L8: binary values are shown as `0x…`; that text is never written
/// back by editing the cell.
#[tokio::test]
async fn binary_values_are_not_edited_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        "CREATE TABLE files (id INTEGER PRIMARY KEY, data BLOB); INSERT INTO files VALUES (1, x'CAFE');",
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile).await;
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::F(2)));
    assert!(app.popup.is_none() && toast(&app).contains("binary data"), "{}", toast(&app));
    app.on_key(key(KeyCode::Enter));
    let Some(Popup::CellView { text, .. }) = &app.popup else { panic!("expected the viewer") };
    assert_eq!(text, "0xcafe");
}

/// Audit M4: a saved password read for a connection attempt that was given
/// up for another one does not start connecting after all.
#[tokio::test]
async fn stale_saved_passwords_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let saved = pg_profile("127.0.0.1", 1, CredentialStore::Keyring);
    let config = Config { connections: vec![saved.clone()], ..Config::default() };
    let (mut app, mut rx, keyring) = app_in(dir.path(), config);
    keyring.put(&saved.id, &Endpoint::of(&saved), "secret");
    app.on_key(key(KeyCode::Enter)); // reads the keyring in the background
    let read = rx.recv().await.unwrap();
    assert!(matches!(read, AppEvent::SavedPassword { .. }));
    let local = sqlite_db(dir.path(), "local.db", "CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    app.start(local);
    wait_for(&mut app, &mut rx, |a| a.browser.is_some()).await;
    app.on_app_event(read);
    assert!(app.busy.is_none() && app.connecting.is_none() && app.popup.is_none());
    assert_eq!(app.browser.as_ref().unwrap().profile.name, "local.db");
}

/// Audit M4: disconnecting while a commit runs says so, and how the commit
/// ended is reported when it does.
#[tokio::test]
async fn commit_result_is_reported_after_disconnecting() {
    let (dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    let profile = app.browser.as_ref().unwrap().profile.clone();
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Backspace));
    let mut lock = lock_sqlite(&profile).await;
    app.on_key(ctrl('s'));
    app.on_key(key(KeyCode::Char('y')));
    app.run_command("q!");
    assert!(app.browser.is_none() && app.busy.is_none());
    assert!(toast(&app).contains("its commit is still running and may still be applied"), "{}", toast(&app));
    sqlx::raw_sql("COMMIT").execute(&mut lock).await.unwrap();
    wait_for(&mut app, &mut rx, |a| toast(a).starts_with("The commit to test")).await;
    assert_eq!(toast(&app), "The commit to test finished: 1 row(s) affected");
    assert_eq!(rows_of(&profile, "SELECT name FROM people WHERE id = 1").await, vec![vec![None]]);
    drop(dir);
}

/// After SQL that wrote, the open table is reloaded, so that its rows (and
/// the keys edits are made on) are not stale; with staged changes, it warns
/// instead. A plain SELECT reloads nothing.
#[tokio::test]
async fn sql_writes_reload_the_open_table() {
    let (_dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    let run = async |app: &mut App, rx: &mut UnboundedReceiver<AppEvent>, sql: &str| {
        app.on_key(key(KeyCode::Char('3')));
        app.browser.as_mut().unwrap().query.editor = TextArea::default();
        type_str(app, sql);
        app.on_key(key(KeyCode::F(5)));
        wait_for(app, rx, |a| !a.browser.as_ref().unwrap().query.running && !table(a).loading).await;
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('1')));
    };
    let seq = app.browser.as_ref().unwrap().seq;
    run(&mut app, &mut rx, "SELECT name FROM people").await;
    assert_eq!(app.browser.as_ref().unwrap().seq, seq, "not reloaded");
    let replace = "DELETE FROM people WHERE id = 2; INSERT INTO people VALUES (2, 'Other')";
    run(&mut app, &mut rx, replace).await;
    assert_eq!(names(&app), vec![Some("Ann".to_string()), Some("Other".into())]);

    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Backspace));
    run(&mut app, &mut rx, replace).await;
    assert!(toast(&app).contains("The SQL may have changed people"), "{}", toast(&app));
    assert_eq!(table(&app).grid.pending_count(), 1);
}

/// What the terminal is sent for one frame (see [`render`] for the text).
fn terminal_bytes(app: &mut App, w: u16, h: u16) -> String {
    use ratatui::backend::{Backend, CrosstermBackend};
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut bytes = Vec::new();
    let mut out = CrosstermBackend::new(&mut bytes);
    let cells = buf.content().iter().enumerate().map(|(i, c)| ((i as u16) % w, (i as u16) / w, c));
    out.draw(cells).unwrap();
    Backend::flush(&mut out).unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Audit L5: names and values from the database are drawn with their
/// hidden characters made visible, and names holding them are flagged.
/// Nothing in them reaches the terminal as a control or bidi character.
#[tokio::test]
async fn hidden_characters_are_shown_and_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let evil = "a\x1b]0;pwned\x07\u{9b}2J\u{202E}gnp.exe";
    let setup = format!(
        "CREATE TABLE \"{evil}\" (id INTEGER PRIMARY KEY, \"c\x1b[2J\u{202E}x\" TEXT DEFAULT 'd\x1b]52;c;ZXZpbA==\x07');
         INSERT INTO \"{evil}\" VALUES (1, 'v\x1b]52;c;ZXZpbA==\x07\u{202E}abc');
         CREATE TABLE accounts (id INTEGER PRIMARY KEY);
         CREATE TABLE \"accounts\u{200B}\" (id INTEGER PRIMARY KEY);
         CREATE TABLE \"accounts\x01\" (id INTEGER PRIMARY KEY);"
    );
    let profile = sqlite_db(dir.path(), "t.db", &setup).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile).await;
    assert_eq!(table(&app).info.table.name, evil);

    let screen = render(&mut app, 160, 30);
    for shown in
        ["│   ▦ accounts ", "│   ▦ ⚠ accounts⟨U+200B⟩", "│   ▦ ⚠ accounts␁", "⚠ a␛]0;pwned␇⟨U+009B⟩2J⟨U+202E⟩gnp.exe"]
    {
        assert!(screen.contains(shown), "{shown} in\n{screen}");
    }
    assert!(screen.contains("v␛]52;c;ZXZpbA==␇⟨U+202E⟩abc"), "{screen}");

    // Every screen that shows them: data, structure, the viewer, the commit
    // preview (values and names).
    let mut sent = terminal_bytes(&mut app, 160, 30);
    app.on_key(key(KeyCode::Char('2')));
    let structure = render(&mut app, 160, 30);
    assert!(structure.contains("⚠ c␛[2J⟨U+202E⟩x") && structure.contains("'d␛]52;c;ZXZpbA==␇'"), "{structure}");
    sent.push_str(&terminal_bytes(&mut app, 160, 30));
    app.on_key(key(KeyCode::Char('1')));
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::F(3)));
    sent.push_str(&terminal_bytes(&mut app, 160, 30));
    app.on_key(key(KeyCode::Esc));
    app.register = Some("new\u{202E}\x1b]0;x\x07".into());
    app.on_key(ctrl('v'));
    app.on_key(ctrl('s'));
    let preview = render(&mut app, 160, 30);
    assert!(preview.contains("hold characters that would not show"), "{preview}");
    assert!(preview.contains("SET \"c␛[2J⟨U+202E⟩x\" = ?") && preview.contains("'new⟨U+202E⟩␛]0;x␇'"), "{preview}");
    sent.push_str(&terminal_bytes(&mut app, 160, 30));
    for (bad, what) in [("\x1b]", "OSC"), ("\x07", "BEL"), ("\u{9b}", "C1 CSI"), ("\u{202E}", "RLO"), ("\x01", "SOH")] {
        assert!(!sent.contains(bad), "{what} reached the terminal");
    }
}

/// Audit L4: what goes on the system clipboard cannot act as keystrokes
/// when pasted (`ESC [201~` would end a bracketed paste); pasting into a
/// cell in dbtui keeps the value exactly.
#[tokio::test]
async fn copied_values_cannot_type_keys() {
    let dir = tempfile::tempdir().unwrap();
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, w TEXT);
         INSERT INTO t VALUES (1, 'x' || char(27) || '[201~; DROP TABLE t; --' || char(27) || '[15~', 'line' || char(10) || 'two');",
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile).await;
    let value = "x\x1b[201~; DROP TABLE t; --\x1b[15~";
    app.on_key(key(KeyCode::Right));
    app.on_key(ctrl('c'));
    assert_eq!(crate::clipboard::sent().as_deref(), Some("x␛[201~; DROP TABLE t; --␛[15~"));
    assert_eq!(app.register.as_deref(), Some(value));
    assert!(toast(&app).contains("2 control character(s) are shown as symbols"), "{}", toast(&app));
    assert_eq!(app.toast.as_ref().unwrap().level, Level::Warning);
    // The viewer copies the same way.
    app.on_key(key(KeyCode::F(3)));
    app.on_key(key(KeyCode::Char('y')));
    assert_eq!(crate::clipboard::sent().as_deref(), Some("x␛[201~; DROP TABLE t; --␛[15~"));
    app.on_key(key(KeyCode::Esc));
    // Line breaks are text, not keys to escape.
    app.on_key(key(KeyCode::Right));
    app.on_key(ctrl('c'));
    assert_eq!(crate::clipboard::sent().as_deref(), Some("line\ntwo"));
    assert_eq!((toast(&app), app.toast.as_ref().unwrap().level), ("Copied “line\ntwo”", Level::Success));
    app.on_key(key(KeyCode::Left));
    app.register = Some(value.into());
    app.on_key(ctrl('v'));
    assert_eq!(table(&app).grid.pending_count(), 0, "pasting the value back changes nothing");
}

/// Audit L7: a value too large to keep is loaded cut, which the grid, the
/// viewer and a copy say; it is never edited or pasted as if whole, nor
/// used as a key. The viewer wraps it once, not on every frame.
#[tokio::test]
async fn large_values_are_cut_and_never_edited() {
    let dir = tempfile::tempdir().unwrap();
    let big = "replace(hex(zeroblob(300000)), '0', 'x')"; // 600,000 bytes
    let profile = sqlite_db(
        dir.path(),
        "t.db",
        &format!(
            "CREATE TABLE a_docs (id INTEGER PRIMARY KEY, body TEXT, note TEXT);
             INSERT INTO a_docs VALUES (1, {big}, 'n');
             CREATE TABLE b_keys (body TEXT PRIMARY KEY, note TEXT);
             INSERT INTO b_keys VALUES ({big}, 'n');"
        ),
    )
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(Config::default(), tx);
    connect_and_open(&mut app, &mut rx, profile.clone()).await;
    let screen = render(&mut app, 120, 30);
    assert!(screen.contains("1 large value(s) only partly loaded"), "{screen}");
    assert_eq!(table(&app).grid.rows[0][1].as_ref().unwrap().len(), crate::db::CELL_BYTES);
    // Results of the SQL editor too.
    app.on_key(key(KeyCode::Char('3')));
    type_str(&mut app, "SELECT body FROM a_docs");
    app.on_key(key(KeyCode::F(5)));
    wait_for(&mut app, &mut rx, |a| !a.browser.as_ref().unwrap().query.running).await;
    let message = &app.browser.as_ref().unwrap().query.message.as_ref().unwrap().0;
    assert!(message.contains("1 large value(s) only partly loaded"), "{message}");
    app.on_key(key(KeyCode::Tab));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(app.popup, Some(Popup::CellView { cut: Some(600_000), .. })));
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Char('1')));

    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::F(2)));
    assert!(app.popup.is_none());
    assert_eq!(toast(&app), "🔒 Only the first 256 KiB of this 585.9 KiB value were loaded: edit it with SQL");
    app.on_key(key(KeyCode::Enter)); // views it instead
    render(&mut app, 120, 30);
    let wrapped = |app: &App| match &app.popup {
        Some(Popup::CellView { wrapped: Some((_, lines)), .. }) => lines.as_ptr(),
        _ => panic!("expected the viewer"),
    };
    let first = wrapped(&app);
    app.on_key(key(KeyCode::End));
    let screen = render(&mut app, 120, 30);
    assert_eq!(wrapped(&app), first, "wrapped once");
    assert!(
        screen.contains("only the first 256 KiB of 585.9 KiB are shown") && screen.contains("… (not loaded)"),
        "{screen}"
    );
    app.on_key(key(KeyCode::Char('y')));
    assert!(toast(&app).starts_with("Copied the first 256 KiB of “xxx"), "{}", toast(&app));
    assert_eq!(crate::clipboard::sent().map(|s| s.len()), Some(crate::db::CELL_BYTES));
    assert_eq!(app.register, None, "a cut value is not pasted into cells");
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Right));
    app.on_key(ctrl('v'));
    assert_eq!(toast(&app), "Nothing to paste — copy a cell first");
    // Replacing it as a whole is fine.
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Backspace));
    app.on_key(ctrl('s'));
    app.on_key(key(KeyCode::Char('y')));
    wait_for(&mut app, &mut rx, |a| !table(a).loading && !table(a).grid.has_pending()).await;
    assert_eq!(rows_of(&profile, "SELECT body FROM a_docs").await, vec![vec![None]]);

    // A cut key cannot find its row.
    app.on_key(key(KeyCode::Esc));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    wait_for(&mut app, &mut rx, |a| table(a).info.table.name == "b_keys" && table(a).loaded && !table(a).loading).await;
    app.on_key(key(KeyCode::Right));
    app.on_key(key(KeyCode::Backspace));
    app.on_key(ctrl('s'));
    let Some(Popup::Message { body, .. }) = &app.popup else { panic!("expected an error") };
    assert!(body.contains("a value too large to load in full"), "{body}");
}

/// Audit L6: a panic in background work (here sqlx reading a row by the
/// columns of a table another program has just changed) ends the request
/// with an error, instead of leaving the table loading forever.
#[tokio::test]
async fn panics_in_background_work_end_the_request() {
    let (dir, mut app, mut rx) = open_people(KeymapMode::Standard, false).await;
    let mut other = ConnectionProfile::new(Driver::Sqlite);
    other.path = dir.path().join("t.db").to_string_lossy().into_owned();
    let other = Database::connect(&other, None).await.unwrap();
    other.run_query("ALTER TABLE people DROP COLUMN name", false).await.unwrap();
    other.close().await;
    app.on_key(ctrl('r'));
    assert!(table(&app).loading);
    wait_for(&mut app, &mut rx, |a| !table(a).loading).await;
    // The panic is in sqlx; should a later version not panic, the reload
    // simply works.
    if let Some(error) = &table(&app).error {
        assert!(error.starts_with("internal error: index out of bounds"), "{error}");
    }
    let screen = render(&mut app, 120, 30);
    assert!(!screen.contains("Loading"), "{screen}");
}
