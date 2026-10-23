//! Secure password storage.
//!
//! Two backends are supported:
//! * the operating system keyring (macOS Keychain, Windows Credential
//!   Manager, Secret Service on Linux/BSD);
//! * an encrypted vault file protected by a master password, for systems
//!   without a keyring (headless servers, containers, WSL...). Secrets are
//!   sealed with XChaCha20-Poly1305 using a key derived with Argon2id.
//!
//! Each password is saved together with the server it is for (an
//! [`Endpoint`]). A profile that is edited, or a config file that is
//! changed, to point somewhere else does not get the saved password, so it
//! is never sent to another server.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::config::{ConnectionProfile, lock, write_private, write_private_new};
use crate::db::Driver;

const KEYRING_SERVICE: &str = "dbtui";

/// How long to wait for the OS keyring before telling the user. A locked
/// Secret Service keyring can block until its unlock prompt is answered, or
/// forever when nobody can see the prompt (e.g. on WSL).
pub const KEYRING_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Endpoint binding
// ---------------------------------------------------------------------------

/// The server a password was saved for: driver, host, port and user, with
/// the defaults [`crate::db::Database::connect`] applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub driver: Driver,
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl Endpoint {
    pub fn new(driver: Driver, host: &str, port: Option<u16>, user: &str) -> Endpoint {
        let host = match host.trim() {
            "" => "localhost",
            h => h,
        };
        Endpoint {
            driver,
            host: host.to_string(),
            port: port.or(driver.default_port()).unwrap_or(0),
            user: user.trim().to_string(),
        }
    }

    pub fn of(profile: &ConnectionProfile) -> Endpoint {
        Endpoint::new(profile.driver, &profile.host, profile.port, &profile.user)
    }
}

impl Endpoint {
    /// `user@host:port`, without the driver.
    pub fn address(&self) -> String {
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        format!("{user}{}:{}", self.host, self.port)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.driver {
            Driver::Postgres => "postgres",
            Driver::MySql => "mysql",
            Driver::Sqlite => "sqlite",
        };
        write!(f, "{scheme}://{}", self.address())
    }
}

/// Marks a saved value that carries its endpoint: this line, the endpoint as
/// JSON on one line, then the password. Anything else is a password saved
/// before passwords were bound to a server (form fields never contain a
/// newline, so no such password starts with this).
const BOUND_PREFIX: &str = "dbtui-secret/1\n";

/// What is stored in the keyring or vault for a password.
fn encode(endpoint: &Endpoint, password: &str) -> Zeroizing<String> {
    let endpoint = serde_json::to_string(endpoint).expect("an endpoint serializes");
    // Sized up front: growing the string would leave copies behind.
    let mut out = Zeroizing::new(String::with_capacity(BOUND_PREFIX.len() + endpoint.len() + 1 + password.len()));
    out.push_str(BOUND_PREFIX);
    out.push_str(&endpoint);
    out.push('\n');
    out.push_str(password);
    out
}

/// A saved password and the server it was saved for.
pub struct StoredSecret {
    password: Zeroizing<String>,
    /// `None` for a password saved before passwords were bound to a server.
    endpoint: Option<Endpoint>,
}

impl std::fmt::Debug for StoredSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSecret").field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

impl StoredSecret {
    fn decode(raw: Zeroizing<String>) -> Result<StoredSecret> {
        let Some(rest) = raw.strip_prefix(BOUND_PREFIX) else {
            return Ok(StoredSecret { password: raw, endpoint: None });
        };
        let (endpoint, password) = rest.split_once('\n').context("the saved password is corrupt")?;
        let endpoint = serde_json::from_str(endpoint).context("the saved password is corrupt")?;
        Ok(StoredSecret { password: Zeroizing::new(password.to_string()), endpoint: Some(endpoint) })
    }

