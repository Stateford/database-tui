//! Secure password storage.
//!
//! Two backends are supported:
//! * the operating system keyring (macOS Keychain, Windows Credential
//!   Manager, Secret Service on Linux/BSD);
//! * an encrypted vault file protected by a master password, for systems
//!   without a keyring (headless servers, containers, WSL...). Secrets are
//!   sealed with XChaCha20-Poly1305 using a key derived with Argon2id.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::write_private;

const KEYRING_SERVICE: &str = "dbtui";

/// How long to wait for the OS keyring. A locked Secret Service keyring can
/// block forever waiting for an unlock prompt nobody can see (e.g. on WSL).
const KEYRING_TIMEOUT: Duration = Duration::from_secs(15);

/// Runs a keyring operation on a dedicated thread: some platform backends
/// drive their own executor and must not run on the async runtime. Gives up
/// after `KEYRING_TIMEOUT`, leaving the stuck thread behind.
fn on_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(KEYRING_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => bail!("the OS keyring did not respond (is it locked?)"),
        Err(RecvTimeoutError::Disconnected) => bail!("keyring thread panicked"),
    }
}

pub fn keyring_set(id: &str, password: &str) -> Result<()> {
    let (id, password) = (id.to_string(), Zeroizing::new(password.to_string()));
    on_thread(move || -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &id)?;
        entry.set_password(&password)?;
        // Verify: some keyrings silently drop writes when locked.
        let back = Zeroizing::new(entry.get_password()?);
        if *back != *password {
            bail!("keyring did not store the password");
        }
        Ok(())
    })
    .context("OS keyring unavailable")
}

pub fn keyring_get(id: &str) -> Result<Option<String>> {
    let id = id.to_string();
    on_thread(move || match keyring::Entry::new(KEYRING_SERVICE, &id).and_then(|e| e.get_password()) {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow!(e)),
    })
    .context("could not read from the OS keyring")
}

pub fn keyring_delete(id: &str) -> Result<()> {
    let id = id.to_string();
    on_thread(move || match keyring::Entry::new(KEYRING_SERVICE, &id).and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!(e)),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KdfParams {
    salt: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
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

const CHECK_PLAINTEXT: &[u8] = b"dbtui-vault-v1";

/// An unlocked vault. The derived key lives only in memory and is wiped on drop.
pub struct Vault {
    path: PathBuf,
    file: VaultFile,
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Vault {
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::config::config_dir()?.join("vault.toml"))
    }

    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    pub fn create(path: &Path, master: &str) -> Result<Vault> {
        if master.chars().count() < 8 {
            bail!("master password must be at least 8 characters");
        }
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let kdf = KdfParams { salt: B64.encode(salt), m_cost: 19 * 1024, t_cost: 2, p_cost: 1 };
        let key = derive_key(master, &kdf)?;
        let check = seal(&key, "__check__", CHECK_PLAINTEXT)?;
        let vault = Vault {
            path: path.to_path_buf(),
            file: VaultFile { version: 1, kdf, check, entries: BTreeMap::new() },
            key,
        };
        vault.save()?;
        Ok(vault)
    }

    pub fn unlock(path: &Path, master: &str) -> Result<Vault> {
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        let file: VaultFile = toml::from_str(&text).context("vault file is corrupt")?;
        if file.version != 1 {
            bail!("unsupported vault version {}", file.version);
        }
        let key = derive_key(master, &file.kdf)?;
        match open(&key, "__check__", &file.check) {
            Ok(plain) if plain.as_slice() == CHECK_PLAINTEXT => {}
            _ => bail!("wrong master password"),
        }
        Ok(Vault { path: path.to_path_buf(), file, key })
    }

    pub fn get(&self, id: &str) -> Result<Option<String>> {
        let Some(sealed) = self.file.entries.get(id) else { return Ok(None) };
        let plain = open(&self.key, id, sealed)?;
        Ok(Some(String::from_utf8(plain.to_vec()).context("vault entry is not valid UTF-8")?))
    }

    pub fn set(&mut self, id: &str, secret: &str) -> Result<()> {
        let sealed = seal(&self.key, id, secret.as_bytes())?;
        self.file.entries.insert(id.to_string(), sealed);
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        if self.file.entries.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Removes an entry without the master password (entries are sealed
    /// independently, so no key is needed to drop one).
    pub fn remove_locked(path: &Path, id: &str) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)?;
        let mut file: VaultFile = toml::from_str(&text).context("vault file is corrupt")?;
        if file.entries.remove(id).is_some() {
            write_private(path, toml::to_string_pretty(&file)?.as_bytes())?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        write_private(&self.path, toml::to_string_pretty(&self.file)?.as_bytes())
    }
}

fn derive_key(master: &str, kdf: &KdfParams) -> Result<Zeroizing<[u8; 32]>> {
    let salt = B64.decode(&kdf.salt).context("invalid vault salt")?;
    let params = argon2::Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| anyhow!("invalid vault parameters: {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(master.as_bytes(), &salt, key.as_mut())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.toml");
        let mut v = Vault::create(&path, "correct horse").unwrap();
        v.set("abc", "s3cret!").unwrap();
        drop(v);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("s3cret"));

        let mut v = Vault::unlock(&path, "correct horse").unwrap();
        assert_eq!(v.get("abc").unwrap().as_deref(), Some("s3cret!"));
        assert_eq!(v.get("missing").unwrap(), None);
        v.remove("abc").unwrap();
        assert_eq!(v.get("abc").unwrap(), None);
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
        let mut v = Vault::create(&path, "correct horse").unwrap();
        v.set("a", "one").unwrap();
        let sealed = v.file.entries["a"].clone();
        v.file.entries.insert("b".into(), sealed);
        assert!(v.get("b").is_err());
    }
}
