//! Persistent, non-secret configuration: preferences and connection profiles.
//! Passwords are never written here; see [`crate::secrets`].

use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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
/// falls back to plain TCP). `verify-ca` would check the chain but not the
/// host name, against the public web CAs as well as the profile's CA file:
/// any public certificate, for any name, would pass. It is refused (see
/// [`ConnectionProfile::check_tls`]), and only read from older files.
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
    /// The modes that can be chosen.
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

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
            // sqlx always trusts the public CAs as well as the CA file, and
            // verify-ca does not check the host name: a certificate anyone
            // can get for their own domain would pass.
            SslMode::VerifyCa => Err("verify-ca is not supported: with dbtui's driver it would accept any publicly \
                                      trusted certificate, for any host name. Use verify-full (with a CA file for a \
                                      private CA)"
                .into()),
            // libpq would verify the chain here; sqlx does not.
            SslMode::Disable | SslMode::Prefer | SslMode::Require if !self.ssl_ca.trim().is_empty() => {
                Err(format!("a CA file is only used by verify-full, not by {}", self.ssl_mode.label()))
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

    /// Applies `change` to the configuration file as it is on disk now and
    /// saves it, then returns the result. Another dbtui instance may have
    /// changed the file since this one loaded it: the file is re-read under
    /// a lock, so its changes are kept rather than overwritten.
    pub fn update(change: impl FnOnce(&mut Config)) -> Result<Config> {
        Self::update_at(&Self::path()?, change)
    }

    pub fn update_at(path: &Path, change: impl FnOnce(&mut Config)) -> Result<Config> {
        let _lock = lock(path)?;
        let mut config = Self::load_from(path)?;
        change(&mut config);
        config.save_to(path)?;
        Ok(config)
    }

    /// Writes the whole file. Use [`Config::update`] to change it.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }
}

/// How long to wait for another dbtui instance to finish changing a file.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// An exclusive lock on a file shared with other dbtui instances, held
/// while the file is read, changed and written back. Released on drop.
pub struct FileLock {
    _file: Option<File>,
}

/// Locks `path` against other dbtui instances, through a lock file next to
/// it (`config.lock` for `config.toml`). The lock is advisory: only dbtui
/// takes it. Never call it while holding the same lock: it would wait for
/// itself.
pub fn lock(path: &Path) -> Result<FileLock> {
    create_private_dir(parent_dir(path))?;
    let lock_path = path.with_extension("lock");
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(&lock_path).with_context(|| format!("could not open {}", lock_path.display()))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(FileLock { _file: Some(file) }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(TryLockError::WouldBlock) => {
                bail!("{} is being changed by another dbtui instance; try again", path.display())
            }
            // Some file systems (network shares) cannot lock files.
            Err(TryLockError::Error(e)) if e.kind() == ErrorKind::Unsupported => return Ok(FileLock { _file: None }),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("could not lock {}", lock_path.display()));
            }
        }
    }
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Creates the directory, readable by the current user only, if it does
/// not exist yet. One that exists is made private if it holds nothing but
/// dbtui's files (older versions created it readable by everyone).
fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        #[cfg(unix)]
        make_private(dir);
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).with_context(|| format!("could not create {}", dir.display()))
}

/// Removes the group and other permissions of `dir`, when it belongs to the
/// current user and only holds files dbtui writes there: a directory chosen
/// with `DBTUI_CONFIG_DIR` that holds anything else is left as it is. Best
/// effort: the files themselves are private either way.
#[cfg(unix)]
fn make_private(dir: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::metadata(dir) else { return };
    // SAFETY: plain system call, without arguments.
    if meta.mode() & 0o077 == 0 || meta.uid() != unsafe { libc::geteuid() } {
        return;
    }
    let ours = |name: &std::ffi::OsStr| {
        name.to_str().is_some_and(|n| {
            ["config.toml", "config.lock", "vault.toml", "vault.lock"].contains(&n) || n.starts_with(".dbtui-")
        })
    };
    let Ok(mut entries) = std::fs::read_dir(dir) else { return };
    if entries.all(|e| e.is_ok_and(|e| ours(&e.file_name()))) {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(meta.mode() & 0o7700));
    }
}

/// Atomically writes a file that only the current user can read.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    write_private_file(path, contents, true)
}

/// Like [`write_private`], but fails with [`ErrorKind::AlreadyExists`] when
/// the file exists, even if it appears while this one is written.
pub fn write_private_new(path: &Path, contents: &[u8]) -> Result<()> {
    write_private_file(path, contents, false)
}