    /// False for a password saved before passwords were bound to a server:
    /// it should be saved again, bound, once it has worked.
    pub fn is_bound(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The password, if it may be sent to `endpoint`. A password saved
    /// without its server counts as saved for `legacy`, the server of the
    /// profile as it was saved.
    pub fn password_for(&self, endpoint: &Endpoint, legacy: &Endpoint) -> Result<&Zeroizing<String>, String> {
        let saved_for = self.endpoint.as_ref().unwrap_or(legacy);
        if saved_for == endpoint {
            Ok(&self.password)
        } else {
            Err(format!("The saved password is for {saved_for}, not {endpoint}: enter the password for this server."))
        }
    }
}

// ---------------------------------------------------------------------------
// OS keyring
// ---------------------------------------------------------------------------

/// A credential store holding one secret per profile id: the OS keyring in
/// the app, an in-memory one in tests. Calls block, possibly for a long time
/// (a locked keyring waits for its unlock prompt).
pub trait KeyringBackend: Send + Sync {
    fn set(&self, id: &str, secret: &str) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<Zeroizing<String>>>;
    /// Succeeds when there is nothing to delete.
    fn delete(&self, id: &str) -> Result<()>;
}

/// The operating system keyring.
pub struct OsKeyring;

impl KeyringBackend for OsKeyring {
    fn set(&self, id: &str, secret: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, id)?;
        entry.set_password(secret)?;
        // Verify: some keyrings silently drop writes when locked.
        let back = Zeroizing::new(entry.get_password()?);
        if back.as_str() != secret {
            bail!("keyring did not store the password");
        }
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<Zeroizing<String>>> {
        match keyring::Entry::new(KEYRING_SERVICE, id).and_then(|e| e.get_password()) {
            Ok(pw) => Ok(Some(Zeroizing::new(pw))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn delete(&self, id: &str) -> Result<()> {
        match keyring::Entry::new(KEYRING_SERVICE, id).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

type Job = Box<dyn FnOnce(&dyn KeyringBackend) + Send>;

/// Keyring access for the app. Operations run one at a time, in the order
/// they were requested, on a dedicated thread: some platform backends drive
/// their own executor and must not run on the async runtime, and running
/// them in order means the last request for a profile decides what the
/// keyring holds, however long each one takes.
#[derive(Clone)]
pub struct Keyring {
    backend: Arc<dyn KeyringBackend>,
    jobs: Arc<OnceLock<mpsc::Sender<Job>>>,
}

impl Keyring {
    pub fn os() -> Keyring {
        Keyring::new(Arc::new(OsKeyring))
    }

    pub fn new(backend: Arc<dyn KeyringBackend>) -> Keyring {
        Keyring { backend, jobs: Arc::new(OnceLock::new()) }
    }

    fn run(&self, job: Job) {
        let jobs = self.jobs.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            let backend = self.backend.clone();
            // Ends once every handle is dropped and the queue is empty. Not
            // joined: a keyring stuck on a prompt must not delay exiting.
            std::thread::Builder::new()
                .name("dbtui-keyring".into())
                .spawn(move || rx.into_iter().for_each(|job| job(&*backend)))
                .expect("could not start the keyring thread");
            tx
        });
        let _ = jobs.send(job);
    }

    /// Reads the password saved for profile `id`. Blocks, so call it off the
    /// UI thread, but gives up after [`KEYRING_TIMEOUT`]: a read changes
    /// nothing, so giving up loses nothing.
    pub fn get(&self, id: &str) -> Result<Option<StoredSecret>> {
        let (tx, rx) = mpsc::channel();
        let id = id.to_string();
        self.run(Box::new(move |backend| {
            let _ = tx.send(guarded(|| backend.get(&id)));
        }));
        let raw = match rx.recv_timeout(KEYRING_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(anyhow!("the OS keyring did not respond (is it locked?)")),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("the keyring thread stopped")),
        }
        .context("could not read from the OS keyring")?;
        raw.map(StoredSecret::decode).transpose()
    }

    /// Saves `password` for profile `id`, bound to `endpoint`. `done` gets
    /// the result on the keyring thread whenever the keyring answers, which
    /// for a locked keyring may be after its unlock prompt, long after.
    pub fn set(&self, id: &str, endpoint: &Endpoint, password: &str, done: impl FnOnce(Result<()>) + Send + 'static) {
        let (id, secret) = (id.to_string(), encode(endpoint, password));
        self.run(Box::new(move |backend| {
            done(guarded(|| backend.set(&id, &secret)).context("OS keyring unavailable"))
        }));
    }

    /// Deletes what is saved for profile `id`; see [`Keyring::set`].
    pub fn delete(&self, id: &str, done: impl FnOnce(Result<()>) + Send + 'static) {
        let id = id.to_string();
        self.run(Box::new(move |backend| done(guarded(|| backend.delete(&id)))));
    }
}

/// Runs a keyring call, turning a panic in the platform backend into an
/// error so that its caller always hears back.
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::terminal::catch_panic_blocking(f).unwrap_or_else(|panic| Err(anyhow!("the OS keyring failed: {panic}")))
}

// ---------------------------------------------------------------------------
// Vault
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KdfParams {
    salt: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

/// Argon2id costs for new vaults: 64 MiB and 3 passes. Vaults made with
/// lower costs (older versions used the OWASP minimum, 19 MiB and 2 passes)
/// are re-encrypted with these when unlocked.
const KDF_M_COST: u32 = 64 * 1024;
const KDF_T_COST: u32 = 3;
const KDF_P_COST: u32 = 1;

/// Costs accepted from a vault file. They are stored in the file, so a
/// tampered file could otherwise make unlocking hang or exhaust memory.
const KDF_M_RANGE: RangeInclusive<u32> = 8 * 1024..=1024 * 1024;
const KDF_T_RANGE: RangeInclusive<u32> = 1..=10;
const KDF_P_RANGE: RangeInclusive<u32> = 1..=8;

impl KdfParams {
    /// Today's costs with a fresh random salt.
    fn new() -> KdfParams {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        KdfParams { salt: B64.encode(salt), m_cost: KDF_M_COST, t_cost: KDF_T_COST, p_cost: KDF_P_COST }
    }

    fn is_outdated(&self) -> bool {
        self.m_cost < KDF_M_COST || self.t_cost < KDF_T_COST
    }

    fn check(&self) -> Result<()> {
        if !KDF_M_RANGE.contains(&self.m_cost)
            || !KDF_T_RANGE.contains(&self.t_cost)
            || !KDF_P_RANGE.contains(&self.p_cost)
        {
            bail!(
                "the vault file asks for unsupported key derivation costs (memory {} KiB, {} passes, {} lanes)",
                self.m_cost,
                self.t_cost,
                self.p_cost
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    kdf: KdfParams,
    /// Known plaintext used to check the master password.
    check: Sealed,
    #[serde(default)]
    entries: BTreeMap<String, Sealed>,
}

const CHECK_ID: &str = "__check__";
const CHECK_PLAINTEXT: &[u8] = b"dbtui-vault-v1";

/// [`Vault::create`] found a vault already there, perhaps just created by
/// another dbtui instance. It is left alone.
#[derive(Debug)]
pub struct VaultExists;

impl std::fmt::Display for VaultExists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a password vault already exists")
    }
}

impl std::error::Error for VaultExists {}

/// The vault file no longer opens with the key of this unlocked vault:
/// another dbtui instance replaced or re-encrypted it. Unlock it again.
#[derive(Debug)]
pub struct VaultChanged;

impl std::fmt::Display for VaultChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the password vault was changed by another dbtui instance: unlock it again")
    }
}

impl std::error::Error for VaultChanged {}

/// An unlocked vault. The derived key lives only in memory and is wiped on
/// drop. Entries are always read from the file, and changed under a lock,
/// so that several dbtui instances can share the vault.
pub struct Vault {
    path: PathBuf,
    /// Salt and costs the key was derived with.
    kdf: KdfParams,
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("path", &self.path).finish_non_exhaustive()
    }
}

fn read_vault(path: &Path) -> Result<VaultFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let file: VaultFile = toml::from_str(&text).context("vault file is corrupt")?;
    if file.version != 1 {
        bail!("unsupported vault version {}", file.version);
    }
    Ok(file)
}

