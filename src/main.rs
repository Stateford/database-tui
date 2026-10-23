//! dbtui — a terminal UI for exploring and editing SQL databases.

mod app;
mod clipboard;
mod config;
mod db;
mod keymap;
mod secrets;
mod ui;
mod util;

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind};
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
    --read-only      open the CONNECTION or FILE.db read-only
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

fn main() -> Result<()> {
    // Changing the environment is only sound while no other thread can read
    // it, so this happens before the async runtime starts its threads.
    // SAFETY: nothing has spawned a thread yet.
    unsafe { db::clear_client_environment() };
    disable_core_dumps();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async_main())
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

async fn async_main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
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
    if let Some(profile) = startup {
        app.start(profile);
    }

    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste).ok();
    let result = run(&mut terminal, app, rx).await;
    crossterm::execute!(std::io::stdout(), DisableBracketedPaste).ok();
    ratatui::restore();
    result
}

/// The saved connection or SQLite file named on the command line.
fn startup_profile(config: &Config, args: &Args) -> Result<Option<ConnectionProfile>> {
    let Some(target) = &args.target else {
        if args.read_only {
            // Don't let the flag look like it protects connections opened later.
            anyhow::bail!("--read-only needs a CONNECTION or FILE.db to open\n\n{USAGE}");
        }
        return Ok(None);
    };
    let mut profile = match config.connections.iter().find(|p| p.name == *target) {
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
    // The flag can only make a saved profile stricter, never looser.
    profile.read_only |= args.read_only;
    Ok(Some(profile))
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut rx: mpsc::UnboundedReceiver<app::AppEvent>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
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
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(event)) => app.on_app_event(event),
            _ => break,
        }
    }
    if let Some(b) = &app.browser {
        b.db.close().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: Option<&str>, read_only: bool) -> Args {
        Args { keymap: None, read_only, target: target.map(Into::into) }
    }

    #[test]
    fn read_only_flag_applies_to_saved_connections_and_files() {
        let mut config = Config::default();
        let mut saved = ConnectionProfile::new(Driver::Postgres);
        saved.name = "prod".into();
        config.connections.push(saved.clone());
        saved.name = "locked".into();
        saved.read_only = true;
        config.connections.push(saved);

        let open = |target, read_only| startup_profile(&config, &args(Some(target), read_only)).unwrap().unwrap();
        assert!(open("prod", true).read_only);
        assert!(!open("prod", false).read_only);
        // A profile marked read-only stays read-only without the flag.
        assert!(open("locked", false).read_only);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.db");
        std::fs::File::create(&file).unwrap();
        let file = file.to_string_lossy().into_owned();
        let p = open(&file, true);
        assert_eq!((p.driver, p.name.as_str(), p.read_only), (Driver::Sqlite, "app.db", true));
        assert!(!open(&file, false).read_only);

        assert!(startup_profile(&config, &args(None, false)).unwrap().is_none());
        assert!(startup_profile(&config, &args(None, true)).is_err());
        assert!(startup_profile(&config, &args(Some("missing"), true)).is_err());
    }
}
