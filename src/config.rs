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

/// TLS mode, named as in libpq. With sqlx only `verify-full` authenticates
/// the server: `prefer` and `require` accept any certificate (and `prefer`
/// falls back to plain TCP), and `verify-ca` checks the chain but not the
/// host name, against the public web CAs as well as the profile's CA file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
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

    /// Default for a new profile, and for one saved without a mode: a server
    /// reached over a network has to prove who it is, otherwise whoever sits
    /// on the path receives the password. Local servers rarely have a
    /// certificate and are not exposed that way.
    pub fn default_for_host(host: &str) -> SslMode {
        if is_local_host(host) { SslMode::Prefer } else { SslMode::VerifyFull }
    }
}

/// True when connecting to `host` does not cross a network: `localhost`, a
/// loopback address or a Unix socket directory. An empty host means
/// `localhost` (see `Database::connect`).
pub fn is_local_host(host: &str) -> bool {
    use std::net::IpAddr;
    let host = host.trim();
    if host.is_empty()
        || host.starts_with('/')
        || ["localhost", "localhost."].iter().any(|l| host.eq_ignore_ascii_case(l))
    {
        return true;
    }
    let ip = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback()),
        Err(_) => false,
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
    /// Missing in the file: the default for the host (see [`Config::load_from`]).
    #[serde(default = "secure_ssl_mode")]
    pub ssl_mode: SslMode,
    /// PEM file with the CA certificate(s) the server's certificate must
    /// chain to, for servers whose CA is not a public one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssl_ca: String,
    /// Force a read-only session regardless of the user's privileges.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub credential: CredentialStore,
}

fn secure_ssl_mode() -> SslMode {
    SslMode::VerifyFull
}

impl ConnectionProfile {
    pub fn new(driver: Driver) -> Self {
        let host = if driver.is_file_based() { String::new() } else { "localhost".to_string() };
        Self {
            id: new_id(),
            name: String::new(),
            driver,
            ssl_mode: SslMode::default_for_host(&host),
            host,
            port: driver.default_port(),
            user: String::new(),
            database: String::new(),
            path: String::new(),
            ssl_ca: String::new(),
            read_only: false,
            credential: CredentialStore::None,
        }
    }

    pub fn needs_password(&self) -> bool {
        !self.driver.is_file_based()
    }

    /// Whether the connection stays on this machine (a file, a socket or a
    /// loopback address).
    pub fn is_local(&self) -> bool {
        self.driver.is_file_based() || is_local_host(&self.host)
    }

    /// Refuses TLS settings that would not protect what the user expects.
    pub fn check_tls(&self) -> Result<(), String> {
        if self.driver.is_file_based() {
            return Ok(());
        }
        match self.ssl_mode {
            // As in libpq: without its own CA, verify-ca would accept any
            // certificate from a public CA, for any host name.
            SslMode::VerifyCa if self.ssl_ca.trim().is_empty() => {
                Err("verify-ca needs a CA file; use verify-full to check the server against public CAs".into())
            }
            // libpq would verify the chain here; sqlx does not.
            SslMode::Disable | SslMode::Prefer | SslMode::Require if !self.ssl_ca.trim().is_empty() => {
                Err(format!("a CA file is only used by verify-ca and verify-full, not by {}", self.ssl_mode.label()))
            }
            _ => Ok(()),
        }
    }

