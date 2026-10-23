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
use crate::db::Transport;
use crate::db::transport_tests::fake_pg_server;
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
    let connected = |profile: &ConnectionProfile, remember| AppEvent::Connected {
        profile: profile.clone(),
        db: browser.db.clone(),
        session: Box::new(browser.session.clone()),
        schemas: browser.schemas.clone(),
        remember: Some(remember),
    };
    for (profile, password) in [(&in_vault, "vault-password"), (&in_keyring, "keyring-password")] {
        // What `connect_with_saved` asks for with an unbound password.
        let remember = app.remember(profile, Zeroizing::new(password.into()), true);
        app.on_app_event(connected(profile, remember));
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
    app.on_app_event(connected(&in_vault, remember));
    // Deleted while connecting.
    let remember = app.remember(&in_keyring, Zeroizing::new("typed".into()), false);
    app.confirm(ConfirmAction::DeleteProfile(in_keyring.id.clone()));
    app.on_app_event(connected(&in_keyring, remember));
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
