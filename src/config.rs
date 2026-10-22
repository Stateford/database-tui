//! Persistent, non-secret configuration: preferences and connection profiles.
//! Passwords are never written here; see [`crate::secrets`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::db::Driver;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeymapMode {
    #[default]
    Standard,
    Vim,
}

impl KeymapMode {
    pub fn label(self) -> &'static str {
        match self {
            KeymapMode::Standard => "STANDARD",
            KeymapMode::Vim => "VIM",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            KeymapMode::Standard => KeymapMode::Vim,
            KeymapMode::Vim => KeymapMode::Standard,
        }
    }
}

/// Where saved passwords go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecretBackend {
    /// OS keyring when available, otherwise the encrypted vault.
    #[default]
    Auto,
    Keyring,
    Vault,
}

/// Where the password of a particular profile is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStore {
    /// Not saved: asked for on every connect.
    #[default]
    None,
    Keyring,
    Vault,
}

impl CredentialStore {
    pub fn label(self) -> &'static str {
        match self {
            CredentialStore::None => "ask on connect",
            CredentialStore::Keyring => "OS keyring",
            CredentialStore::Vault => "encrypted vault",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 5] =
        [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionProfile {
    /// Stable identifier, also used as the key in the credential store.
    pub id: String,
    pub name: String,
    pub driver: Driver,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    /// Database file (SQLite).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Force a read-only session regardless of the user's privileges.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub credential: CredentialStore,
}

impl ConnectionProfile {
    pub fn new(driver: Driver) -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            driver,
            host: if driver.is_file_based() { String::new() } else { "localhost".into() },
            port: driver.default_port(),
            user: String::new(),
            database: String::new(),
            path: String::new(),
            ssl_mode: SslMode::default(),
            read_only: false,
            credential: CredentialStore::None,
        }
    }

    pub fn needs_password(&self) -> bool {
        !self.driver.is_file_based()
    }

    /// `user@host:port/db` or the file path for SQLite.
    pub fn target(&self) -> String {
        if self.driver.is_file_based() {
            return self.path.clone();
        }
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(if self.host.is_empty() { "localhost" } else { &self.host });
        if let Some(port) = self.port {
            s.push_str(&format!(":{port}"));
        }
        if !self.database.is_empty() {
            s.push('/');
            s.push_str(&self.database);
        }
        s
    }
}

pub fn new_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn default_page_size() -> usize {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub keymap: KeymapMode,
    #[serde(default)]
    pub secret_backend: SecretBackend,
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    #[serde(default, rename = "connection")]
    pub connections: Vec<ConnectionProfile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keymap: KeymapMode::default(),
            secret_backend: SecretBackend::default(),
            page_size: default_page_size(),
            connections: Vec::new(),
        }
    }
}

/// Directory holding `config.toml` and `vault.toml`.
/// `DBTUI_CONFIG_DIR` overrides the platform default.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DBTUI_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "dbtui")
        .map(|d| d.config_dir().to_path_buf())
        .context("could not determine a configuration directory")
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Config> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }
}

/// Atomically writes a file that only the current user can read.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config { keymap: KeymapMode::Vim, ..Default::default() };
        let mut p = ConnectionProfile::new(Driver::Postgres);
        p.name = "prod".into();
        p.user = "app".into();
        p.credential = CredentialStore::Keyring;
        cfg.connections.push(p.clone());
        cfg.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("password"));
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.keymap, KeymapMode::Vim);
        assert_eq!(loaded.connections, vec![p]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.page_size, 200);
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn target_strings() {
        let mut p = ConnectionProfile::new(Driver::MySql);
        p.user = "root".into();
        p.database = "shop".into();
        assert_eq!(p.target(), "root@localhost:3306/shop");
    }
}
