//! Application state and event handling.

pub mod browser;
pub mod form;
pub mod grid;
pub mod input;
pub mod query;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::TextArea;
use tokio::sync::mpsc::UnboundedSender;
use zeroize::Zeroizing;

use self::browser::{Browser, CommitSnapshot, Committing, Focus, OpenTable, Tab, TreeItem};
use self::form::{ConnectionForm, FormOutcome};
use self::grid::{Grid, RowKey};
use self::input::Input;
use self::query::{EditorFocus, EditorOutcome, VimMode};
use crate::config::{Config, ConnectionProfile, CredentialStore, KeymapMode, SecretBackend};
use crate::db::{
    Database, Driver, PageRequest, ResultSet, SchemaInfo, SessionInfo, Statement, TableRef, TableStructure, sql,
};
use crate::keymap::{Action, Context, Keymap};
use crate::secrets::{Endpoint, KEYRING_TIMEOUT, Keyring, StoredSecret, Vault, VaultChanged, VaultExists};

/// Results of background work, delivered to the UI loop.
///
/// Each carries what it was requested for: the connection attempt
/// (`attempt`), or the connection ([`Browser::generation`]) and the request
/// (`seq`). Results of attempts or requests made since are dropped.
pub enum AppEvent {
    Connected {
        attempt: u64,
        profile: ConnectionProfile,
        db: Database,
        session: Box<SessionInfo>,
        schemas: Vec<SchemaInfo>,
    },
    /// The password saved in the keyring for a profile being connected.
    SavedPassword {
        attempt: u64,
        profile: ConnectionProfile,
        result: Result<Option<StoredSecret>, String>,
    },
    ConnectFailed {
        attempt: u64,
        profile: ConnectionProfile,
        error: String,
        needs_password: bool,
    },
    /// The level is a warning when the server was not authenticated.
    TestResult(Result<(Level, String), String>),
    Schemas {
        generation: u64,
        seq: u64,
        result: Result<Vec<SchemaInfo>, String>,
    },
    Page {
        generation: u64,
        seq: u64,
        structure: Option<Result<TableStructure, String>>,
        data: Result<ResultSet, String>,
    },
    Count {
        generation: u64,
        seq: u64,
        count: Result<u64, String>,
    },
    /// `may_write`: the SQL is not plainly read-only.
    Query {
        generation: u64,
        seq: u64,
        may_write: bool,
        result: Result<ResultSet, String>,
    },
    /// `profile`: the name of the connection, which may be closed by now.
    Committed {
        generation: u64,
        seq: u64,
        profile: String,
        result: Result<u64, String>,
    },
    /// A keyring write finished, possibly long after it was requested.
    KeyringStored(SaveRequest, Result<(), String>),
    /// `report`: tell the user if it failed (the keyring was known to hold
    /// the password, rather than possibly holding an old copy).
    KeyringRemoved {
        op: u64,
        report: bool,
        result: Result<(), String>,
    },
    /// The vault prompt's master password was checked in the background
    /// (`create`: a new vault was made with it).
    VaultOpened {
        seq: u64,
        create: bool,
        result: anyhow::Result<Vault>,
    },
}

/// A password to save for a profile, once its connection has worked.
pub struct Remember {
    pub password: Zeroizing<String>,
    /// Save again, bound to its server and where it already is, a password
    /// saved before passwords were bound; rather than one the user typed,
    /// which goes to the configured backend.
    pub rebind: bool,
    /// The profile's credential generation when connecting (see
    /// [`CredentialOps`]): if it has changed since, the password was saved
    /// or forgotten in the meantime and this one is not kept.
    pub generation: u64,
}

/// A password being saved for a profile.
pub struct SaveRequest {
    pub id: String,
    pub endpoint: Endpoint,
    pub password: Zeroizing<String>,
    /// The credential generation this save belongs to.
    pub generation: u64,
    /// Re-saving a password that is already saved: no message on success.
    pub quiet: bool,
}

/// Bookkeeping that lets results of background keyring work be matched to
/// what the user did since: a profile deleted, its password saved again or
/// no longer saved. Such late results are cleaned up, not recorded.
#[derive(Default)]
struct CredentialOps {
    counter: u64,
    /// Per profile id, the generation of its latest password change (saved,
    /// re-saved or forgotten). A result from an older generation is stale.
    generation: HashMap<String, u64>,
    /// Per profile id, the last keyring write or delete queued. The keyring
    /// runs them in order, so that one decides what the keyring ends up
    /// holding for the profile.
    last_keyring_op: HashMap<String, u64>,
    /// Keyring operations not answered yet: when they started, and whether
    /// the user has been told that the keyring is slow (deletes are not
    /// worth telling about).
    pending: HashMap<u64, (Instant, bool)>,
}

impl CredentialOps {
    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// Starts a new generation for a profile's password.
    fn bump(&mut self, id: &str) -> u64 {
        let generation = self.next();
        self.generation.insert(id.to_string(), generation);
        generation
    }

    fn generation(&self, id: &str) -> u64 {
        self.generation.get(id).copied().unwrap_or(0)
    }

    /// Whether the profile's password is being saved to the keyring.
    fn saving(&self, id: &str) -> bool {
        self.pending.contains_key(&self.generation(id))
    }

    /// Whether anything about the profile's password happened this session.
    fn knows(&self, id: &str) -> bool {
        self.generation.contains_key(id) || self.last_keyring_op.contains_key(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Connections,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Toast {
    pub text: String,
    pub level: Level,
    pub until: Instant,
}

pub enum ConfirmAction {
    DeleteProfile(String),
    Disconnect,
    Quit,
    DiscardAndOpen(TableRef),
    DiscardEdits,
    /// Connect with a saved password the user is asked about first (see
    /// [`App::send_saved_password`]).
    SendSavedPassword(Box<SavedPassword>),
}

impl ConfirmAction {
    /// Whether Enter confirms, as well as `y`. Not for sending a password:
    /// a second Enter after the one that asked to connect must not do it.
    pub fn enter_confirms(&self) -> bool {
        !matches!(self, ConfirmAction::SendSavedPassword(_))
    }
}

/// A saved password waiting for the user's go-ahead.
pub struct SavedPassword {
    pub profile: ConnectionProfile,
    pub password: Zeroizing<String>,
    /// Saved before passwords were bound to their server: bound once it works.
    pub unbound: bool,
}

pub enum AfterUnlock {
    Connect(ConnectionProfile),
    Store(SaveRequest),
}

pub struct PasswordPrompt {
    pub profile: ConnectionProfile,
    pub input: Input,
    pub remember: bool,
    pub focus_remember: bool,
    pub error: Option<String>,
}

pub struct VaultPrompt {
    pub create: bool,
    pub input: Input,
    pub confirm: Input,
    pub focus: usize,
    pub error: Option<String>,
    pub then: AfterUnlock,
    /// While the master password is checked in the background (deriving the
    /// key takes a moment): the `seq` of that request.
    pub working: Option<u64>,
}

/// The row a cell editor edits.
pub enum EditTarget {
    /// A fetched row, by its key: wherever it is when the edit is applied.
    Row(RowKey),
    /// A new row staged in the grid ([`Grid::id`]), by position.
    NewRow { grid: u64, index: usize },
}

/// Edits one cell. It remembers the cell by connection, table, row key and
/// column name, not by position: the edit is staged on that cell, wherever
/// it is by then, or not at all if it is gone.
pub struct CellEditor {
    pub editor: TextArea<'static>,
    pub generation: u64,
    pub table: TableRef,
    pub target: EditTarget,
    pub column: String,
    /// The value when the editor opened (`None`: NULL or the default).
    pub original: Option<String>,
    /// How the lines are joined: CRLF when the value used it throughout,
    /// so that the value comes back exactly as it was.
    pub newline: &'static str,
    /// Whether the text was changed at all.
    pub modified: bool,
    pub data_type: String,
    pub nullable: bool,
    pub new_row: bool,
    /// The value holds characters the editor cannot show (see
    /// [`crate::util::has_hidden`]); the viewer shows them.
    pub hidden: bool,
}

impl CellEditor {
    pub fn value(&self) -> String {
        self.editor.lines().join(self.newline)
    }

    /// Whether confirming `value` would change nothing: the same text, or a
    /// NULL (or default) left untouched.
    fn unchanged(&self, value: &str) -> bool {
        match &self.original {
            Some(original) => original == value,
            None => !self.modified,
        }
    }
}

/// Splits a value into editor lines that join back into exactly that value
/// (`str::lines` would drop `\r` and a final newline).
fn editor_lines(text: &str) -> (Vec<String>, &'static str) {
    let crlf = text.contains("\r\n") && text.matches('\n').count() == text.matches("\r\n").count();
    let newline = if crlf { "\r\n" } else { "\n" };
    (text.split(newline).map(String::from).collect(), newline)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Filter,
    TreeFilter,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub hint: String,
    pub input: Input,
}

pub enum Popup {
    Help {
        scroll: u16,
    },
    Confirm {
        title: String,
        message: String,
        action: ConfirmAction,
    },
    Message {
        title: String,
        body: String,
        error: bool,
        scroll: u16,
    },
    Form(Box<ConnectionForm>),
    Password(PasswordPrompt),
    Vault(VaultPrompt),
    CellEditor(Box<CellEditor>),
    // `text` is what is shown: the value made safe to display (JSON
    // re-indented); `value` is the cell's value, which is what gets copied
    // (`None`: NULL or the default); `cut`: only the start of the value was
    // loaded, and the size of all of it; `binary`: the value is bytes shown
    // as `0x…`. `wrapped`: `text` wrapped to a width, kept until the width
    // changes, as a large value takes a while.
    CellView {
        title: String,
        text: String,
        value: Option<String>,
        cut: Option<u64>,
        binary: bool,
        scroll: usize,
        wrapped: Option<(usize, Vec<String>)>,
    },
    Prompt(Prompt),
    // `previews`: the statements as shown (see `commit_preview`); `hidden`:
    // they hold characters that would not show as themselves;
    // `non_transactional`: engine of a MySQL table that cannot roll back;
    // `snapshot`: what the statements were built from (see `CommitSnapshot`).
    Commit {
        statements: Vec<Statement>,
        previews: Vec<String>,
        hidden: bool,
        scroll: u16,
        non_transactional: Option<String>,
        snapshot: Box<CommitSnapshot>,
    },
}

/// Most bytes of a statement shown in the commit preview: its values can be
/// large (a pasted document, say).
const PREVIEW_BYTES: usize = 4096;

/// A statement as the commit preview shows it: made safe to display, and
/// cut if very long.
fn commit_preview(statement: &Statement) -> String {
    let full = statement.preview();
    match crate::util::clip(&full, PREVIEW_BYTES) {
        (all, false) => crate::util::display_safe(all),
        (start, true) => format!(
            "{} … ({} more)",
            crate::util::display_safe(start),
            crate::util::size((full.len() - start.len()) as u64)
        ),
    }
}

/// A change a cell editor stages.
enum CellChange {
    Set(Option<String>),
    /// Back to the column default (new rows only).
    Default,
}

/// Several statements on a table that cannot roll back are only committed
/// after an explicit `!`: if one fails, the ones before it stay applied.
pub fn commit_needs_force(statements: &[Statement], non_transactional: Option<&String>) -> bool {
    non_transactional.is_some() && statements.len() > 1
}

pub struct App {
    pub config: Config,
    pub screen: Screen,
    pub selected_conn: usize,
    pub browser: Option<Browser>,
    pub popup: Option<Popup>,
    /// Vim `:` command line, when open.
    pub command: Option<Input>,
    pub toast: Option<Toast>,
    pub keymap: Keymap,
    pub vault: Option<Vault>,
    keyring: Keyring,
    credentials: CredentialOps,
    /// Last vault prompt request (see [`VaultPrompt::working`]).
    vault_seq: u64,
    /// Id of the latest connection attempt, and that attempt while it is
    /// not over: the result of any other one is discarded.
    connect_seq: u64,
    connecting: Option<u64>,
    /// The password to save once the attempt in `connecting` works.
    to_remember: Option<Remember>,
    /// `--read-only`: every connection opened in this run is read-only.
    pub read_only_run: bool,
    /// Where `config.toml` and `vault.toml` live.
    config_dir: Option<PathBuf>,
    pub busy: Option<String>,
    pub tick: usize,
    pub should_quit: bool,
    /// Internal register for copy / paste of cell values.
    pub register: Option<String>,
    tx: UnboundedSender<AppEvent>,
}

/// Formats an error with its causes, skipping causes whose text is already
/// part of the message (sqlx repeats the driver error in its source).
fn err_text(e: anyhow::Error) -> String {
    let mut out = String::new();
    for cause in e.chain() {
        let text = cause.to_string();
        if !out.contains(&text) {
            if !out.is_empty() {
                out.push_str(": ");
            }
            out.push_str(&text);
        }
    }
    out
}

/// A failed connection's error, with what to do about it when the server's
/// certificate was refused.
fn connect_error(e: anyhow::Error) -> String {
    with_certificate_hint(err_text(e), "\n\n")
}

fn with_certificate_hint(error: String, separator: &str) -> String {
    match certificate_hint(&error) {
        Some(hint) => format!("{error}{separator}{hint}"),
        None => error,
    }
}

/// What to do when the server's certificate could not be verified: a
/// remote profile verifies it by default (verify-full), including one saved
/// without an SSL mode, which earlier versions connected with `prefer`.
fn certificate_hint(error: &str) -> Option<&'static str> {
    if !error.contains("invalid peer certificate") {
        return None;
    }
    Some(if error.contains("UnknownIssuer") {
        "The server's certificate is not from a CA dbtui trusts. If the server uses a private CA (a company CA, \
         AWS RDS, Cloud SQL, MySQL's self-generated certificates), set CA file (ssl_ca) in the connection form to \
         that CA's PEM file. Choose a weaker SSL mode only if you accept that the server is not verified."
    } else if error.contains("not valid for name") || error.contains("NotValidForName") {
        "The certificate was issued for another host name: connect with a name it is valid for."
    } else {
        "The server's certificate could not be verified (SSL mode verify-full): check the host name and the CA \
         file (ssl_ca) in the connection form."
    })
}

/// Runs background work whose result the app waits for, turning a panic in
/// it (a bug in a database driver, say) into an error: the request then
/// ends like a failed one, rather than leaving the app loading forever.
async fn guarded<T>(work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    crate::terminal::catch_panic(work).await.unwrap_or_else(|panic| Err(format!("internal error: {panic}")))
}

impl App {
    pub fn new(config: Config, tx: UnboundedSender<AppEvent>) -> Self {
        Self {
            config,
            screen: Screen::Connections,
            selected_conn: 0,
            browser: None,
            popup: None,
            command: None,
            toast: None,
            keymap: Keymap::default(),
            vault: None,
            keyring: Keyring::os(),
            credentials: CredentialOps::default(),
            vault_seq: 0,
            connect_seq: 0,
            connecting: None,
            to_remember: None,
            read_only_run: false,
            config_dir: crate::config::config_dir().ok(),
            busy: None,
            tick: 0,
            should_quit: false,
            register: None,
            tx,
        }
    }

    pub fn keymap_mode(&self) -> KeymapMode {
        self.config.keymap
    }

    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let secs = match level {
            Level::Error => 8,
            Level::Warning => 5,
            _ => 3,
        };
        self.toast = Some(Toast { text: text.into(), level, until: Instant::now() + Duration::from_secs(secs) });
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.toast = None;
        }
        // A panic on a thread of its own (a database driver's, say): the
        // work it was doing fails or is retried, and the app goes on.
        if let Some(panic) = crate::terminal::take_background_panic() {
            self.notify(Level::Error, format!("Internal error in a background thread: {panic}"));
        }
        // A slow keyring is not a failure: the write completes (or fails)
        // whenever the keyring answers, and is handled then.
        let mut slow = false;
        for (started, told) in self.credentials.pending.values_mut() {
            if !*told && started.elapsed() >= KEYRING_TIMEOUT {
                *told = true;
                slow = true;
            }
        }
        if slow {
            self.notify(
                Level::Warning,
                "The OS keyring is not answering (is it locked?). The password will be saved there once it does, \
                 and is not saved anywhere else meanwhile.",
            );
        }
    }

