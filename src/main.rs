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
    --read-only      open FILE.db read-only
    -h, --help       show this help
    -V, --version    show the version

Configuration lives in the platform config directory (override with
DBTUI_CONFIG_DIR). Passwords are kept in the OS keyring or an encrypted vault,
never in the config file.";

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

#[tokio::main]
async fn main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };
    let mut config = Config::load().context("loading configuration")?;
    if let Some(mode) = args.keymap {
        config.keymap = mode;
        config.save().ok();
    }

    // Resolve what to open at start-up before touching the terminal.
    let startup = match &args.target {
        None => None,
        Some(target) => match config.connections.iter().find(|p| p.name == *target) {
            Some(p) => Some(p.clone()),
            None if std::path::Path::new(target).is_file() => {
                let mut p = ConnectionProfile::new(Driver::Sqlite);
                p.name = std::path::Path::new(target)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.clone());
                p.path = std::fs::canonicalize(target)?.to_string_lossy().into_owned();
                p.read_only = args.read_only;
                Some(p)
            }
            None => anyhow::bail!("no saved connection or file named {target:?}"),
        },
    };

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
    if let Some(b) = &app.browser {
        b.db.close().await;
    }
    Ok(())
}
