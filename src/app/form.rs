//! Create / edit connection profile form.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::input::Input;
use crate::config::{ConnectionProfile, CredentialStore, SslMode};
use crate::db::Driver;
use crate::secrets::Endpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Driver,
    Host,
    Port,
    User,
    Password,
    Database,
    Ssl,
    SslCa,
    Path,
    SavePassword,
    ReadOnly,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Driver => "Driver",
            Field::Host => "Host",
            Field::Port => "Port",
            Field::User => "User",
            Field::Password => "Password",
            Field::Database => "Database",
            Field::Ssl => "SSL mode",
            Field::SslCa => "CA file",
            Field::Path => "File",
            Field::SavePassword => "Save password",
            Field::ReadOnly => "Read-only",
        }
    }

    pub fn is_text(self) -> bool {
        !matches!(self, Field::Driver | Field::Ssl | Field::SavePassword | Field::ReadOnly)
    }
}

pub enum FormOutcome {
    None,
    Cancel,
    Save,
    Test,
}

pub struct ConnectionForm {
    /// The profile being edited (`id`, stored credential) — fields below win.
    pub original: ConnectionProfile,
    pub is_new: bool,
    pub driver: Driver,
    pub name: Input,
    pub host: Input,
    pub port: Input,
    pub user: Input,
    pub password: Input,
    pub database: Input,
    pub path: Input,
    pub ssl: SslMode,
    /// The SSL mode follows the host (see [`SslMode::default_for_host`])
    /// until it is chosen by hand. Only for new profiles: a saved profile
    /// keeps its mode.
    pub ssl_auto: bool,
    pub ssl_ca: Input,
    pub save_password: bool,
    pub read_only: bool,
    pub focus: usize,
    pub error: Option<String>,
}

impl ConnectionForm {
    pub fn new(profile: ConnectionProfile, is_new: bool) -> Self {
        Self {
            driver: profile.driver,
            name: Input::new(&profile.name),
            host: Input::new(&profile.host),
            port: Input::new(profile.port.map(|p| p.to_string()).unwrap_or_default()),
            user: Input::new(&profile.user),
            password: Input::default(),
            database: Input::new(&profile.database),
            path: Input::new(&profile.path),
            ssl: profile.ssl_mode,
            ssl_auto: is_new,
            ssl_ca: Input::new(&profile.ssl_ca),
            save_password: is_new || profile.credential != CredentialStore::None,
            read_only: profile.read_only,
            focus: 0,
            error: None,
            original: profile,
            is_new,
        }
    }

    pub fn fields(&self) -> Vec<Field> {
        match self.driver {
            Driver::Sqlite => vec![Field::Name, Field::Driver, Field::Path, Field::ReadOnly],
            _ => vec![
                Field::Name,
                Field::Driver,
                Field::Host,
                Field::Port,
                Field::User,
                Field::Password,
                Field::Database,
                Field::Ssl,
                Field::SslCa,
                Field::SavePassword,
                Field::ReadOnly,
            ],
        }
    }

    pub fn focused(&self) -> Field {
        let fields = self.fields();
        fields[self.focus.min(fields.len() - 1)]
    }

    pub fn input(&self, field: Field) -> Option<&Input> {
        Some(match field {
            Field::Name => &self.name,
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::User => &self.user,
            Field::Password => &self.password,
            Field::Database => &self.database,
            Field::SslCa => &self.ssl_ca,
            Field::Path => &self.path,
            _ => return None,
        })
    }

    fn input_mut(&mut self, field: Field) -> Option<&mut Input> {
        Some(match field {
            Field::Name => &mut self.name,
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::User => &mut self.user,
            Field::Password => &mut self.password,
            Field::Database => &mut self.database,
            Field::SslCa => &mut self.ssl_ca,
            Field::Path => &mut self.path,
            _ => return None,
        })
    }

    pub fn has_stored_password(&self) -> bool {
        self.original.credential != CredentialStore::None
    }

    /// The server of the saved profile, when the form now points to another
    /// one: its saved password is then not used (see [`Endpoint`]).
    pub fn saved_password_elsewhere(&self) -> Option<Endpoint> {
        let saved = Endpoint::of(&self.original);
        let port = self.port.value().trim().parse().ok();
        let current = Endpoint::new(self.driver, self.host.value(), port, self.user.value());
        (self.has_stored_password() && current != saved).then_some(saved)
    }