    fn config_path(&self) -> anyhow::Result<PathBuf> {
        let dir = self.config_dir.as_ref().context("could not determine a configuration directory")?;
        Ok(dir.join("config.toml"))
    }

    fn vault_path(&self) -> anyhow::Result<PathBuf> {
        let dir = self.config_dir.as_ref().context("could not determine a configuration directory")?;
        Ok(dir.join("vault.toml"))
    }

    /// Changes the configuration and saves it. The change is made to the
    /// file as it is on disk, which another dbtui instance may have changed
    /// meanwhile, and the result becomes this app's configuration.
    fn update_config(&mut self, mut change: impl FnMut(&mut Config)) {
        match self.config_path().and_then(|path| Config::update_at(&path, &mut change)) {
            Ok(config) => self.config = config,
            Err(e) => {
                change(&mut self.config);
                self.notify(Level::Error, format!("Could not save configuration: {}", err_text(e)));
            }
        }
        self.selected_conn = self.selected_conn.min(self.config.connections.len().saturating_sub(1));
    }

    fn profile(&self, id: &str) -> Option<&ConnectionProfile> {
        self.config.connections.iter().find(|p| p.id == id)
    }

    fn set_keymap(&mut self, mode: KeymapMode) {
        self.keymap.clear();
        if let Some(b) = &mut self.browser {
            b.query.vim_mode = VimMode::Normal;
        }
        self.update_config(|c| c.keymap = mode);
        let how = match mode {
            KeymapMode::Vim => "Vim keys enabled (hjkl, gg/G, dd, :w …)",
            KeymapMode::Standard => "Standard keys enabled (arrows, Enter, Del, Ctrl-S …)",
        };
        self.notify(Level::Info, how);
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    pub fn on_paste(&mut self, text: String) {
        // It may be a password: wiped once used.
        let text = Zeroizing::new(text);
        match &mut self.popup {
            Some(Popup::Form(form)) => form.paste(&text),
            Some(Popup::Password(p)) => p.input.insert_str(text.lines().next().unwrap_or_default()),
            Some(Popup::Vault(v)) if v.working.is_none() => {
                let target = if v.focus == 0 { &mut v.input } else { &mut v.confirm };
                target.insert_str(text.lines().next().unwrap_or_default());
            }
            Some(Popup::Prompt(p)) => p.input.insert_str(&text.replace('\n', " ")),
            Some(Popup::CellEditor(c)) => {
                c.modified |= c.editor.insert_str(text.as_str());
            }
            Some(_) => {}
            None => {
                if let Some(cmd) = &mut self.command {
                    cmd.insert_str(&text.replace('\n', " "));
                } else if let Some(b) = &mut self.browser
                    && b.tab == Tab::Query
                    && b.focus == Focus::Main
                    && b.query.focus == EditorFocus::Editor
                {
                    b.query.editor.insert_str(text.as_str());
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.popup.is_some() {
            self.popup_key(key);
            return;
        }
        if self.command.is_some() {
            self.command_key(key);
            return;
        }
        match self.screen {
            Screen::Connections => self.connections_key(key),
            Screen::Browser => self.browser_key(key),
        }
    }

    fn command_key(&mut self, key: KeyEvent) {
        let Some(cmd) = &mut self.command else { return };
        match key.code {
            KeyCode::Esc => self.command = None,
            KeyCode::Enter => {
                let text = cmd.value().trim().to_string();
                self.command = None;
                self.run_command(&text);
            }
            KeyCode::Backspace if cmd.value().is_empty() => self.command = None,
            _ => {
                cmd.handle_key(key);
            }
        }
    }

    fn connections_key(&mut self, key: KeyEvent) {
        let Some(action) = self.keymap.resolve(self.keymap_mode(), Context::Connections, key) else { return };
        let n = self.config.connections.len();
        match action {
            Action::Up => self.selected_conn = self.selected_conn.saturating_sub(1),
            Action::Down => self.selected_conn = (self.selected_conn + 1).min(n.saturating_sub(1)),
            Action::Top => self.selected_conn = 0,
            Action::Bottom => self.selected_conn = n.saturating_sub(1),
            Action::Select | Action::Right => self.connect_selected(),
            Action::New => {
                let profile = ConnectionProfile::new(Driver::Postgres);
                self.popup = Some(Popup::Form(Box::new(ConnectionForm::new(profile, true))));
            }
            Action::EditItem => {
                if let Some(p) = self.config.connections.get(self.selected_conn).cloned() {
                    let mut form = ConnectionForm::new(p, false);
                    // Its password is still on its way to the keyring.
                    form.save_password |= self.credentials.saving(&form.original.id);
                    self.popup = Some(Popup::Form(Box::new(form)));
                }
            }
            Action::DeleteItem => {
                if let Some(p) = self.config.connections.get(self.selected_conn) {
                    self.popup = Some(Popup::Confirm {
                        title: "Delete connection".into(),
                        message: format!("Delete \"{}\" and its saved password?", p.name),
                        action: ConfirmAction::DeleteProfile(p.id.clone()),
                    });
                }
            }
            Action::Quit | Action::Back => self.should_quit = true,
            Action::Help => self.popup = Some(Popup::Help { scroll: 0 }),
            Action::ToggleKeymap => self.set_keymap(self.keymap_mode().toggled()),
            Action::CommandLine if self.keymap_mode() == KeymapMode::Vim => self.command = Some(Input::default()),
            _ => {}
        }
    }

    fn browser_key(&mut self, key: KeyEvent) {
        let mode = self.keymap_mode();
        let Some(b) = &mut self.browser else { return };

        // The SQL editor consumes most keys itself.
        if b.focus == Focus::Main && b.tab == Tab::Query && b.query.focus == EditorFocus::Editor {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if key.code == KeyCode::F(5) || (ctrl && key.code == KeyCode::Char('e')) {
                self.run_query();
                return;
            }
            match b.query.handle_key(key, mode) {
                EditorOutcome::Handled => {}
                EditorOutcome::CommandLine => self.command = Some(Input::default()),
                EditorOutcome::Action(action) => self.browser_action(action),
            }
            return;
        }

        let ctx = match (b.focus, b.tab) {
            (Focus::Sidebar, _) => Context::Sidebar,
            (Focus::Main, Tab::Structure) => Context::Structure,
            (Focus::Main, _) => Context::Grid,
        };
        if b.tab == Tab::Query && b.focus == Focus::Main && key.code == KeyCode::F(5) {
            self.run_query();
            return;
        }
        if let Some(action) = self.keymap.resolve(mode, ctx, key) {
            self.browser_action(action);
        }
    }

    fn browser_action(&mut self, action: Action) {
        let Some(b) = &mut self.browser else { return };
        // Actions available everywhere in the browser.
        match action {
            Action::Quit => return self.request_quit(),
            Action::Help => {
                self.popup = Some(Popup::Help { scroll: 0 });
                return;
            }
            Action::ToggleKeymap => return self.set_keymap(self.keymap_mode().toggled()),
            Action::CommandLine => {
                self.command = Some(Input::default());
                return;
            }
            Action::TabData | Action::TabStructure | Action::TabQuery | Action::NextTab | Action::PrevTab => {
                let idx = b.tab.index();
                b.tab = match action {
                    Action::TabData => Tab::Data,
                    Action::TabStructure => Tab::Structure,
                    Action::TabQuery => Tab::Query,
                    Action::NextTab => Tab::ALL[(idx + 1) % 3],
                    _ => Tab::ALL[(idx + 2) % 3],
                };
                b.focus = Focus::Main;
                if b.tab == Tab::Query {
                    b.query.focus = EditorFocus::Editor;
                }
                return;
            }
            Action::FocusNext | Action::FocusPrev => {
                // Sidebar -> main (-> results on the SQL tab) -> sidebar.
                let forward = action == Action::FocusNext;
                match (b.focus, b.tab, b.query.focus) {
                    (Focus::Sidebar, Tab::Query, _) => {
                        b.focus = Focus::Main;
                        b.query.focus = if forward { EditorFocus::Editor } else { EditorFocus::Results };
                    }
                    (Focus::Sidebar, _, _) => b.focus = Focus::Main,
                    (Focus::Main, Tab::Query, EditorFocus::Editor) if forward => b.query.focus = EditorFocus::Results,
                    (Focus::Main, Tab::Query, EditorFocus::Results) if !forward => b.query.focus = EditorFocus::Editor,
                    (Focus::Main, _, _) => b.focus = Focus::Sidebar,
                }
                return;
            }
            Action::FocusSidebar => {
                b.focus = Focus::Sidebar;
                return;
            }
            Action::FocusMain => {
                b.focus = Focus::Main;
                return;
            }
            Action::Commit => return self.begin_commit(),
            _ => {}
        }
        match b.focus {
            Focus::Sidebar => self.sidebar_action(action),
            Focus::Main => match b.tab {
                Tab::Structure => self.structure_action(action),
                Tab::Data | Tab::Query => self.grid_action(action),
            },
        }
    }

    fn sidebar_action(&mut self, action: Action) {
        let Some(b) = &mut self.browser else { return };
        let page = 10;
        match action {
            Action::Up => b.move_tree(-1),
            Action::Down => b.move_tree(1),
            Action::PageUp | Action::HalfPageUp => b.move_tree(-page),
            Action::PageDown | Action::HalfPageDown => b.move_tree(page),
            Action::Top => b.tree_selected = 0,
            Action::Bottom => b.tree_selected = b.tree_items().len().saturating_sub(1),
            Action::Right => match b.selected_item() {
                Some(TreeItem::Schema(s)) => b.set_expanded(s, true),
                Some(TreeItem::Table(..)) => self.open_selected_table(),
                None => {}
            },
            Action::Left => match b.selected_item() {
                Some(TreeItem::Schema(s)) => b.set_expanded(s, false),
                Some(TreeItem::Table(s, _)) => b.set_expanded(s, false),
                None => {}
            },
            Action::Select => match b.selected_item() {
                Some(TreeItem::Schema(s)) => {
                    let open = b.expanded.contains(&b.schemas[s].name);
                    b.set_expanded(s, !open);
                }
                Some(TreeItem::Table(..)) => self.open_selected_table(),
                None => {}
            },
            Action::Filter => {
                let current = b.tree_filter.clone();
                self.popup = Some(Popup::Prompt(Prompt {
                    kind: PromptKind::TreeFilter,
                    title: "Find table".into(),
                    hint: "Type to filter tables by name".into(),
                    input: Input::new(current),
                }));
            }
            Action::Back => {
                if !b.tree_filter.is_empty() {
                    b.tree_filter.clear();
                    b.clamp_tree();
                } else {
                    self.request_disconnect();
                }
            }
            Action::Refresh => self.refresh_schemas(),
            _ => {}
        }
    }

    fn structure_action(&mut self, action: Action) {
        let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) else {
            if action == Action::Back
                && let Some(b) = &mut self.browser
            {
                b.focus = Focus::Sidebar;
            }
            return;
        };
        match action {
            Action::Up => t.structure_scroll = t.structure_scroll.saturating_sub(1),
            Action::Down => t.structure_scroll = t.structure_scroll.saturating_add(1),
            Action::PageUp | Action::HalfPageUp => t.structure_scroll = t.structure_scroll.saturating_sub(10),
            Action::PageDown | Action::HalfPageDown => t.structure_scroll = t.structure_scroll.saturating_add(10),
            Action::Top => t.structure_scroll = 0,
            Action::Refresh => {
                if !t.grid.has_pending() {
                    t.structure = None;
                }
                self.request_page();
            }
            Action::Back => {
                if let Some(b) = &mut self.browser {
                    b.focus = Focus::Sidebar;
                }
            }
            _ => {}
        }
    }

    /// Returns the grid that currently has focus (table data or query results).
    fn active_grid(&mut self) -> Option<&mut Grid> {
        let b = self.browser.as_mut()?;
        match b.tab {
            Tab::Data => b.table.as_mut().map(|t| &mut t.grid),
            Tab::Query => b.query.result.as_mut(),
            Tab::Structure => None,
        }
    }

    fn grid_action(&mut self, action: Action) {
        let on_query = self.browser.as_ref().is_some_and(|b| b.tab == Tab::Query);
        // Navigation and read-only actions.
        if let Some(grid) = self.active_grid() {
            let handled = match action {
                Action::Up => {
                    grid.move_row(-1);
                    true
                }
                Action::Down => {
                    grid.move_row(1);
                    true
                }
                Action::Left => {
                    grid.move_col(-1);
                    true
                }
                Action::Right => {
                    grid.move_col(1);
                    true
                }
                Action::PageUp => {
                    grid.page(-1.0);
                    true
                }
                Action::PageDown => {
                    grid.page(1.0);
                    true
                }
                Action::HalfPageUp => {
                    grid.page(-0.5);
                    true
                }
                Action::HalfPageDown => {
                    grid.page(0.5);
                    true
                }
                Action::Top => {
                    grid.top();
                    true
                }
                Action::Bottom => {
                    grid.bottom();
                    true
                }
                Action::Home => {
                    grid.home();
                    true
                }
                Action::End => {
                    grid.end();
                    true
                }
                _ => false,
            };
            if handled {
                return;
            }
        }
        match action {
            Action::View => self.view_cell(),
            Action::Copy => self.copy_cell(),
            Action::Back => {
                if let Some(b) = &mut self.browser {
                    if b.tab == Tab::Query {
                        b.query.focus = EditorFocus::Editor;
                    } else {
                        b.focus = Focus::Sidebar;
                    }
                }
            }
            _ if on_query => {
                if matches!(action, Action::Select | Action::Edit) {
                    self.view_cell();
                } else if matches!(action, Action::Refresh) {
                    self.run_query();
                }
            }
            // Enter edits when allowed, otherwise shows the value; while the
            // table is busy, it says why it cannot edit yet.
            Action::Select => match self.edit_permission(false) {
                Ok(()) if !self.binary_cell() && self.cut_cell().is_none() => self.edit_cell(),
                Err(reason) if self.table_busy().is_some_and(|busy| reason == busy) => self.deny(reason),
                _ => self.view_cell(),
            },
            Action::Edit => self.edit_cell(),
            Action::SetNull => self.set_current_cell(None),
            Action::Paste => {
                let value = self.register.clone();
                match value {
                    Some(v) => self.set_current_cell(Some(v)),
                    None => self.notify(Level::Info, "Nothing to paste — copy a cell first"),
                }
            }
            Action::DeleteRow => self.delete_row(),
            Action::InsertRow => self.insert_row(),
            Action::Undo => {
                if let Some(reason) = self.table_busy() {
                    return self.deny(reason.into());
                }
                if let Some(grid) = self.active_grid()
                    && !grid.undo()
                {
                    self.notify(Level::Info, "Nothing to undo");
                }
            }
            Action::Refresh => self.request_page(),
            Action::NextPage => self.change_page(1),
            Action::PrevPage => self.change_page(-1),
            Action::Filter => self.open_filter_prompt(),
            Action::Sort => self.toggle_sort(),
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Permissions
    // ------------------------------------------------------------------

    /// Why the open table cannot be changed right now: its rows are being
    /// replaced, or its changes committed. Changes staged meanwhile would
    /// land on whatever rows are there once that is done.
    fn table_busy(&self) -> Option<&'static str> {
        let b = self.browser.as_ref()?;
        if b.commit.is_some() {
            return Some("A commit is running — wait for its result");
        }
        b.table.as_ref().filter(|t| t.loading).map(|_| "The table is loading — wait for it to finish")
    }

    /// Whether the table cannot be reloaded (or filtered, sorted, paged)
    /// now; tells the user why.
    fn reload_blocked(&mut self) -> bool {
        let Some(b) = &self.browser else { return true };
        if b.commit.is_some() {
            self.notify(Level::Warning, "A commit is running — wait for its result");
            return true;
        }
        if b.has_pending_edits() {
            let hint = match self.config.keymap {
                KeymapMode::Vim => ":w to commit, :e! to discard",
                KeymapMode::Standard => "Ctrl-S to commit, Ctrl-Z to undo",
            };
            self.notify(Level::Warning, format!("Pending changes — {hint} before reloading"));
            return true;
        }
        false
    }

    /// Checks whether the current cell may be edited. `Err` carries the reason.
    fn edit_permission(&self, set_null: bool) -> Result<(), String> {
        let b = self.browser.as_ref().ok_or("not connected")?;
        let t = b.table.as_ref().ok_or("no table open")?;
        if let Some(reason) = &b.session.read_only_reason {
            return Err(format!("Read-only session: {reason}"));
        }
        if let Some(reason) = self.table_busy() {
            return Err(reason.into());
        }
        if t.grid.is_empty() {
            return Err("no rows".into());
        }
        let row = t.grid.row;
        if t.grid.is_inserted(row) {
            return if t.privileges().insert {
                Ok(())
            } else {
                Err(format!("Read-only: no INSERT permission on {}", t.info.table))
            };
        }
        if !t.privileges().update {
            return Err(format!("Read-only: no UPDATE permission on {}", t.info.table));
        }
        if !t.has_primary_key() {
            return Err(format!("{} has no primary key, so rows cannot be edited safely", t.info.table));
        }
        if t.grid.edits.deleted.contains(&row) {
            return Err("Row is marked for deletion — undo first".into());
        }
        if set_null
            && let Some(name) = t.grid.columns.get(t.grid.col)
            && let Some(col) = t.structure.as_ref().and_then(|s| s.columns.iter().find(|c| c.name == *name))
            && !col.nullable
        {
            return Err(format!("Column {} does not accept NULL", col.name));
        }
        Ok(())
    }

    fn deny(&mut self, reason: String) {
        self.notify(Level::Warning, format!("🔒 {reason}"));
    }

    // ------------------------------------------------------------------
    // Grid editing
    // ------------------------------------------------------------------

    /// Whether the current cell holds a stored binary value, shown as
    /// `0x…`: editing that text would store the text instead.
    fn binary_cell(&self) -> bool {
        let Some(t) = self.browser.as_ref().and_then(|b| b.table.as_ref()) else { return false };
        t.grid.binary(t.grid.row, t.grid.col)
    }

    /// When the current cell shows only the start of a value too large to
    /// load in full: the size of that start and of all of it. Editing the
    /// start would store it alone.
    fn cut_cell(&self) -> Option<(usize, u64)> {
        let t = self.browser.as_ref()?.table.as_ref()?;
        let full = t.grid.cut(t.grid.row, t.grid.col)?;
        Some((t.grid.current_text().map_or(0, |s| s.len()), full))
    }

    fn edit_cell(&mut self) {
        if let Err(reason) = self.edit_permission(false) {
            return self.deny(reason);
        }
        if self.binary_cell() {
            return self.deny("This is binary data, shown as hex: edit it with SQL".into());
        }
        if let Some((loaded, full)) = self.cut_cell() {
            return self.deny(format!(
                "Only the first {} of this {} value were loaded: edit it with SQL",
                crate::util::size(loaded as u64),
                crate::util::size(full)
            ));
        }
        let Some(b) = self.browser.as_ref() else { return };
        let Some(t) = b.table.as_ref() else { return };
        let (row, col) = (t.grid.row, t.grid.col);
        let Some(column) = t.grid.columns.get(col).cloned() else { return };
        let new_row = t.grid.is_inserted(row);
        let target = if new_row {
            EditTarget::NewRow { grid: t.grid.id, index: row - t.grid.rows.len() }
        } else {
            match t.grid.row_key(row, &t.key_columns()) {
                Some(key) => EditTarget::Row(key),
                None => {
                    return self.deny(format!("{} has no primary key, so rows cannot be edited safely", t.info.table));
                }
            }
        };
        let info = t.structure.as_ref().and_then(|s| s.columns.iter().find(|c| c.name == column));
        let original = t.grid.cell_text(row, col);
        let hidden = original.as_deref().is_some_and(crate::util::has_hidden);
        let (lines, newline) = editor_lines(original.as_deref().unwrap_or_default());
        let mut editor = TextArea::new(lines);
        editor.move_cursor(ratatui_textarea::CursorMove::Bottom);
        editor.move_cursor(ratatui_textarea::CursorMove::End);
        self.popup = Some(Popup::CellEditor(Box::new(CellEditor {
            editor,
            generation: b.generation,
            table: t.info.table.clone(),
            target,
            original,
            newline,
            modified: false,
            data_type: info.map(|c| c.data_type.clone()).unwrap_or_default(),
            nullable: info.is_none_or(|c| c.nullable),
            new_row,
            hidden,
            column,
        })));
    }

    fn set_current_cell(&mut self, value: Option<String>) {
        if let Err(reason) = self.edit_permission(value.is_none()) {
            return self.deny(reason);
        }
        if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
            let (row, col) = (t.grid.row, t.grid.col);
            t.grid.set_cell(row, col, value);
        }
    }

    fn delete_row(&mut self) {
        if let Some(reason) = self.table_busy() {
            return self.deny(reason.into());
        }
        let Some(b) = self.browser.as_ref() else { return };
        let Some(t) = b.table.as_ref() else { return };
        if t.grid.is_empty() {
            return;
        }
        let row = t.grid.row;
        if !t.grid.is_inserted(row) {
            let denied = if let Some(reason) = &b.session.read_only_reason {
                Some(format!("Read-only session: {reason}"))
            } else if !t.privileges().delete {
                Some(format!("Read-only: no DELETE permission on {}", t.info.table))
            } else if !t.has_primary_key() {
                Some(format!("{} has no primary key, so rows cannot be deleted safely", t.info.table))
            } else {
                None
            };
            if let Some(reason) = denied {
                return self.deny(reason);
            }
        }
        if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
            t.grid.toggle_delete(row);
        }
    }

    fn insert_row(&mut self) {
        let Some(b) = self.browser.as_ref() else { return };
        let Some(t) = b.table.as_ref() else { return };
        let denied = if let Some(reason) = &b.session.read_only_reason {
            Some(format!("Read-only session: {reason}"))
        } else if !t.privileges().insert {
            Some(format!("Read-only: no INSERT permission on {}", t.info.table))
        } else if let Some(reason) = self.table_busy() {
            Some(reason.to_string())
        } else if !t.loaded || t.grid.columns.is_empty() {
            Some("Table is still loading".to_string())
        } else {
            None
        };
        if let Some(reason) = denied {
            return self.deny(reason);
        }
        if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
            t.grid.insert_row();
            t.grid.home();
        }
    }

    fn view_cell(&mut self) {
        let Some(b) = self.browser.as_ref() else { return };
        let grid = match b.tab {
            Tab::Data => b.table.as_ref().map(|t| &t.grid),
            Tab::Query => b.query.result.as_ref(),
            Tab::Structure => None,
        };
        let Some(grid) = grid.filter(|g| !g.is_empty()) else { return };
        let column = grid.columns.get(grid.col).cloned().unwrap_or_default();
        let (cut, binary) = (grid.cut(grid.row, grid.col), grid.binary(grid.row, grid.col));
        let (value, text) = match grid.cell(grid.row, grid.col).0 {
            grid::CellValue::Value(v) => (Some(v.to_string()), None),
            grid::CellValue::Null => (None, Some("NULL".to_string())),
            grid::CellValue::Default => (None, Some("(default)".to_string())),
        };
        // The start of a document is not one.
        let formatted = value.as_deref().filter(|_| cut.is_none()).and_then(pretty_value);
        let title = if formatted.is_some() { format!("{column} · JSON, re-indented") } else { column };
        let shown = formatted.as_deref().or(text.as_deref()).or(value.as_deref()).unwrap_or_default();
        let text = crate::util::display_safe_lines(shown);
        self.popup = Some(Popup::CellView { title, text, value, cut, binary, scroll: 0, wrapped: None });
    }

    fn copy_cell(&mut self) {
        let Some(grid) = self.active_grid() else { return };
        let (text, cut, binary) = (grid.current_text(), grid.cut(grid.row, grid.col), grid.binary(grid.row, grid.col));
        match text {
            Some(text) => self.copy_value(text, cut, binary),
            None => self.notify(Level::Info, "Cell is NULL — nothing copied"),
        }
    }

    /// Copies a cell's value: to the system clipboard, made safe to paste
    /// there (see [`crate::clipboard::clipboard_text`]), and as it is to the
    /// register that pastes into cells, unless only its start was loaded
    /// (`cut`: the size of all of it), which would store that start alone,
    /// or it is `binary` data shown as `0x…`, which would store that text.
    fn copy_value(&mut self, value: String, cut: Option<u64>, binary: bool) {
        let preview: String = value.chars().take(40).collect();
        let copied = crate::clipboard::copy(&value);
        let mut level = Level::Success;
        let mut message = format!("Copied “{preview}”");
        if let Some(full) = cut {
            level = Level::Warning;
            message = format!(
                "Copied the first {} of “{preview}”: only that much of its {} was loaded. \
                 It cannot be pasted into a cell.",
                crate::util::size(value.len() as u64),
                crate::util::size(full)
            );
            self.register = None;
        } else if binary {
            level = Level::Warning;
            message.push_str(
                ": binary data, shown as hex. It cannot be pasted into a cell, which would store the text: \
                 use SQL.",
            );
            self.register = None;
        } else {
            self.register = Some(value);
        }
        if copied.cut {
            level = Level::Warning;
            message.push_str(&format!(
                " Only the first {} went to the clipboard.",
                crate::util::size(crate::clipboard::COPY_LIMIT as u64)
            ));
        }
        if copied.escaped > 0 {
            level = Level::Warning;
            message.push_str(&format!(
                " {} control character(s) are shown as symbols (␛…) on the clipboard, so that pasting cannot \
                 type keys; pasting into a cell here keeps them.",
                copied.escaped
            ));
        }
        self.notify(level, message);
    }

    fn change_page(&mut self, delta: isize) {
        if self.reload_blocked() {
            return;
        }
        let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) else { return };
        if delta > 0 && !t.has_next_page() {
            return self.notify(Level::Info, "Already on the last page");
        }
        if delta < 0 && t.page == 0 {
            return self.notify(Level::Info, "Already on the first page");
        }
        t.page = (t.page as isize + delta).max(0) as usize;
        t.grid.row = 0;
        self.request_page();
    }