fn write_vault(path: &Path, file: &VaultFile) -> Result<()> {
    write_private(path, toml::to_string_pretty(file)?.as_bytes())
}

impl Vault {
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Creates a vault. Never replaces an existing one: if another dbtui
    /// instance has created one in the meantime, this fails with
    /// [`VaultExists`]. Slow (key derivation): call it off the UI thread.
    pub fn create(path: &Path, master: &str) -> Result<Vault> {
        if master.chars().count() < 8 {
            bail!("master password must be at least 8 characters");
        }
        let kdf = KdfParams::new();
        let key = derive_key(master, &kdf)?;
        let file = VaultFile {
            version: 1,
            kdf: kdf.clone(),
            check: seal(&key, CHECK_ID, CHECK_PLAINTEXT)?,
            entries: BTreeMap::new(),
        };
        let _lock = lock(path)?;
        if path.exists() {
            return Err(VaultExists.into());
        }
        if let Err(e) = write_private_new(path, toml::to_string_pretty(&file)?.as_bytes()) {
            if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) {
                return Err(VaultExists.into());
            }
            return Err(e);
        }
        Ok(Vault { path: path.to_path_buf(), kdf, key })
    }

    /// Unlocks a vault, re-encrypting it with today's key derivation costs
    /// if it was made with lower ones. Slow: call it off the UI thread.
    pub fn unlock(path: &Path, master: &str) -> Result<Vault> {
        let file = read_vault(path)?;
        let vault = Vault::open(path, &file, master)?;
        if !file.kdf.is_outdated() {
            return Ok(vault);
        }
        // Best effort: the vault works either way.
        Ok(vault.upgrade(master).unwrap_or(vault))
    }

    fn open(path: &Path, file: &VaultFile, master: &str) -> Result<Vault> {
        let key = derive_key(master, &file.kdf)?;
        match open(&key, CHECK_ID, &file.check) {
            Ok(plain) if plain.as_slice() == CHECK_PLAINTEXT => {}
            _ => bail!("wrong master password"),
        }
        Ok(Vault { path: path.to_path_buf(), kdf: file.kdf.clone(), key })
    }

    /// Re-encrypts every entry under a key derived with today's costs.
    fn upgrade(&self, master: &str) -> Result<Vault> {
        let _lock = lock(&self.path)?;
        let file = read_vault(&self.path)?;
        if file.kdf != self.kdf {
            // Changed by another instance since it was read: use it as it is.
            return Vault::open(&self.path, &file, master);
        }
        let kdf = KdfParams::new();
        let key = derive_key(master, &kdf)?;
        let mut entries = BTreeMap::new();
        for (id, sealed) in &file.entries {
            entries.insert(id.clone(), seal(&key, id, &open(&self.key, id, sealed)?)?);
        }
        let check = seal(&key, CHECK_ID, CHECK_PLAINTEXT)?;
        write_vault(&self.path, &VaultFile { version: 1, kdf: kdf.clone(), check, entries })?;
        Ok(Vault { path: self.path.clone(), kdf, key })
    }

    /// The vault file as it is now, provided this vault's key still opens it.
    fn load(&self) -> Result<VaultFile> {
        let file = read_vault(&self.path)?;
        let same_key = file.kdf == self.kdf
            && matches!(open(&self.key, CHECK_ID, &file.check), Ok(plain) if plain.as_slice() == CHECK_PLAINTEXT);
        if !same_key {
            return Err(VaultChanged.into());
        }
        Ok(file)
    }

    pub fn get(&self, id: &str) -> Result<Option<StoredSecret>> {
        let file = self.load()?;
        let Some(sealed) = file.entries.get(id) else { return Ok(None) };
        let mut plain = open(&self.key, id, sealed)?;
        let text = match String::from_utf8(std::mem::take(&mut *plain)) {
            Ok(text) => Zeroizing::new(text),
            Err(e) => {
                e.into_bytes().zeroize();
                bail!("vault entry is not valid UTF-8");
            }
        };
        StoredSecret::decode(text).map(Some)
    }

    /// Saves `password` for profile `id`, bound to `endpoint`.
    pub fn set(&self, id: &str, endpoint: &Endpoint, password: &str) -> Result<()> {
        let sealed = seal(&self.key, id, encode(endpoint, password).as_bytes())?;
        let _lock = lock(&self.path)?;
        let mut file = self.load()?;
        file.entries.insert(id.to_string(), sealed);
        write_vault(&self.path, &file)
    }

    /// Removes an entry, if there is one. Needs no master password: entries
    /// are sealed independently, so none is needed to drop one.
    pub fn remove(path: &Path, id: &str) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let _lock = lock(path)?;
        let mut file = read_vault(path)?;
        if file.entries.remove(id).is_some() {
            write_vault(path, &file)?;
        }
        Ok(())
    }
}