    /// Warning about the TLS mode for the host currently in the form.
    pub fn tls_warning(&self) -> Option<&'static str> {
        let mut p = self.original.clone();
        p.driver = self.driver;
        p.host = self.host.value().to_string();
        p.ssl_mode = self.ssl;
        p.tls_warning()
    }

    fn host_changed(&mut self) {
        if self.ssl_auto {
            self.ssl = SslMode::default_for_host(self.host.value());
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.fields().len();
        match key.code {
            KeyCode::Esc => return FormOutcome::Cancel,
            KeyCode::Char('s') if ctrl => return FormOutcome::Save,
            KeyCode::Char('t') if ctrl => return FormOutcome::Test,
            KeyCode::Enter => {
                if self.focus + 1 >= n {
                    return FormOutcome::Save;
                }
                self.focus += 1;
            }
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + n - 1) % n,
            KeyCode::Left | KeyCode::Right if !self.focused().is_text() => {
                self.cycle(if key.code == KeyCode::Left { -1 } else { 1 })
            }
            KeyCode::Char(' ') if !self.focused().is_text() => self.cycle(1),
            _ => {
                let field = self.focused();
                if field == Field::Port && matches!(key.code, KeyCode::Char(c) if !c.is_ascii_digit()) {
                    return FormOutcome::None;
                }
                if let Some(input) = self.input_mut(field) {
                    input.handle_key(key);
                }
                if field == Field::Host {
                    self.host_changed();
                }
            }
        }
        self.error = None;
        FormOutcome::None
    }

    pub fn paste(&mut self, text: &str) {
        let field = self.focused();
        if let Some(input) = self.input_mut(field) {
            input.insert_str(text.lines().next().unwrap_or_default());
        }
        if field == Field::Host {
            self.host_changed();
        }
    }

    fn cycle(&mut self, dir: i32) {
        match self.focused() {
            Field::Driver => {
                let all = Driver::ALL;
                let idx = all.iter().position(|d| *d == self.driver).unwrap_or(0) as i32;
                let new = all[(idx + dir).rem_euclid(all.len() as i32) as usize];
                // Keep the port in sync when it still holds the old default.
                let old_default = self.driver.default_port().map(|p| p.to_string()).unwrap_or_default();
                if self.port.value() == old_default {
                    self.port.set(new.default_port().map(|p| p.to_string()).unwrap_or_default());
                }
                if !new.is_file_based() && self.host.value().is_empty() {
                    self.host.set("localhost");
                    self.host_changed();
                }
                self.driver = new;
                self.focus = 1;
            }
            Field::Ssl => {
                let all = SslMode::ALL;
                self.ssl = match all.iter().position(|s| *s == self.ssl) {
                    Some(idx) => all[(idx as i32 + dir).rem_euclid(all.len() as i32) as usize],
                    // verify-ca, saved by an older version: what it was meant to do.
                    None => SslMode::VerifyFull,
                };
                self.ssl_auto = false;
            }
            Field::SavePassword => self.save_password = !self.save_password,
            Field::ReadOnly => self.read_only = !self.read_only,
            _ => {}
        }
    }

    /// Validates the form and builds the profile (without touching secrets).
    pub fn to_profile(&self) -> Result<ConnectionProfile, String> {
        let name = self.name.value().trim();
        if name.is_empty() {
            return Err("a name is required".into());
        }
        let mut p = self.original.clone();
        p.name = name.to_string();
        p.driver = self.driver;
        p.read_only = self.read_only;
        if self.driver.is_file_based() {
            if self.path.value().trim().is_empty() {
                return Err("a database file is required".into());
            }
            p.path = self.path.value().trim().to_string();
            p.host.clear();
            p.port = None;
            p.user.clear();
            p.database.clear();
            p.ssl_ca.clear();
        } else {
            p.host = self.host.value().trim().to_string();
            p.port = match self.port.value().trim() {
                "" => None,
                s => Some(s.parse().map_err(|_| "port must be a number between 1 and 65535".to_string())?),
            };
            p.user = self.user.value().trim().to_string();
            p.database = self.database.value().trim().to_string();
            p.ssl_mode = self.ssl;
            p.ssl_ca = self.ssl_ca.value().trim().to_string();
            p.path.clear();
            p.check_tls()?;
        }
        Ok(p)
    }
}
