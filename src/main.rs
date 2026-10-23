//! dbtui — a terminal UI for exploring and editing SQL databases.

mod app;
mod clipboard;
mod config;
mod db;
mod keymap;
mod secrets;
mod terminal;
mod ui;
mod util;

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::app::App;
use crate::config::{Config, ConnectionProfile, KeymapMode};
use crate::db::Driver;

const USAGE: &str = "\
dbtui — explore and edit PostgreSQL, MySQL/MariaDB and SQLite databases

USAGE:
    dbtui [OPTIONS] [CONNECTION | FILE.db]

ARGS:
    CONNECTION       name of a saved connection to open immediately
    FILE.db          open a SQLite database file directly

OPTIONS:
    --vim            use vim-style keys (saved as your preference)
    --standard       use standard keys (saved as your preference)
    --read-only      make every connection opened in this run read-only
    -h, --help       show this help
    -V, --version    show the version

Configuration lives in the platform config directory (override with
DBTUI_CONFIG_DIR). Passwords are kept in the OS keyring or an encrypted vault,
never in the config file. Connection settings come from the profile only:
PG* environment variables (PGPASSWORD, PGSSLCERT, PGOPTIONS...) are ignored.";

struct Args {
    keymap: Option<KeymapMode>,
    read_only: bool,
    target: Option<String>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args { keymap: None, read_only: false, target: None };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("dbtui {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--vim" => args.keymap = Some(KeymapMode::Vim),
            "--standard" => args.keymap = Some(KeymapMode::Standard),
            "--read-only" => args.read_only = true,
            s if s.starts_with('-') => anyhow::bail!("unknown option {s}\n\n{USAGE}"),
            s => args.target = Some(s.to_string()),
        }
    }
    Ok(Some(args))
}

fn main() -> Result<ExitCode> {
    // Changing the environment is only sound while no other thread can read
    // it, so this happens before the async runtime starts its threads.
    // SAFETY: nothing has spawned a thread yet.
    unsafe { db::clear_client_environment() };
    disable_core_dumps();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(async_main());
    // Work still running in the background (a keyring read waiting for its
    // unlock prompt, a query) must not keep dbtui from exiting.
    runtime.shutdown_timeout(Duration::from_millis(500));
    result
}

/// A core dump would contain the passwords and the vault key in memory.
fn disable_core_dumps() {
    #[cfg(unix)]
    {
        let limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: plain system call on a valid struct. Lowering the limit
        // cannot fail for lack of privileges.
        unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) };
    }
}

async fn async_main() -> Result<ExitCode> {
    let Some(args) = parse_args()? else { return Ok(ExitCode::SUCCESS) };
    let mut config = Config::load().context("loading configuration")?;
    if let Some(mode) = args.keymap {
        match Config::update(|c| c.keymap = mode) {
            Ok(saved) => config = saved,
            Err(_) => config.keymap = mode,
        }
    }

    // Resolve what to open at start-up before touching the terminal.
    let startup = startup_profile(&config, &args)?;

    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(config, tx);
    app.read_only_run = args.read_only;
    if let Some(profile) = startup {
        app.start(profile);
    }

    let mut signals = terminal::Signals::new().context("listening for signals")?;
    let mut terminal = terminal::init().context("setting up the terminal")?;
    let result = run(&mut terminal, &mut app, rx, &mut signals).await;
    terminal::restore();
    let mut exit = result?;
    // Closing the connection waits for what still runs on it (a query, a
    // commit): only for so long, with the terminal already given back, and
    // a signal still ends dbtui meanwhile.
    if let (Exit::Quit, Some(b)) = (&exit, app.browser.take())
        && let Err(stop) = signals.run_until(close_within(&b.db, CLOSE_TIMEOUT)).await
    {
        exit = Exit::Stopped(stop);
    }
    match exit {
        Exit::Quit => Ok(ExitCode::SUCCESS),
        Exit::Stopped(stop) => {
            // The terminal may be gone (SIGHUP): never fail over this.
            let _ = writeln!(std::io::stderr(), "dbtui: stopped by {}", stop.name);
            Ok(ExitCode::from(stop.exit_code()))
        }
    }
}

/// How long quitting waits for the connection to close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Closes `db`, waiting at most `limit` for work still running on it to end
/// (`false` if it had not). Past that, its connections are dropped with the
/// process, and the server rolls back what they had not committed.
async fn close_within(db: &db::Database, limit: Duration) -> bool {
    tokio::time::timeout(limit, db.close()).await.is_ok()
}