fn derive_key(master: &str, kdf: &KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    kdf.check()?;
    let salt = B64.decode(&kdf.salt).context("invalid vault salt")?;
    let params = argon2::Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| anyhow!("invalid vault parameters: {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    // The working memory is enough to recompute the key: wipe it as well.
    let mut memory = Zeroizing::new(vec![argon2::Block::default(); argon.params().block_count()]);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into_with_memory(master.as_bytes(), &salt, key.as_mut(), memory.as_mut_slice())
        .map_err(|e| anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Encrypts with the entry id as associated data so sealed values cannot be
/// swapped between profiles.
fn seal(key: &[u8; 32], id: &str, plain: &[u8]) -> Result<Sealed> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let data = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: id.as_bytes() })
        .map_err(|_| anyhow!("encryption failed"))?;
    Ok(Sealed { nonce: B64.encode(nonce), data: B64.encode(data) })
}

fn open(key: &[u8; 32], id: &str, sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = B64.decode(&sealed.nonce)?;
    if nonce.len() != 24 {
        bail!("invalid nonce");
    }
    let data = B64.decode(&sealed.data)?;
    cipher
        .decrypt(XNonce::from_slice(&nonce), Payload { msg: &data, aad: id.as_bytes() })
        .map(Zeroizing::new)
        .map_err(|_| anyhow!("could not decrypt vault entry"))
}