    fn open_filter_prompt(&mut self) {
        if self.browser.as_ref().is_none_or(|b| b.table.is_none()) || self.reload_blocked() {
            return;
        }
        let Some(t) = self.browser.as_ref().and_then(|b| b.table.as_ref()) else { return };
        let column = t.grid.columns.get(t.grid.col).cloned();
        self.popup = Some(Popup::Prompt(Prompt {
            kind: PromptKind::Filter,
            title: format!("Filter {}", t.info.table.name),
            hint: match column {
                Some(c) => format!("SQL WHERE condition, e.g. {c} IS NOT NULL — empty clears"),
                None => "SQL WHERE condition — empty clears".into(),
            },
            input: Input::new(t.filter.clone().unwrap_or_default()),
        }));
    }

    fn toggle_sort(&mut self) {
        if self.reload_blocked() {
            return;
        }
        let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) else { return };
        let Some(col) = t.grid.columns.get(t.grid.col).cloned() else { return };
        t.order_by = match &t.order_by {
            Some((c, true)) if *c == col => Some((col, false)),
            Some((c, false)) if *c == col => None,
            _ => Some((col, true)),
        };
        t.page = 0;
        self.request_page();
    }

    // ------------------------------------------------------------------
    // Popups
    // ------------------------------------------------------------------

    fn popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        let vim = self.keymap_mode() == KeymapMode::Vim;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let scroll_delta = |key: &KeyEvent| -> Option<i32> {
            match key.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-10),
                KeyCode::PageDown => Some(10),
                KeyCode::Char('k') if vim => Some(-1),
                KeyCode::Char('j') if vim => Some(1),
                KeyCode::Char('u') if vim && key.modifiers.contains(KeyModifiers::CONTROL) => Some(-10),
                KeyCode::Char('d') if vim && key.modifiers.contains(KeyModifiers::CONTROL) => Some(10),
                _ => None,
            }
        };
        let scrolled = |s: u16, d: i32| (s as i32 + d).max(0) as u16;

        self.popup = match popup {
            Popup::Help { scroll } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1) => None,
                _ => Some(Popup::Help { scroll: scroll_delta(&key).map_or(scroll, |d| scrolled(scroll, d)) }),
            },
            Popup::Message { title, body, error, scroll } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
                _ => Some(Popup::Message {
                    title,
                    body,
                    error,
                    scroll: scroll_delta(&key).map_or(scroll, |d| scrolled(scroll, d)),
                }),
            },
            Popup::CellView { title, text, value, cut, binary, mut scroll, wrapped } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ') | KeyCode::Char('v') => None,
                code => {
                    match code {
                        KeyCode::Char('y') | KeyCode::Char('c') if !ctrl || code == KeyCode::Char('c') => {
                            // The value itself, as the grid copies it: not the
                            // re-indented text, and nothing for NULL (which would
                            // otherwise be pasted back as the text 'NULL').
                            match &value {
                                Some(v) => self.copy_value(v.clone(), cut, binary),
                                None if text == "NULL" => self.notify(Level::Info, "Cell is NULL — nothing copied"),
                                None => self.notify(Level::Info, "Cell uses the column default — nothing copied"),
                            }
                        }
                        // Scrolling stops at the end when drawn.
                        KeyCode::Home => scroll = 0,
                        KeyCode::Char('g') if vim => scroll = 0,
                        KeyCode::End => scroll = usize::MAX,
                        KeyCode::Char('G') if vim => scroll = usize::MAX,
                        _ => {
                            if let Some(d) = scroll_delta(&key) {
                                scroll = scroll.saturating_add_signed(d as isize);
                            }
                        }
                    }
                    Some(Popup::CellView { title, text, value, cut, binary, scroll, wrapped })
                }
            },
            Popup::Confirm { title, message, action } => match key.code {
                KeyCode::Enter if !action.enter_confirms() => Some(Popup::Confirm { title, message, action }),
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm(action);
                    self.popup.take()
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => None,
                _ => Some(Popup::Confirm { title, message, action }),
            },
            Popup::Commit { statements, previews, hidden, scroll, non_transactional, snapshot } => {
                let force = commit_needs_force(&statements, non_transactional.as_ref());
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter if !force => {
                        self.apply_statements(statements, non_transactional, *snapshot);
                        None
                    }
                    KeyCode::Char('!') if force => {
                        self.apply_statements(statements, non_transactional, *snapshot);
                        None
                    }
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => None,
                    code => {
                        if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                            self.notify(Level::Warning, "This table cannot roll back: press ! to commit anyway");
                        }
                        Some(Popup::Commit {
                            statements,
                            previews,
                            hidden,
                            scroll: scroll_delta(&key).map_or(scroll, |d| scrolled(scroll, d)),
                            non_transactional,
                            snapshot,
                        })
                    }
                }
            }
            Popup::Form(mut form) => match form.handle_key(key) {
                FormOutcome::None => Some(Popup::Form(form)),
                FormOutcome::Cancel => None,
                FormOutcome::Save => {
                    self.save_form(form);
                    self.popup.take()
                }
                FormOutcome::Test => {
                    self.test_connection(&form);
                    Some(Popup::Form(form))
                }
            },
            Popup::Password(mut p) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let password = Zeroizing::new(p.input.value().to_string());
                    let remember = p.remember.then(|| self.remember(&p.profile, password.clone(), false));
                    self.spawn_connect(p.profile.clone(), Some(password), remember);
                    None
                }
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                    p.focus_remember = !p.focus_remember;
                    Some(Popup::Password(p))
                }
                KeyCode::Char(' ') if p.focus_remember => {
                    p.remember = !p.remember;
                    Some(Popup::Password(p))
                }
                _ => {
                    if !p.focus_remember {
                        p.input.handle_key(key);
                    }
                    Some(Popup::Password(p))
                }
            },
            Popup::Vault(mut v) => match key.code {
                KeyCode::Esc => {
                    if let AfterUnlock::Store { .. } = v.then {
                        self.notify(Level::Warning, "Password was not saved");
                    }
                    None
                }
                _ if v.working.is_some() => Some(Popup::Vault(v)),
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down if v.create => {
                    v.focus = 1 - v.focus;
                    Some(Popup::Vault(v))
                }
                KeyCode::Enter if v.create && v.focus == 0 => {
                    v.focus = 1;
                    Some(Popup::Vault(v))
                }
                KeyCode::Enter => self.submit_vault(v),
                _ => {
                    if v.focus == 0 {
                        v.input.handle_key(key)
                    } else {
                        v.confirm.handle_key(key)
                    };
                    v.error = None;
                    Some(Popup::Vault(v))
                }
            },
            Popup::Prompt(mut p) => match key.code {
                KeyCode::Esc => {
                    if p.kind == PromptKind::TreeFilter
                        && let Some(b) = &mut self.browser
                    {
                        b.tree_filter.clear();
                        b.clamp_tree();
                    }
                    None
                }
                KeyCode::Enter => {
                    self.submit_prompt(p);
                    None
                }
                _ => {
                    p.input.handle_key(key);
                    if p.kind == PromptKind::TreeFilter
                        && let Some(b) = &mut self.browser
                    {
                        b.tree_filter = p.input.value().to_string();
                        b.tree_selected =
                            b.tree_items().iter().position(|i| matches!(i, TreeItem::Table(..))).unwrap_or(0);
                    }
                    Some(Popup::Prompt(p))
                }
            },
            Popup::CellEditor(mut c) => {
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Esc => None,
                    KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                        c.editor.insert_newline();
                        c.modified = true;
                        Some(Popup::CellEditor(c))
                    }
                    KeyCode::Enter => {
                        let value = c.value();
                        // Nothing to stage: not even a rewritten line ending.
                        if c.unchanged(&value) { None } else { self.stage_from_editor(c, CellChange::Set(Some(value))) }
                    }
                    KeyCode::Char('n') if ctrl => {
                        if c.nullable {
                            self.stage_from_editor(c, CellChange::Set(None))
                        } else {
                            self.notify(Level::Warning, format!("Column {} does not accept NULL", c.column));
                            Some(Popup::CellEditor(c))
                        }
                    }
                    KeyCode::Char('d') if ctrl && c.new_row => self.stage_from_editor(c, CellChange::Default),
                    _ => {
                        c.modified |= c.editor.input(key);
                        Some(Popup::CellEditor(c))
                    }
                }
            }
        };
    }

    /// Where the cell an editor edits is now, if it is still there: the
    /// same connection and table, the row with the same key (or the same
    /// new row), and the column with the same name.
    fn editor_cell(&self, c: &CellEditor) -> Option<(usize, usize)> {
        let b = self.browser.as_ref().filter(|b| b.generation == c.generation)?;
        let t = b.table.as_ref().filter(|t| t.info.table == c.table)?;
        let col = t.grid.columns.iter().position(|name| *name == c.column)?;
        let row = match &c.target {
            EditTarget::Row(key) => t.grid.find_row(key)?,
            EditTarget::NewRow { grid, index } => {
                (t.grid.id == *grid && *index < t.grid.edits.inserted.len()).then(|| t.grid.rows.len() + index)?
            }
        };
        Some((row, col))
    }

    /// Stages what a cell editor asks for on its cell, wherever that is
    /// now. While the table is busy the editor stays open, to try again.
    fn stage_from_editor(&mut self, c: Box<CellEditor>, change: CellChange) -> Option<Popup> {
        if let Some(reason) = self.table_busy() {
            self.deny(reason.into());
            return Some(Popup::CellEditor(c));
        }
        let Some((row, col)) = self.editor_cell(&c) else {
            self.notify(Level::Warning, "The row is no longer shown (the table was reloaded): nothing was staged");
            return None;
        };
        if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
            match change {
                CellChange::Set(value) => t.grid.set_cell(row, col, value),
                CellChange::Default => t.grid.set_default(row, col),
            }
            t.grid.row = row;
            t.grid.col = col;
        }
        None
    }

    /// After the open table's rows or staged changes were replaced: closes
    /// a cell editor whose cell is gone, rather than leaving it to stage
    /// its value on another one.
    fn check_cell_editor(&mut self) {
        if let Some(Popup::CellEditor(c)) = &self.popup
            && self.editor_cell(c).is_none()
        {
            self.popup = None;
            self.notify(
                Level::Warning,
                "The row being edited is no longer shown (the table was reloaded): nothing was staged",
            );
        }
    }

    fn submit_prompt(&mut self, p: Prompt) {
        match p.kind {
            PromptKind::TreeFilter => {
                if let Some(b) = &mut self.browser {
                    b.tree_filter = p.input.value().to_string();
                    b.clamp_tree();
                }
            }
            PromptKind::Filter => {
                let Some(driver) = self.browser.as_ref().map(|b| b.db.driver()) else { return };
                if self.reload_blocked() {
                    return;
                }
                let value = p.input.value().trim().to_string();
                if let Err(e) = sql::validate_filter(driver, &value) {
                    return self.notify(Level::Error, format!("Invalid filter: {e}"));
                }
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
                    t.filter = (!value.is_empty()).then_some(value);
                    t.page = 0;
                    t.grid.row = 0;
                }
                self.request_page();
            }
        }
    }

    fn confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteProfile(id) => {
                if let Some(profile) = self.profile(&id).cloned() {
                    self.notify(Level::Success, format!("Deleted {}", profile.name));
                    self.update_config(|c| c.connections.retain(|p| p.id != id));
                    self.forget_password(&id, profile.credential);
                }
            }
            ConfirmAction::Disconnect => self.disconnect(),
            ConfirmAction::Quit => self.should_quit = true,
            ConfirmAction::SendSavedPassword(saved) => {
                let SavedPassword { profile, password, unbound } = *saved;
                // Saved before passwords were bound: bind it once it works.
                let rebind = unbound.then(|| self.remember(&profile, password.clone(), true));
                self.spawn_connect(profile, Some(password), rebind);
            }
            // The changes being committed are not discarded: they are no
            // longer the user's to discard.
            ConfirmAction::DiscardAndOpen(_) | ConfirmAction::DiscardEdits if self.commit_running() => {
                self.notify(Level::Warning, "A commit is running — wait for its result");
            }
            ConfirmAction::DiscardAndOpen(table) => {
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
                    t.grid.clear_edits();
                }
                self.open_table(&table);
            }
            ConfirmAction::DiscardEdits => {
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
                    t.grid.clear_edits();
                }
                self.notify(Level::Info, "Pending changes discarded");
                self.request_page();
            }
        }
    }

    fn commit_running(&self) -> bool {
        self.browser.as_ref().is_some_and(|b| b.commit.is_some())
    }

    fn request_quit(&mut self) {
        let pending = self.browser.as_ref().map(|b| b.pending_count()).unwrap_or(0);
        if let Some(b) = self.browser.as_ref().filter(|b| b.commit.is_some()) {
            self.popup = Some(Popup::Confirm {
                title: "Quit".into(),
                message: format!(
                    "A commit to {} is still running. It may still be applied, and its result will not be shown. \
                     Quit anyway?",
                    b.profile.name
                ),
                action: ConfirmAction::Quit,
            });
        } else if pending > 0 {
            self.popup = Some(Popup::Confirm {
                title: "Quit".into(),
                message: format!("You have {pending} uncommitted change(s). Quit and discard them?"),
                action: ConfirmAction::Quit,
            });
        } else {
            self.should_quit = true;
        }
    }

    fn request_disconnect(&mut self) {
        let Some(b) = &self.browser else { return };
        let pending = b.pending_count();
        let message = if b.commit.is_some() {
            format!("A commit to {} is still running and may still be applied. Disconnect anyway?", b.profile.name)
        } else if pending > 0 {
            format!("Disconnect from {} and discard {pending} uncommitted change(s)?", b.profile.name)
        } else {
            format!("Disconnect from {}?", b.profile.name)
        };
        self.popup = Some(Popup::Confirm { title: "Disconnect".into(), message, action: ConfirmAction::Disconnect });
    }

    fn disconnect(&mut self) {
        if let Some(b) = self.browser.take() {
            let db = b.db.clone();
            tokio::spawn(async move { db.close().await });
            if b.commit.is_some() {
                // Its result is reported when it comes (see `Committed`).
                self.busy = None;
                self.notify(
                    Level::Warning,
                    format!(
                        "Disconnected from {}; its commit is still running and may still be applied",
                        b.profile.name
                    ),
                );
            } else {
                self.notify(Level::Info, format!("Disconnected from {}", b.profile.name));
            }
        }
        self.screen = Screen::Connections;
    }

    // ------------------------------------------------------------------
    // Vim command line
    // ------------------------------------------------------------------

    pub fn run_command(&mut self, cmd: &str) {
        let (name, arg) = match cmd.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (cmd, ""),
        };
        let connected = self.browser.is_some();
        match name {
            "" => {}
            "q" | "quit" | "close" if connected => self.request_disconnect(),
            "q!" | "quit!" if connected => self.disconnect(),
            "q" | "quit" | "qa" | "qall" | "quitall" => self.request_quit(),
            "q!" | "qa!" | "quit!" => self.should_quit = true,
            "w" | "write" | "wq" | "x" => self.begin_commit(),
            "e!" | "discard" => self.confirm(ConfirmAction::DiscardEdits),
            "e" | "r" | "refresh" | "reload" => {
                self.request_page();
                self.refresh_schemas();
            }
            "set" => match arg {
                "vim" | "keymap=vim" => self.set_keymap(KeymapMode::Vim),
                "novim" | "keymap=standard" | "keymap=std" => self.set_keymap(KeymapMode::Standard),
                _ => self.notify(Level::Error, format!("Unknown option: {arg} (try :set vim or :set novim)")),
            },
            "help" | "h" => self.popup = Some(Popup::Help { scroll: 0 }),
            "sql" | "query" | "data" | "structure" | "struct" if connected => {
                let action = match name {
                    "data" => Action::TabData,
                    "structure" | "struct" => Action::TabStructure,
                    _ => Action::TabQuery,
                };
                self.browser_action(action);
            }
            "run" if connected => self.run_query(),
            "filter" | "where" if connected => {
                let Some(driver) = self.browser.as_ref().map(|b| b.db.driver()) else { return };
                if self.reload_blocked() {
                    return;
                }
                if let Err(e) = sql::validate_filter(driver, arg) {
                    return self.notify(Level::Error, format!("Invalid filter: {e}"));
                }
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
                    t.filter = (!arg.is_empty()).then(|| arg.to_string());
                    t.page = 0;
                }
                self.request_page();
            }
            "nofilter" if connected => self.run_command("filter"),
            "sort" if connected => {
                if self.reload_blocked() {
                    return;
                }
                let mut parts = arg.split_whitespace();
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut()) {
                    t.order_by = parts.next().map(|c| (c.to_string(), !matches!(parts.next(), Some("desc" | "DESC"))));
                    t.page = 0;
                }
                self.request_page();
            }
            "disconnect" if connected => self.request_disconnect(),
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let target = n.parse::<usize>().unwrap_or(1).saturating_sub(1);
                if let Some(grid) = self.active_grid() {
                    grid.row = target.min(grid.total_rows().saturating_sub(1));
                }
            }
            _ => self.notify(Level::Error, format!("Not a command: {name}")),
        }
    }

    // ------------------------------------------------------------------
    // Connections & credentials
    // ------------------------------------------------------------------

    /// Connects to a profile given on the command line.
    pub fn start(&mut self, profile: ConnectionProfile) {
        self.connect(profile);
    }

    fn connect_selected(&mut self) {
        if let Some(profile) = self.config.connections.get(self.selected_conn).cloned() {
            self.connect(profile);
        }
    }

    /// Starts a connection attempt. Whatever an earlier attempt still
    /// brings back (a connection, an error, a saved password) is discarded.
    fn begin_connect(&mut self) -> u64 {
        self.connect_seq += 1;
        self.connecting = Some(self.connect_seq);
        self.to_remember = None;
        self.connect_seq
    }

    fn connect(&mut self, profile: ConnectionProfile) {
        let attempt = self.begin_connect();
        if !profile.needs_password() {
            return self.spawn_connect(profile, None, None);
        }
        match profile.credential {
            CredentialStore::None => self.ask_password(profile, None),
            // Read from the keyring in the background: it can be slow.
            CredentialStore::Keyring => {
                self.busy = Some(format!("Connecting to {}", profile.name));
                let (keyring, tx) = (self.keyring.clone(), self.tx.clone());
                tokio::task::spawn_blocking(move || {
                    let result = keyring.get(&profile.id).map_err(err_text);
                    let _ = tx.send(AppEvent::SavedPassword { attempt, profile, result });
                });
            }
            CredentialStore::Vault => match &self.vault {
                None => self.open_vault_prompt(AfterUnlock::Connect(profile)),
                Some(vault) => {
                    let result = vault.get(&profile.id);
                    self.connect_with_saved(profile, result, "the vault");
                }
            },
        }
    }

    /// Connects with the password saved for `profile`, if it was saved for
    /// the server the profile now points to (see [`App::send_saved_password`]);
    /// otherwise asks for one.
    fn connect_with_saved(
        &mut self,
        profile: ConnectionProfile,
        saved: anyhow::Result<Option<StoredSecret>>,
        store: &str,
    ) {
        let secret = match saved {
            Ok(Some(secret)) => secret,
            Ok(None) => return self.ask_password(profile, Some(format!("No password saved in {store}"))),
            Err(e) => {
                if e.is::<VaultChanged>() {
                    self.vault = None;
                }
                return self.ask_password(profile, Some(err_text(e)));
            }
        };
        if let Some(password) = secret.unbound_password() {
            return self.send_saved_password(profile, password.clone(), true);
        }
        match secret.password_for(&Endpoint::of(&profile)) {
            Ok(password) => self.send_saved_password(profile, password.clone(), false),
            Err(mismatch) => self.ask_password(profile, Some(mismatch)),
        }
    }

    /// Connects with a saved password, unless the user should decide first
    /// whether it goes there: one saved before passwords were bound to their
    /// server (nothing says the profile still points to it), or a remote
    /// server that the SSL mode does not authenticate (whoever is on the
    /// network path could pose as it and receive the password).
    fn send_saved_password(&mut self, profile: ConnectionProfile, password: Zeroizing<String>, unbound: bool) {
        let mut reasons = Vec::new();
        if let Some(warning) = profile.tls_warning() {
            reasons.push(format!(
                "⚠ SSL mode {}: {warning}. Whoever is on the network path could pose as the server and receive \
                 the password.",
                profile.ssl_mode.label()
            ));
        }
        if unbound {
            reasons.push(
                "⚠ It was saved by an earlier version of dbtui, which did not record the server it is for: check \
                 that this is that server. Once it works, it is saved for this server only."
                    .to_string(),
            );
        }
        if reasons.is_empty() {
            return self.spawn_connect(profile, Some(password), None);
        }
        self.busy = None;
        self.connecting = None;
        self.popup = Some(Popup::Confirm {
            title: "Send saved password?".into(),
            message: format!(
                "Send the password saved for {} to {}?\n\n{}",
                profile.name,
                Endpoint::of(&profile),
                reasons.join("\n\n")
            ),
            action: ConfirmAction::SendSavedPassword(Box::new(SavedPassword { profile, password, unbound })),
        });
    }

    fn remember(&self, profile: &ConnectionProfile, password: Zeroizing<String>, rebind: bool) -> Remember {
        Remember { password, rebind, generation: self.credentials.generation(&profile.id) }
    }

    fn ask_password(&mut self, profile: ConnectionProfile, error: Option<String>) {
        self.busy = None;
        self.popup = Some(Popup::Password(PasswordPrompt {
            remember: profile.credential != CredentialStore::None,
            profile,
            input: Input::default(),
            focus_remember: false,
            error,
        }));
    }

    fn spawn_connect(
        &mut self,
        mut profile: ConnectionProfile,
        password: Option<Zeroizing<String>>,
        remember: Option<Remember>,
    ) {
        let attempt = self.begin_connect();
        self.to_remember = remember;
        self.busy = Some(format!("Connecting to {}", profile.name));
        let forced_read_only = self.read_only_run && !profile.read_only;
        profile.read_only |= self.read_only_run;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = guarded(async {
                let db = Database::connect(&profile, password.as_deref().map(String::as_str))
                    .await
                    .map_err(connect_error)?;
                let info = async { anyhow::Ok((db.session_info(&profile).await?, db.schemas().await?)) }.await;
                match info {
                    Ok((mut session, schemas)) => {
                        if forced_read_only {
                            session.read_only_reason = Some("dbtui was started with --read-only".into());
                        }
                        Ok((db, session, schemas))
                    }
                    Err(e) => {
                        db.close().await;
                        Err(err_text(e))
                    }
                }
            })
            .await;
            let event = match result {
                Ok((db, session, schemas)) => {
                    AppEvent::Connected { attempt, profile, db, session: Box::new(session), schemas }
                }
                Err(error) => AppEvent::ConnectFailed { attempt, profile, error, needs_password: false },
            };
            let _ = tx.send(event);
        });
    }

    fn test_connection(&mut self, form: &ConnectionForm) {
        let mut profile = match form.to_profile() {
            Ok(p) => p,
            Err(e) => return self.notify(Level::Error, e),
        };
        profile.read_only |= self.read_only_run;
        // Where its password is saved now, which may have changed since the
        // form was opened.
        profile.credential = self.profile(&profile.id).map_or(CredentialStore::None, |p| p.credential);
        let endpoint = Endpoint::of(&profile);
        let mut password =
            (!form.password.value().is_empty()).then(|| Zeroizing::new(form.password.value().to_string()));
        let saved =
            if password.is_none() && profile.needs_password() { profile.credential } else { CredentialStore::None };
        // A saved password is only used, unasked, for the server it was saved
        // for, and when that server is authenticated (or local); otherwise
        // it is typed. Connecting from the list asks instead.
        if saved != CredentialStore::None
            && let Some(warning) = profile.tls_warning()
        {
            let error = format!(
                "The saved password is not sent unasked with SSL mode {} ({warning}): type it in Password to test",
                profile.ssl_mode.label()
            );
            let _ = self.tx.send(AppEvent::TestResult(Err(error)));
            return;
        }
        let use_keyring = saved == CredentialStore::Keyring;
        if saved == CredentialStore::Vault
            && let Some(vault) = &self.vault
        {
            let saved = vault.get(&profile.id).map_err(err_text).and_then(|secret| match secret {
                Some(secret) => secret.password_for(&endpoint).map(|pw| Some(pw.clone())),
                None => Ok(None),
            });
            match saved {
                Ok(saved) => password = saved,
                // Reported like any other test failure, once the form is back.
                Err(e) => {
                    let _ = self.tx.send(AppEvent::TestResult(Err(e)));
                    return;
                }
            }
        }
        self.busy = Some("Testing connection".into());
        let (keyring, tx) = (self.keyring.clone(), self.tx.clone());
        tokio::spawn(async move {
            let test = async {
                let mut password = password;
                if use_keyring {
                    let id = profile.id.clone();
                    let saved = tokio::task::spawn_blocking(move || keyring.get(&id)).await??;
                    if let Some(secret) = saved {
                        password = Some(secret.password_for(&endpoint).map_err(anyhow::Error::msg)?.clone());
                    }
                }
                let db = Database::connect(&profile, password.as_deref().map(String::as_str)).await?;
                let info = db.session_info(&profile).await;
                db.close().await;
                let info = info?;
                let mut msg = format!("Connected to {} as {}", info.server_version, info.user);
                if let Some((transport, _)) = info.transport.summary(&profile) {
                    msg.push_str(&format!(" · {transport}"));
                }
                let level = match profile.tls_warning() {
                    Some(warning) => {
                        msg.push_str(&format!(" · ⚠ {}: {warning}", profile.ssl_mode.label()));
                        Level::Warning
                    }
                    None => Level::Success,
                };
                anyhow::Ok((level, msg))
            };
            let result = guarded(async { test.await.map_err(|e| with_certificate_hint(err_text(e), " ")) }).await;
            let _ = tx.send(AppEvent::TestResult(result));
        });
    }

    fn save_form(&mut self, form: Box<ConnectionForm>) {
        let mut profile = match form.to_profile() {
            Ok(p) => p,
            Err(e) => {
                let mut form = form;
                form.error = Some(e);
                self.popup = Some(Popup::Form(form));
                return;
            }
        };
        let id = profile.id.clone();
        // The profile as saved now: its password may have been saved (by a
        // keyring that was slow to answer) since the form was opened.
        let saved = self.profile(&id).cloned();
        profile.credential = saved.as_ref().map_or(CredentialStore::None, |p| p.credential);
        let password = Zeroizing::new(form.password.value().to_string());
        let save_password = form.save_password && profile.needs_password();
        let (name, endpoint) = (profile.name.clone(), Endpoint::of(&profile));
        // A saved password is for the server it was saved for: one pointed
        // elsewhere loses it (one saved by an earlier version, which does
        // not say which server it is for, would otherwise go there).
        let moved_from = saved.as_ref().map(Endpoint::of).filter(|old| *old != endpoint && profile.needs_password());
        let had_password = profile.credential != CredentialStore::None || self.credentials.knows(&id);
        let forget = had_password && (!save_password || moved_from.is_some());
        if forget {
            self.forget_password(&id, profile.credential);
        }
        self.update_config(|c| {
            let mut p = profile.clone();
            match c.connections.iter().position(|p| p.id == id) {
                Some(pos) => {
                    // Where its password is saved is recorded as it is on
                    // disk, unless it was just forgotten.
                    p.credential = if forget { CredentialStore::None } else { c.connections[pos].credential };
                    c.connections[pos] = p;
                }
                None => c.connections.push(p),
            }
        });
        self.selected_conn = self.config.connections.iter().position(|p| p.id == id).unwrap_or(0);
        if save_password && !password.is_empty() {
            self.notify(Level::Success, format!("Saved {name}"));
            self.store_password(&id, endpoint, password);
        } else if let Some(old) = moved_from.filter(|_| forget) {
            self.notify(
                Level::Info,
                format!(
                    "Saved {name}. Its saved password was for {} and has been removed: it is asked for on connect.",
                    old.address()
                ),
            );
        } else {
            self.notify(Level::Success, format!("Saved {name}"));
        }
    }

    /// Records where a profile's password now is. False if the profile is
    /// gone (perhaps deleted by another dbtui instance).
    fn set_credential(&mut self, id: &str, store: CredentialStore) -> bool {
        self.update_config(|c| {
            if let Some(p) = c.connections.iter_mut().find(|p| p.id == id) {
                p.credential = store;
            }
        });
        self.profile(id).is_some()
    }

    /// Saves a password for profile `id`, bound to `endpoint`, using the
    /// configured backend (keyring first, vault as fallback).
    fn store_password(&mut self, id: &str, endpoint: Endpoint, password: Zeroizing<String>) {
        let generation = self.credentials.bump(id);
        let request = SaveRequest { id: id.to_string(), endpoint, password, generation, quiet: false };
        if self.config.secret_backend == SecretBackend::Vault {
            return self.store_in_vault(request);
        }
        self.store_in_keyring(request);
    }

    /// Saves a password that was saved before passwords were bound to their
    /// server again, bound, where it already is.
    fn rebind_password(&mut self, profile: &ConnectionProfile, password: Zeroizing<String>) {
        let generation = self.credentials.bump(&profile.id);
        let request =
            SaveRequest { id: profile.id.clone(), endpoint: Endpoint::of(profile), password, generation, quiet: true };
        match profile.credential {
            CredentialStore::Keyring => self.store_in_keyring(request),
            CredentialStore::Vault => self.store_in_vault(request),
            CredentialStore::None => {}
        }
    }

    /// The keyring can block for a long time (e.g. waiting to be unlocked),
    /// so it is never called on the UI thread; the result comes back as
    /// `AppEvent::KeyringStored`, whenever the keyring answers.
    fn store_in_keyring(&mut self, request: SaveRequest) {
        let generation = request.generation;
        self.credentials.last_keyring_op.insert(request.id.clone(), generation);
        self.credentials.pending.insert(generation, (Instant::now(), false));
        let tx = self.tx.clone();
        let (id, endpoint, password) = (request.id.clone(), request.endpoint.clone(), request.password.clone());
        self.keyring.set(&id, &endpoint, &password, move |result| {
            let _ = tx.send(AppEvent::KeyringStored(request, result.map_err(err_text)));
        });
    }

    fn keyring_stored(&mut self, request: SaveRequest, result: Result<(), String>) {
        let SaveRequest { id, generation, quiet, .. } = &request;
        self.credentials.pending.remove(generation);
        if self.credentials.generation(id) != *generation || self.profile(id).is_none() {
            // Stale: the profile was deleted, or its password saved again or
            // no longer saved, while the keyring was busy. Unless a later
            // keyring operation for it is queued (it runs after this one, and
            // decides what the keyring holds), remove what this one wrote.
            if self.credentials.last_keyring_op.get(id) == Some(generation) {
                self.keyring_delete(id, false);
            }
            return;
        }
        match result {
            Ok(()) => {
                let id = id.clone();
                if !self.set_credential(&id, CredentialStore::Keyring) {
                    return self.keyring_delete(&id, false);
                }
                // The password may have been in the vault until now.
                self.vault_delete(&id, false);
                if !quiet {
                    self.notify(Level::Success, "Password saved in the OS keyring");
                }
            }
            // A password that is already saved stays where it is.
            Err(_) if *quiet => {}
            // Don't replace whatever dialog the user opened in the meantime with the vault prompt.
            Err(e)
                if self.config.secret_backend == SecretBackend::Keyring
                    || (self.vault.is_none() && self.popup.is_some()) =>
            {
                self.notify(Level::Error, format!("Could not save password: {e}"));
            }
            Err(_) => self.store_in_vault(request),
        }
    }

    fn store_in_vault(&mut self, request: SaveRequest) {
        // Saved again or forgotten, or the profile deleted, meanwhile (e.g.
        // while the vault prompt was open).
        if self.credentials.generation(&request.id) != request.generation || self.profile(&request.id).is_none() {
            return;
        }
        let Some(vault) = &self.vault else {
            return self.open_vault_prompt(AfterUnlock::Store(request));
        };
        let SaveRequest { id, endpoint, password, quiet, .. } = &request;
        if let Err(e) = vault.set(id, endpoint, password) {
            if e.is::<VaultChanged>() {
                self.vault = None;
            }
            return self.notify(Level::Error, format!("Could not save password: {}", err_text(e)));
        }
        let previous = self.profile(id).map(|p| p.credential);
        if !self.set_credential(id, CredentialStore::Vault) {
            return self.vault_delete(id, false);
        }
        // The keyring may still hold an older copy.
        if previous == Some(CredentialStore::Keyring) || self.credentials.last_keyring_op.contains_key(id) {
            self.keyring_delete(id, previous == Some(CredentialStore::Keyring));
        }
        if !quiet {
            self.notify(Level::Success, "Password saved in the encrypted vault");
        }
    }

    /// Removes a profile's saved password from both stores (it may have been
    /// saved in either one at some point), and makes saves still in progress
    /// stale, so that their results are removed rather than recorded.
    fn forget_password(&mut self, id: &str, credential: CredentialStore) {
        self.credentials.bump(id);
        self.vault_delete(id, credential == CredentialStore::Vault);
        self.keyring_delete(id, credential == CredentialStore::Keyring);
    }

    /// Removes a profile's vault entry, if any; the vault need not be unlocked.
    fn vault_delete(&mut self, id: &str, report: bool) {
        if let Err(e) = self.vault_path().and_then(|path| Vault::remove(&path, id))
            && report
        {
            self.notify(Level::Warning, format!("Could not remove saved password: {}", err_text(e)));
        }
    }

    fn keyring_delete(&mut self, id: &str, report: bool) {
        let op = self.credentials.next();
        self.credentials.last_keyring_op.insert(id.to_string(), op);
        self.credentials.pending.insert(op, (Instant::now(), true));
        let tx = self.tx.clone();
        self.keyring.delete(id, move |result| {
            let _ = tx.send(AppEvent::KeyringRemoved { op, report, result: result.map_err(err_text) });
        });
    }

    /// Whether keyring work is still in progress: a password being saved or
    /// removed, whose result should be recorded before quitting.
    pub fn keyring_busy(&self) -> bool {
        !self.credentials.pending.is_empty()
    }

    /// Saves the password a connection worked with, if asked to.
    fn remember_password(&mut self, connected: &ConnectionProfile, remember: Remember) {
        // Only for the profile as it was connected: not if it has been
        // deleted or now points to another server, or if its password was
        // saved or forgotten in the meantime.
        let Some(current) = self.profile(&connected.id).cloned() else { return };
        let endpoint = Endpoint::of(connected);
        if Endpoint::of(&current) != endpoint || self.credentials.generation(&current.id) != remember.generation {
            return;
        }
        if remember.rebind {
            self.rebind_password(&current, remember.password);
        } else {
            self.store_password(&current.id, endpoint, remember.password);
        }
    }

    fn open_vault_prompt(&mut self, then: AfterUnlock) {
        let create = match self.vault_path() {
            Ok(p) => !Vault::exists(&p),
            Err(e) => return self.notify(Level::Error, err_text(e)),
        };
        self.popup = Some(Popup::Vault(VaultPrompt {
            create,
            input: Input::default(),
            confirm: Input::default(),
            focus: 0,
            error: None,
            then,
            working: None,
        }));
    }

    /// Checks the master password in the background: deriving the key is
    /// deliberately slow. The result comes back as `AppEvent::VaultOpened`.
    fn submit_vault(&mut self, mut v: VaultPrompt) -> Option<Popup> {
        let path = match self.vault_path() {
            Ok(p) => p,
            Err(e) => {
                v.error = Some(err_text(e));
                return Some(Popup::Vault(v));
            }
        };
        if v.create && v.input.value() != v.confirm.value() {
            v.error = Some("Passwords do not match".into());
            return Some(Popup::Vault(v));
        }
        let master = Zeroizing::new(v.input.value().to_string());
        self.vault_seq += 1;
        let (seq, create, tx) = (self.vault_seq, v.create, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let open = || if create { Vault::create(&path, &master) } else { Vault::unlock(&path, &master) };
            let result = crate::terminal::catch_panic_blocking(open)
                .unwrap_or_else(|panic| Err(anyhow::anyhow!("internal error: {panic}")));
            let _ = tx.send(AppEvent::VaultOpened { seq, create, result });
        });
        v.working = Some(seq);
        v.error = None;
        Some(Popup::Vault(v))
    }

    fn vault_opened(&mut self, seq: u64, create: bool, result: anyhow::Result<Vault>) {
        let prompt = match self.popup.take() {
            Some(Popup::Vault(v)) if v.working == Some(seq) => Some(v),
            other => {
                self.popup = other;
                None
            }
        };
        match (result, prompt) {
            (Ok(vault), Some(v)) => {
                self.vault = Some(vault);
                match v.then {
                    AfterUnlock::Connect(profile) => self.connect(profile),
                    AfterUnlock::Store(request) => self.store_in_vault(request),
                }
            }
            // Cancelled while it was created: no vault after all.
            (Ok(vault), None) if create => {
                if !vault.discard_if_empty().unwrap_or(false) {
                    self.vault = Some(vault);
                }
            }
            // Cancelled while it was checked: keep it unlocked, nothing more.
            (Ok(vault), None) => self.vault = Some(vault),
            (Err(e), Some(mut v)) => {
                v.working = None;
                v.input.set("");
                v.confirm.set("");
                v.focus = 0;
                v.error = Some(if e.is::<VaultExists>() {
                    // Created meanwhile, by another dbtui instance: never replaced.
                    v.create = false;
                    "A vault has just been created by another dbtui instance: enter its master password".into()
                } else {
                    err_text(e)
                });
                self.popup = Some(Popup::Vault(v));
            }
            (Err(_), None) => {}
        }
    }

    // ------------------------------------------------------------------
    // Data loading
    // ------------------------------------------------------------------

    fn open_selected_table(&mut self) {
        let Some(b) = &self.browser else { return };
        let Some(TreeItem::Table(s, t)) = b.selected_item() else { return };
        let table = b.schemas[s].tables[t].table.clone();
        if b.table.as_ref().is_some_and(|t| t.info.table == table) {
            if let Some(b) = &mut self.browser {
                b.focus = Focus::Main;
                if b.tab == Tab::Query {
                    b.tab = Tab::Data;
                }
            }
            return;
        }
        if b.commit.is_some() {
            return self.notify(Level::Warning, "A commit is running — wait for its result");
        }
        if b.has_pending_edits() {
            self.popup = Some(Popup::Confirm {
                title: "Discard changes".into(),
                message: format!("Discard {} uncommitted change(s) and open {}?", b.pending_count(), table),
                action: ConfirmAction::DiscardAndOpen(table),
            });
            return;
        }
        self.open_table(&table);
    }

    fn open_table(&mut self, table: &TableRef) {
        let page_size = self.config.page_size.clamp(10, 10_000);
        let Some(b) = &mut self.browser else { return };
        let Some(info) = b.schemas.iter().flat_map(|s| &s.tables).find(|t| t.table == *table).cloned() else { return };
        let mut info = info;
        info.privileges = b.effective(info.privileges);
        b.table = Some(OpenTable::new(info, page_size));
        b.focus = Focus::Main;
        if b.tab == Tab::Query {
            b.tab = Tab::Data;
        }
        self.request_page();
    }

    /// Reloads the open table (whichever tab is shown), unless that would
    /// drop staged changes or come before a commit's result.
    fn request_page(&mut self) {
        if self.browser.as_ref().is_none_or(|b| b.table.is_none()) || self.reload_blocked() {
            return;
        }
        let Some(b) = &mut self.browser else { return };
        // The filter is user SQL: same read-only guard as the SQL editor.
        let read_only = b.fully_read_only();
        let Some(t) = &mut b.table else { return };
        b.seq += 1;
        let (generation, seq) = (b.generation, b.seq);
        t.loading = true;
        t.total = None;
        let db = b.db.clone();
        let table = t.info.table.clone();
        let known_structure = t.structure.clone();
        let mut req = PageRequest {
            filter: t.filter.clone(),
            order_by: t.order_by.clone(),
            default_order: Vec::new(),
            limit: t.page_size,
            offset: t.page * t.page_size,
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let structure = match &known_structure {
                Some(_) => None,
                None => Some(guarded(async { db.structure(&table).await.map_err(err_text) }).await),
            };
            let columns = match (&known_structure, &structure) {
                (Some(s), _) | (None, Some(Ok(s))) => s.columns.as_slice(),
                _ => &[],
            };
            req.default_order = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
            let data = guarded(async { db.fetch_page(&table, &req, read_only).await.map_err(err_text) }).await;
            let ok = data.is_ok();
            let _ = tx.send(AppEvent::Page { generation, seq, structure, data });
            if ok {
                let filter = req.filter.as_deref();
                let count = guarded(async { db.count_rows(&table, filter, read_only).await.map_err(err_text) }).await;
                let _ = tx.send(AppEvent::Count { generation, seq, count });
            }
        });
    }

    fn refresh_schemas(&mut self) {
        let Some(b) = &mut self.browser else { return };
        b.schema_seq += 1;
        let (generation, seq, db) = (b.generation, b.schema_seq, b.db.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = guarded(async { db.schemas().await.map_err(err_text) }).await;
            let _ = tx.send(AppEvent::Schemas { generation, seq, result });
        });
    }

    fn run_query(&mut self) {
        let Some(b) = &mut self.browser else { return };
        let sql = b.query.sql();
        if sql.trim().is_empty() {
            return self.notify(Level::Info, "Nothing to run");
        }
        let read_only = b.fully_read_only();
        if read_only && let Some(keyword) = sql::write_keyword(b.db.driver(), &sql) {
            let reason =
                b.session.read_only_reason.clone().unwrap_or_else(|| "your account has no write permissions".into());
            b.query.message = Some((format!("🔒 Refused {keyword}: read-only session ({reason})"), true));
            return self.notify(Level::Warning, format!("🔒 Read-only: refused {keyword}"));
        }
        b.query.seq += 1;
        b.query.running = true;
        b.query.message = Some(("Running…".into(), false));
        let (generation, seq) = (b.generation, b.query.seq);
        let may_write = sql::write_keyword(b.db.driver(), &sql).is_some();
        let db = b.db.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = guarded(async { db.run_query(&sql, read_only).await.map_err(err_text) }).await;
            let _ = tx.send(AppEvent::Query { generation, seq, may_write, result });
        });
    }

    fn begin_commit(&mut self) {
        let Some(b) = &self.browser else { return };
        if b.commit.is_some() {
            return self.notify(Level::Warning, "A commit is already running — wait for its result");
        }
        let Some(t) = &b.table else { return self.notify(Level::Info, "Nothing to commit") };
        if !t.grid.has_pending() {
            return self.notify(Level::Info, "Nothing to commit");
        }
        if let Some(reason) = &b.session.read_only_reason {
            return self.deny(format!("Read-only session: {reason}"));
        }
        if t.loading {
            return self.notify(Level::Warning, "The table is loading — wait for it to finish");
        }
        let Some(structure) = &t.structure else { return };
        match t.grid.build_statements(b.db.driver(), &t.info.table, structure) {
            Ok(statements) => {
                let non_transactional = structure.non_transactional_engine.clone();
                let snapshot = CommitSnapshot {
                    generation: b.generation,
                    table: t.info.table.clone(),
                    grid: t.grid.id,
                    edits: t.grid.edits.clone(),
                };
                let previews = statements.iter().map(commit_preview).collect();
                let hidden = statements.iter().any(|s| crate::util::has_hidden(&s.preview()));
                self.popup = Some(Popup::Commit {
                    statements,
                    previews,
                    hidden,
                    scroll: 0,
                    non_transactional,
                    snapshot: Box::new(snapshot),
                })
            }
            Err(e) => {
                self.popup = Some(Popup::Message { title: "Cannot commit".into(), body: e, error: true, scroll: 0 })
            }
        }
    }

    /// Runs the statements of a commit preview, if they still are what the
    /// preview showed: the same connection, table and staged changes.
    fn apply_statements(
        &mut self,
        statements: Vec<Statement>,
        non_transactional: Option<String>,
        snapshot: CommitSnapshot,
    ) {
        let Some(b) = &mut self.browser else { return };
        let current = b.table.as_ref().is_some_and(|t| {
            t.info.table == snapshot.table && t.grid.id == snapshot.grid && t.grid.edits.same_changes(&snapshot.edits)
        });
        let refused = if b.generation != snapshot.generation {
            Some("the connection is not the one the changes were made on")
        } else if b.commit.is_some() {
            Some("another commit is still running")
        } else if !current {
            Some("the table or its staged changes are no longer the ones previewed")
        } else {
            None
        };
        if let Some(reason) = refused {
            return self.notify(Level::Error, format!("Nothing was committed: {reason}"));
        }
        b.commit_seq += 1;
        let (generation, seq, profile) = (b.generation, b.commit_seq, b.profile.name.clone());
        b.commit = Some(Committing { seq, snapshot });
        self.busy = Some(format!("Committing {} statement(s)", statements.len()));
        // The database's "rolled back" is not true for these engines.
        let note = non_transactional.filter(|_| statements.len() > 1).map(|engine| {
            format!(
                "\n\n⚠ {engine} tables do not support transactions: statements that ran before the failing \
                 one were kept. Reload the table to see what changed."
            )
        });
        let db = b.db.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = match crate::terminal::catch_panic(db.apply(&statements)).await {
                Ok(result) => result.map_err(|e| err_text(e) + note.as_deref().unwrap_or_default()),
                // It may have happened before or after the commit itself.
                Err(panic) => Err(format!(
                    "internal error: {panic}\n\nThe changes may or may not have been committed: reload the table \
                     to see."
                )),
            };
            let _ = tx.send(AppEvent::Committed { generation, seq, profile, result });
        });
    }

    // ------------------------------------------------------------------
    // Background results
    // ------------------------------------------------------------------

    pub fn on_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected { attempt, profile, db, session, schemas } => {
                if self.connecting != Some(attempt) {
                    // Another connection was asked for since: this one goes.
                    tokio::spawn(async move { db.close().await });
                    return;
                }
                self.connecting = None;
                self.busy = None;
                if let Some(remember) = self.to_remember.take() {
                    self.remember_password(&profile, remember);
                }
                if let Some(old) = self.browser.take() {
                    tokio::spawn(async move { old.db.close().await });
                }
                let browser = Browser::new(attempt, profile, db, *session, schemas);
                let level = browser.access_level();
                let mut msg = format!("Connected as {} · {}", browser.session.user, level.label());
                if let Some(reason) = &browser.session.read_only_reason {
                    msg.push_str(&format!(" ({reason})"));
                }
                let mut level = Level::Success;
                if let Some(warning) = browser.profile.tls_warning() {
                    msg.push_str(&format!(" · ⚠ {}: {warning}", browser.profile.ssl_mode.label()));
                    level = Level::Warning;
                }
                self.notify(level, msg);
                self.browser = Some(browser);
                self.screen = Screen::Browser;
            }
            AppEvent::ConnectFailed { attempt, profile, error, needs_password } => {
                if self.connecting != Some(attempt) {
                    return;
                }
                self.connecting = None;
                self.to_remember = None;
                self.busy = None;
                if needs_password {
                    self.ask_password(profile, Some(error));
                } else {
                    self.popup = Some(Popup::Message {
                        title: format!("Could not connect to {}", profile.name),
                        body: error,
                        error: true,
                        scroll: 0,
                    });
                }
            }
            AppEvent::SavedPassword { attempt, profile, result } => {
                if self.connecting == Some(attempt) {
                    self.connect_with_saved(profile, result.map_err(anyhow::Error::msg), "the OS keyring")
                }
            }
            AppEvent::KeyringStored(request, result) => self.keyring_stored(request, result),
            AppEvent::KeyringRemoved { op, report, result } => {
                self.credentials.pending.remove(&op);
                if let Err(e) = result
                    && report
                {
                    self.notify(Level::Warning, format!("Could not remove saved password: {e}"));
                }
            }
            AppEvent::VaultOpened { seq, create, result } => self.vault_opened(seq, create, result),
            AppEvent::TestResult(result) => {
                self.busy = None;
                match result {
                    Ok((level, msg)) => self.notify(level, msg),
                    Err(e) => {
                        if let Some(Popup::Form(form)) = &mut self.popup {
                            form.error = Some(e.clone());
                        }
                        self.notify(Level::Error, e);
                    }
                }
            }
            AppEvent::Schemas { generation, seq, result } => {
                let Some(b) = self.browser.as_mut().filter(|b| b.generation == generation && b.schema_seq == seq)
                else {
                    return;
                };
                match result {
                    Ok(schemas) => b.set_schemas(schemas),
                    Err(e) => self.notify(Level::Error, e),
                }
            }
            AppEvent::Page { generation, seq, structure, data } => {
                let Some(b) = self.browser.as_mut().filter(|b| b.generation == generation && b.seq == seq) else {
                    return;
                };
                let Some(t) = &mut b.table else { return };
                t.loading = false;
                if let Some(structure) = structure {
                    match structure {
                        Ok(s) => t.structure = Some(s),
                        Err(e) => t.error = Some(e),
                    }
                }
                // Staged changes are never dropped (reloads wait for them to
                // be committed or discarded, so this is not expected): they
                // stay on the rows they were made on.
                let pending = t.grid.pending_count();
                match data {
                    Ok(_) if pending > 0 => self.notify(
                        Level::Warning,
                        format!("Not reloaded: {pending} pending change(s) are kept on the rows shown"),
                    ),
                    Ok(rs) => {
                        let columns = if rs.columns.is_empty() {
                            t.structure
                                .as_ref()
                                .map(|s| s.columns.iter().map(|c| c.name.clone()).collect())
                                .unwrap_or_default()
                        } else {
                            rs.columns
                        };
                        let (row, col) = (t.grid.row, t.grid.col);
                        let mut grid = Grid::new(columns, rs.rows).with_kinds(rs.kinds);
                        grid.row = row.min(grid.total_rows().saturating_sub(1));
                        grid.col = col.min(grid.columns.len().saturating_sub(1));
                        t.grid = grid;
                        t.loaded = true;
                        t.error = None;
                    }
                    Err(e) => {
                        // Don't leave stale rows on screen under a new filter/page.
                        if pending == 0 {
                            t.grid.clear_rows();
                        }
                        t.error = Some(e.clone());
                        self.notify(Level::Error, e);
                    }
                }
                self.check_cell_editor();
            }
            AppEvent::Count { generation, seq, count } => {
                if let Some(b) = &mut self.browser
                    && (b.generation, b.seq) == (generation, seq)
                    && let Some(t) = &mut b.table
                {
                    t.total = count.ok();
                }
            }
            AppEvent::Query { generation, seq, may_write, result } => {
                let Some(b) = self.browser.as_mut().filter(|b| b.generation == generation && b.query.seq == seq) else {
                    return;
                };
                b.query.running = false;
                match result {
                    Ok(rs) => {
                        let ms = rs.elapsed.as_secs_f64() * 1000.0;
                        // Statements may have changed the schema or the open table
                        // (PostgreSQL also counts the rows a SELECT returns).
                        let changed = may_write && (rs.rows_affected > 0 || rs.columns.is_empty());
                        let (rows, columns, affected, truncated) =
                            (rs.rows.len(), rs.columns.is_empty(), rs.rows_affected, rs.truncated);
                        b.query.result =
                            (!rs.columns.is_empty()).then(|| Grid::new(rs.columns, rs.rows).with_kinds(rs.kinds));
                        let cut = b.query.result.as_ref().map_or(0, Grid::cut_count);
                        let mut msg = if columns {
                            format!("✓ {affected} row(s) affected · {ms:.1} ms")
                        } else {
                            let more = if truncated { format!(" (first {rows} shown)") } else { String::new() };
                            format!("✓ {rows} row(s){more} · {ms:.1} ms")
                        };
                        if cut > 0 {
                            msg.push_str(&format!(" · ⚠ {cut} large value(s) only partly loaded"));
                        }
                        b.query.message = Some((msg, false));
                        if changed {
                            self.refresh_schemas();
                            self.reload_after_sql();
                        }
                    }
                    Err(e) => b.query.message = Some((format!("✗ {e}"), true)),
                }
            }
            AppEvent::Committed { generation, seq, profile, result } => {
                self.committed(generation, seq, profile, result)
            }
        }
    }

    /// After SQL that may have written: the open table's rows may be stale,
    /// and a row's key may now belong to another record.
    fn reload_after_sql(&mut self) {
        let Some(b) = &self.browser else { return };
        let Some(t) = &b.table else { return };
        if b.commit.is_some() {
            // Reloaded once the commit is done.
        } else if t.grid.has_pending() {
            let message = format!(
                "The SQL may have changed {}: check the rows of your {} pending change(s) before committing",
                t.info.table.name,
                t.grid.pending_count()
            );
            self.notify(Level::Warning, message);
        } else {
            self.request_page();
        }
    }

    fn committed(&mut self, generation: u64, seq: u64, profile: String, result: Result<u64, String>) {
        let Some(b) = self.browser.as_mut().filter(|b| b.generation == generation) else {
            // From a connection closed while the commit ran.
            return match result {
                Ok(n) => self.notify(Level::Info, format!("The commit to {profile} finished: {n} row(s) affected")),
                Err(e) => self.notify(Level::Error, format!("The commit to {profile} failed: {e}")),
            };
        };
        let Some(commit) = b.commit.take_if(|c| c.seq == seq) else { return };
        self.busy = None;
        match result {
            Ok(n) => {
                let snapshot = commit.snapshot;
                if let Some(t) = self.browser.as_mut().and_then(|b| b.table.as_mut())
                    && t.info.table == snapshot.table
                    && t.grid.id == snapshot.grid
                {
                    t.grid.remove_committed(&snapshot.edits);
                }
                self.check_cell_editor();
                self.notify(Level::Success, format!("Committed — {n} row(s) affected"));
                self.request_page();
            }
            Err(e) => {
                self.popup = Some(Popup::Message { title: "Commit failed".into(), body: e, error: true, scroll: 0 })
            }
        }
    }
}

/// JSON values re-indented for the cell viewer; `None` for anything else.
fn pretty_value(text: &str) -> Option<String> {
    let t = text.trim();
    if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
        return crate::util::pretty_json(t);
    }
    None
}