/// How the UI loop ended.
enum Exit {
    Quit,
    /// A signal asked dbtui to stop: it does so at once.
    Stopped(terminal::Stop),
}

/// The saved connection or SQLite file named on the command line.
fn startup_profile(config: &Config, args: &Args) -> Result<Option<ConnectionProfile>> {
    let Some(target) = &args.target else { return Ok(None) };
    let profile = match config.connections.iter().find(|p| p.name == *target) {
        Some(p) => p.clone(),
        None if std::path::Path::new(target).is_file() => {
            let mut p = ConnectionProfile::new(Driver::Sqlite);
            p.name = std::path::Path::new(target)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.clone());
            p.path = std::fs::canonicalize(target)?.to_string_lossy().into_owned();
            p
        }
        None => anyhow::bail!("no saved connection or file named {target:?}"),
    };
    // `--read-only` is not applied here: it applies to every connection of
    // the run (see `App::read_only_run`), this one included.
    Ok(Some(profile))
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mut rx: mpsc::UnboundedReceiver<app::AppEvent>,
    signals: &mut terminal::Signals,
) -> Result<Exit> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => app.on_key(key),
                Some(Ok(Event::Paste(text))) => app.on_paste(text),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(event) = rx.recv() => app.on_app_event(event),
            _ = tick.tick() => app.on_tick(),
            stop = signals.recv() => return Ok(Exit::Stopped(stop)),
        }
        if app.should_quit {
            break;
        }
    }
    // Saving a password in the keyring normally takes a moment: let it
    // finish and be recorded (or cleaned up), unless the keyring is stuck.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while app.keyring_busy() {
        app.busy = Some("Finishing with the OS keyring".into());
        terminal.draw(|frame| ui::draw(frame, app))?;
        match signals.run_until(tokio::time::timeout_at(deadline, rx.recv())).await {
            Err(stop) => return Ok(Exit::Stopped(stop)),
            Ok(Ok(Some(event))) => app.on_app_event(event),
            Ok(_) => break,
        }
    }
    Ok(Exit::Quit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: Option<&str>, read_only: bool) -> Args {
        Args { keymap: None, read_only, target: target.map(Into::into) }
    }

    /// Quitting does not hang on a connection still in use (a long query, a
    /// commit waiting for a lock): closing it gives up after a while.
    #[tokio::test]
    async fn closing_does_not_wait_for_running_work() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.db");
        std::fs::File::create(&file).unwrap();
        let mut profile = ConnectionProfile::new(Driver::Sqlite);
        profile.path = file.to_string_lossy().into_owned();
        let db = db::Database::connect(&profile, None).await.unwrap();
        let db::Database::Sqlite(pool, _) = &db else { unreachable!() };
        let busy = pool.acquire().await.unwrap();
        let started = std::time::Instant::now();
        assert!(!close_within(&db, Duration::from_millis(200)).await);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(busy);
        assert!(close_within(&db, Duration::from_secs(5)).await);
        assert!(db.is_closed());
    }

    /// The target is a saved connection, by name, or a SQLite file. Saved
    /// profiles are opened as saved: `--read-only` applies to the whole run
    /// (see `read_only_runs_stay_read_only` in the app tests).
    #[test]
    fn startup_targets() {
        let mut config = Config::default();
        let mut saved = ConnectionProfile::new(Driver::Postgres);
        saved.name = "prod".into();
        config.connections.push(saved.clone());
        saved.name = "locked".into();
        saved.read_only = true;
        config.connections.push(saved);

        let open = |target, read_only| startup_profile(&config, &args(Some(target), read_only)).unwrap().unwrap();
        assert_eq!(open("prod", true), config.connections[0]);
        assert!(open("locked", false).read_only);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.db");
        std::fs::File::create(&file).unwrap();
        let file = file.to_string_lossy().into_owned();
        let p = open(&file, true);
        assert_eq!((p.driver, p.name.as_str()), (Driver::Sqlite, "app.db"));

        assert!(startup_profile(&config, &args(None, false)).unwrap().is_none());
        assert!(startup_profile(&config, &args(None, true)).unwrap().is_none());
        assert!(startup_profile(&config, &args(Some("missing"), true)).is_err());
    }
}