/// An in-memory keyring for tests.
#[cfg(test)]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::{Condvar, Mutex};

    use super::*;

    /// While held, writes and deletes wait, like a locked keyring waiting
    /// for its unlock prompt to be answered.
    #[derive(Default)]
    pub struct FakeKeyring {
        entries: Mutex<HashMap<String, String>>,
        held: Mutex<bool>,
        released: Condvar,
    }

    impl FakeKeyring {
        pub fn hold(&self) {
            *self.held.lock().unwrap() = true;
        }

        pub fn release(&self) {
            *self.held.lock().unwrap() = false;
            self.released.notify_all();
        }

        fn wait(&self) {
            let held = self.held.lock().unwrap();
            drop(self.released.wait_while(held, |held| *held).unwrap());
        }

        /// The raw stored value.
        pub fn raw(&self, id: &str) -> Option<String> {
            self.entries.lock().unwrap().get(id).cloned()
        }

        pub fn put_raw(&self, id: &str, value: &str) {
            self.entries.lock().unwrap().insert(id.into(), value.into());
        }

        pub fn put(&self, id: &str, endpoint: &Endpoint, password: &str) {
            self.put_raw(id, &encode(endpoint, password));
        }

        pub fn secret(&self, id: &str) -> Option<StoredSecret> {
            self.raw(id).map(|raw| StoredSecret::decode(Zeroizing::new(raw)).unwrap())
        }
    }

    impl KeyringBackend for FakeKeyring {
        fn set(&self, id: &str, secret: &str) -> Result<()> {
            self.wait();
            self.put_raw(id, secret);
            Ok(())
        }

        fn get(&self, id: &str) -> Result<Option<Zeroizing<String>>> {
            Ok(self.raw(id).map(Zeroizing::new))
        }

        fn delete(&self, id: &str) -> Result<()> {
            self.wait();
            self.entries.lock().unwrap().remove(id);
            Ok(())
        }
    }

    impl Vault {
        /// An entry as older versions saved it: the password alone.
        pub fn put_legacy(&self, id: &str, password: &str) {
            let sealed = seal(&self.key, id, password.as_bytes()).unwrap();
            let mut file = self.load().unwrap();
            file.entries.insert(id.into(), sealed);
            write_vault(&self.path, &file).unwrap();
        }
    }

    impl StoredSecret {
        pub fn password(&self) -> &str {
            &self.password
        }

        pub fn endpoint(&self) -> Option<&Endpoint> {
            self.endpoint.as_ref()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeKeyring;
    use super::*;

    fn endpoint(host: &str) -> Endpoint {
        Endpoint::new(Driver::Postgres, host, None, "app")
    }

    fn rewrite(path: &Path, change: impl FnOnce(&mut VaultFile)) {
        let mut file = read_vault(path).unwrap();
        change(&mut file);
        write_vault(path, &file).unwrap();
    }

    /// A vault as older versions made it: lower costs, unbound entries.
    fn legacy_vault(path: &Path, master: &str, entries: &[(&str, &str)]) {
        let kdf = KdfParams { m_cost: 19 * 1024, t_cost: 2, ..KdfParams::new() };
        let key = derive_key(master, &kdf).unwrap();
        let entries = entries.iter().map(|(id, pw)| (id.to_string(), seal(&key, id, pw.as_bytes()).unwrap())).collect();
        let file = VaultFile { version: 1, check: seal(&key, CHECK_ID, CHECK_PLAINTEXT).unwrap(), kdf, entries };
        write_vault(path, &file).unwrap();
    }

    #[test]
    fn vault_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let v = Vault::create(&path, "correct horse").unwrap();
        v.set("abc", &endpoint("db"), "s3cret!").unwrap();
        drop(v);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("s3cret"));

        let v = Vault::unlock(&path, "correct horse").unwrap();
        let secret = v.get("abc").unwrap().unwrap();
        assert_eq!((secret.password(), secret.endpoint()), ("s3cret!", Some(&endpoint("db"))));
        assert!(v.get("missing").unwrap().is_none());
        Vault::remove(&path, "abc").unwrap();
        assert!(v.get("abc").unwrap().is_none());
    }

    #[test]
    fn wrong_master_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        Vault::create(&path, "correct horse").unwrap();
        let err = Vault::unlock(&path, "battery staple").unwrap_err();
        assert!(err.to_string().contains("wrong master password"));
    }

    #[test]
    fn short_master_password_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Vault::create(&dir.path().join("v.toml"), "short").is_err());
    }

    #[test]
    fn entries_bound_to_their_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let v = Vault::create(&path, "correct horse").unwrap();
        v.set("a", &endpoint("db"), "one").unwrap();
        rewrite(&path, |f| {
            let sealed = f.entries["a"].clone();
            f.entries.insert("b".into(), sealed);
        });
        assert!(v.get("b").is_err());
    }

    /// Audit L1: a password is only handed out for the server it was saved
    /// for. One saved by an older version counts as saved for the profile's
    /// server as it was saved.
    #[test]
    fn passwords_are_bound_to_their_endpoint() {
        let saved = endpoint("db.example.com");
        assert_eq!(saved, Endpoint::new(Driver::Postgres, " db.example.com ", Some(5432), " app"));
        assert_eq!(Endpoint::new(Driver::MySql, "", None, "").to_string(), "mysql://localhost:3306");
        assert_eq!(saved.to_string(), "postgres://app@db.example.com:5432");

        let secret = StoredSecret::decode(encode(&saved, "pw\nwith newline")).unwrap();
        assert!(secret.is_bound());
        assert_eq!(secret.password_for(&saved, &endpoint("other")).unwrap().as_str(), "pw\nwith newline");
        for other in [
            endpoint("evil.example.com"),
            Endpoint { port: 6432, ..saved.clone() },
            Endpoint { user: "postgres".into(), ..saved.clone() },
            Endpoint { driver: Driver::MySql, ..saved.clone() },
        ] {
            let err = secret.password_for(&other, &saved).unwrap_err();
            assert!(err.contains("postgres://app@db.example.com:5432") && err.contains(&other.to_string()), "{err}");
        }

        let legacy = StoredSecret::decode(Zeroizing::new("old-password".into())).unwrap();
        assert!(!legacy.is_bound());
        assert_eq!(legacy.password_for(&saved, &saved).unwrap().as_str(), "old-password");
        assert!(legacy.password_for(&endpoint("evil.example.com"), &saved).is_err());

        assert!(StoredSecret::decode(Zeroizing::new(format!("{BOUND_PREFIX}{{not json\npw"))).is_err());
    }

    /// Old vaults still open, and are re-encrypted with today's costs; their
    /// entries keep working (and stay unbound until used).
    #[test]
    fn old_vaults_are_upgraded_on_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        legacy_vault(&path, "correct horse", &[("abc", "old-password")]);
        let before = read_vault(&path).unwrap();

        let v = Vault::unlock(&path, "correct horse").unwrap();
        let after = read_vault(&path).unwrap();
        assert_eq!((after.kdf.m_cost, after.kdf.t_cost, after.kdf.p_cost), (64 * 1024, 3, 1));
        assert_ne!(after.kdf.salt, before.kdf.salt);
        let secret = v.get("abc").unwrap().unwrap();
        assert_eq!((secret.password(), secret.is_bound()), ("old-password", false));

        let v = Vault::unlock(&path, "correct horse").unwrap();
        assert_eq!(v.get("abc").unwrap().unwrap().password(), "old-password");
        assert_eq!(read_vault(&path).unwrap().kdf, after.kdf);
        assert!(Vault::unlock(&path, "wrong horse").is_err());

        let new = dir.path().join("new.toml");
        Vault::create(&new, "correct horse").unwrap();
        let kdf = read_vault(&new).unwrap().kdf;
        assert_eq!((kdf.m_cost, kdf.t_cost, kdf.p_cost), (KDF_M_COST, KDF_T_COST, KDF_P_COST));
    }

    /// Key derivation costs come from the file; absurd ones are refused
    /// rather than hanging the app or exhausting memory.
    #[test]
    fn kdf_costs_from_the_file_are_limited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        Vault::create(&path, "correct horse").unwrap();
        for (m, t, p) in [(u32::MAX, 3, 1), (64 * 1024, u32::MAX, 1), (64 * 1024, 3, 255), (8, 1, 1)] {
            rewrite(&path, |f| (f.kdf.m_cost, f.kdf.t_cost, f.kdf.p_cost) = (m, t, p));
            let started = std::time::Instant::now();
            let err = Vault::unlock(&path, "correct horse").unwrap_err();
            assert!(err.to_string().contains("unsupported key derivation costs"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    /// Creating a vault never replaces one, e.g. one another instance has
    /// just created.
    #[test]
    fn create_never_replaces_a_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let first = Vault::create(&path, "correct horse").unwrap();
        first.set("a", &endpoint("db"), "one").unwrap();
        let err = Vault::create(&path, "battery staple").unwrap_err();
        assert!(err.is::<VaultExists>(), "{err}");
        let v = Vault::unlock(&path, "correct horse").unwrap();
        assert_eq!(v.get("a").unwrap().unwrap().password(), "one");
    }

    /// Two instances sharing a vault see and keep each other's changes, and
    /// one that holds a key the file no longer uses says so.
    #[test]
    fn instances_share_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let one = Vault::create(&path, "correct horse").unwrap();
        let two = Vault::unlock(&path, "correct horse").unwrap();
        one.set("a", &endpoint("a"), "one").unwrap();
        two.set("b", &endpoint("b"), "two").unwrap();
        assert_eq!(one.get("b").unwrap().unwrap().password(), "two");
        assert_eq!(two.get("a").unwrap().unwrap().password(), "one");
        Vault::remove(&path, "a").unwrap();
        assert!(one.get("a").unwrap().is_none());

        std::fs::remove_file(&path).unwrap();
        Vault::create(&path, "battery staple").unwrap();
        assert!(one.get("b").unwrap_err().is::<VaultChanged>());
        assert!(one.set("b", &endpoint("b"), "two").unwrap_err().is::<VaultChanged>());
    }

    /// Audit L2: keyring writes run in order, and a slow one reports its
    /// result whenever it finishes rather than being given up on.
    #[test]
    fn keyring_operations_run_in_order() {
        let fake = Arc::new(FakeKeyring::default());
        let keyring = Keyring::new(fake.clone());
        let (tx, rx) = mpsc::channel();
        fake.hold();
        let done = |tx: &mpsc::Sender<&'static str>, what| {
            let tx = tx.clone();
            move |r: Result<()>| tx.send(if r.is_ok() { what } else { "error" }).unwrap()
        };
        keyring.set("a", &endpoint("db"), "first", done(&tx, "set"));
        keyring.delete("a", done(&tx, "delete"));
        keyring.set("a", &endpoint("db"), "second", done(&tx, "set again"));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "still waiting for the keyring");
        fake.release();
        let order: Vec<_> = (0..3).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(order, ["set", "delete", "set again"]);
        let secret = keyring.get("a").unwrap().unwrap();
        assert_eq!((secret.password(), secret.endpoint()), ("second", Some(&endpoint("db"))));
        assert!(fake.raw("a").unwrap().starts_with(BOUND_PREFIX));
        assert!(keyring.get("missing").unwrap().is_none());
    }
}