    /// Why a server reached over a network is not authenticated by this
    /// profile's TLS mode: whoever is on the path could then read the
    /// password. `None` for verify-full and for local connections.
    pub fn tls_warning(&self) -> Option<&'static str> {
        if self.is_local() {
            return None;
        }
        Some(match self.ssl_mode {
            SslMode::Disable => "not encrypted: the password can be read in transit",
            SslMode::Prefer => "server not verified, TLS optional: use verify-full",
            SslMode::Require => "server not verified: use verify-full",
            SslMode::VerifyCa => "server host name not checked: use verify-full",
            SslMode::VerifyFull => return None,
        })
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
            Ok(text) => Self::parse(&text).with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    fn parse(text: &str) -> Result<Config> {
        /// Only the fields needed to tell a missing `ssl_mode` apart.
        #[derive(Deserialize)]
        struct Modes {
            #[serde(default, rename = "connection")]
            connections: Vec<Mode>,
        }
        #[derive(Deserialize)]
        struct Mode {
            ssl_mode: Option<SslMode>,
        }
        let mut config: Config = toml::from_str(text)?;
        let modes: Modes = toml::from_str(text)?;
        // A mode written in the file is kept as is; a missing one gets the
        // same default as a new profile for that host.
        for (p, m) in config.connections.iter_mut().zip(modes.connections) {
            if m.ssl_mode.is_none() {
                p.ssl_mode = SslMode::default_for_host(&p.host);
            }
        }
        Ok(config)
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

    /// Audit M1: only local servers default to a mode that does not
    /// authenticate the server.
    #[test]
    fn tls_mode_defaults_follow_the_host() {
        for local in ["", "localhost", "LocalHost.", "127.0.0.1", "127.8.9.10", "::1", "[::1]", "::ffff:127.0.0.1"] {
            assert!(is_local_host(local), "{local}");
        }
        assert!(is_local_host("/var/run/postgresql"));
        for remote in
            ["db.example.com", "10.0.0.5", "192.168.1.2", "::ffff:10.0.0.1", "[2001:db8::1]", "localhost.evil"]
        {
            assert!(!is_local_host(remote), "{remote}");
            assert_eq!(SslMode::default_for_host(remote), SslMode::VerifyFull);
        }
        assert_eq!(SslMode::default_for_host("localhost"), SslMode::Prefer);
        assert_eq!(ConnectionProfile::new(Driver::Postgres).ssl_mode, SslMode::Prefer);
        assert_eq!(ConnectionProfile::new(Driver::MySql).ssl_mode, SslMode::Prefer);
    }

    /// Saved modes are kept as written; a missing one gets the default for
    /// the host, and the CA file is optional.
    #[test]
    fn saved_tls_settings() {
        let text = r#"
            [[connection]]
            id = "a"
            name = "remote, no mode"
            driver = "postgres"
            host = "db.example.com"

            [[connection]]
            id = "b"
            name = "local, no mode"
            driver = "mysql"
            host = "127.0.0.1"

            [[connection]]
            id = "c"
            name = "remote, explicit"
            driver = "postgres"
            host = "db.example.com"
            ssl_mode = "prefer"
            ssl_ca = "/etc/ssl/db-ca.pem"
        "#;
        let cfg = Config::parse(text).unwrap();
        let modes: Vec<_> = cfg.connections.iter().map(|p| (p.ssl_mode, p.ssl_ca.as_str())).collect();
        assert_eq!(
            modes,
            vec![(SslMode::VerifyFull, ""), (SslMode::Prefer, ""), (SslMode::Prefer, "/etc/ssl/db-ca.pem")]
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("ssl_ca").count(), 1, "{text}");
        assert_eq!(Config::load_from(&path).unwrap().connections, cfg.connections);
    }

    /// Audit M1/M2: modes that do not authenticate a remote server are
    /// flagged, and verify-ca is only accepted with the profile's own CA.
    #[test]
    fn tls_checks_and_warnings() {
        let mut p = ConnectionProfile::new(Driver::Postgres);
        assert_eq!((p.check_tls(), p.tls_warning()), (Ok(()), None));
        p.host = "db.example.com".into();
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa] {
            p.ssl_mode = mode;
            assert!(p.tls_warning().is_some(), "{mode:?}");
        }
        p.ssl_mode = SslMode::VerifyFull;
        assert_eq!((p.check_tls(), p.tls_warning()), (Ok(()), None));

        p.ssl_mode = SslMode::VerifyCa;
        assert!(p.check_tls().unwrap_err().contains("CA file"));
        p.ssl_ca = "ca.pem".into();
        assert!(p.check_tls().is_ok());
        p.ssl_mode = SslMode::Require;
        assert!(p.check_tls().unwrap_err().contains("only used by verify-ca and verify-full"));

        let mut file = ConnectionProfile::new(Driver::Sqlite);
        file.ssl_mode = SslMode::Disable;
        assert_eq!((file.check_tls(), file.tls_warning()), (Ok(()), None));
    }

    #[test]
    fn target_strings() {
        let mut p = ConnectionProfile::new(Driver::MySql);
        p.user = "root".into();
        p.database = "shop".into();
        assert_eq!(p.target(), "root@localhost:3306/shop");
    }
}