fn write_private_file(path: &Path, contents: &[u8], replace: bool) -> Result<()> {
    use std::io::Write;
    let dir = parent_dir(path);
    create_private_dir(dir)?;
    // A new file with a random name, created exclusively (never through a
    // link or over a file someone else prepared), then renamed into place.
    let mut tmp = tempfile::Builder::new()
        .prefix(".dbtui-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(contents)?;
    tmp.as_file().sync_all()?;
    let persisted = if replace { tmp.persist(path) } else { tmp.persist_noclobber(path) };
    persisted.map_err(|e| e.error).with_context(|| format!("could not write {}", path.display()))?;
    // Make the rename itself durable.
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        dir.sync_all().ok();
    }
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

    /// Audit L3: files are written through a new file with a random name,
    /// never through one someone else prepared, and end up 0600 in a 0700
    /// directory. Creating a file never replaces one.
    #[cfg(unix)]
    #[test]
    fn private_files_are_written_safely() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("dbtui");
        let path = sub.join("vault.toml");
        write_private(&path, b"first").unwrap();
        assert_eq!((mode(&sub), mode(&path)), (0o700, 0o600));

        // What could be planted at the fixed temporary names older versions
        // used, and a target whose mode was changed, are of no consequence.
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"important").unwrap();
        std::os::unix::fs::symlink(&victim, sub.join("vault.tmp")).unwrap();
        std::fs::write(sub.join("config.tmp"), b"planted").unwrap();
        std::fs::set_permissions(sub.join("config.tmp"), std::fs::Permissions::from_mode(0o666)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"second").unwrap();
        write_private(&sub.join("config.toml"), b"config").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"important");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!((mode(&path), mode(&sub.join("config.toml"))), (0o600, 0o600));
        let mut names: Vec<_> = std::fs::read_dir(&sub).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["config.tmp", "config.toml", "vault.tmp", "vault.toml"], "no temporary files left");

        let err = write_private_new(&path, b"third").unwrap_err();
        assert_eq!(err.downcast_ref::<std::io::Error>().unwrap().kind(), ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    /// Audit L3: a config directory an older version created readable by
    /// everyone is made private; a directory holding other files (one
    /// chosen with DBTUI_CONFIG_DIR, say) is left alone.
    #[cfg(unix)]
    #[test]
    fn existing_config_directories_are_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("dbtui");
        let shared = dir.path().join("shared");
        for d in [&old, &shared] {
            std::fs::create_dir(d).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
            write_private(&d.join("config.toml"), b"").unwrap();
        }
        assert_eq!(mode(&old), 0o700);
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        Config::update_at(&old.join("config.toml"), |c| c.page_size = 10).unwrap();
        assert_eq!(mode(&old), 0o700, "taken by the lock as well");

        std::fs::write(shared.join("notes.txt"), b"mine").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_private(&shared.join("config.toml"), b"").unwrap();
        assert_eq!(mode(&shared), 0o755);
    }

    /// Two dbtui instances changing the configuration at the same time keep
    /// each other's changes: each change is made to the file as it is.
    #[test]
    fn concurrent_updates_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let stale = Config::load_from(&path).unwrap();
        let writers: Vec<_> = (0..4)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        Config::update_at(&path, |c| {
                            let mut p = ConnectionProfile::new(Driver::Sqlite);
                            p.name = format!("{t}-{i}");
                            c.connections.push(p);
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        // An instance that loaded the file long ago still keeps them all.
        assert!(stale.connections.is_empty());
        let config = Config::update_at(&path, |c| c.keymap = KeymapMode::Vim).unwrap();
        assert_eq!(config.connections.len(), 40);
        assert_eq!(Config::load_from(&path).unwrap().connections.len(), 40);
        assert_eq!(Config::load_from(&path).unwrap().keymap, KeymapMode::Vim);

        // A file that cannot be read is not overwritten.
        std::fs::write(&path, "not = [valid").unwrap();
        assert!(Config::update_at(&path, |c| c.page_size = 5).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid");
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
    /// flagged, and verify-ca, which would accept any public certificate
    /// whatever the CA file, is refused.
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

        p.ssl_ca = "ca.pem".into();
        assert!(p.check_tls().is_ok());
        p.ssl_mode = SslMode::VerifyCa;
        assert!(p.check_tls().unwrap_err().contains("verify-ca is not supported"));
        assert!(!SslMode::ALL.contains(&SslMode::VerifyCa));
        p.ssl_mode = SslMode::Require;
        assert!(p.check_tls().unwrap_err().contains("only used by verify-full"));

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
