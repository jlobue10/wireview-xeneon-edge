//! The shared secret and the HMAC that authenticates bridge replies.
//!
//! Both programs run as the same user and share a random secret stored in a
//! per-user file. The client sends a fresh nonce; the bridge answers with
//! `X-WireView-Auth`, an HMAC-SHA256 over `nonce "." body`. The wire format is
//! unchanged from the Python 1.0.1 releases, so either side may be the old or
//! the new implementation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const AUTH_HEADER: &str = "X-WireView-Auth";
pub const SECRET_ENV: &str = "WIREVIEW_BRIDGE_SECRET";
const MIN_SECRET_LEN: usize = 32;
const NONCE_BYTES: usize = 16;

/// Per-user file holding the bridge secret (override: `WIREVIEW_BRIDGE_SECRET`).
pub fn bridge_secret_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SECRET_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = || var("HOME").or_else(|| var("USERPROFILE")).unwrap_or_else(|| PathBuf::from("."));
    let base = if cfg!(windows) {
        var("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local"))
    } else {
        var("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
    };
    base.join("wireview").join("bridge.secret")
}

/// Reads the secret file, remembering it until the file's mtime changes.
#[derive(Debug)]
pub struct SecretStore {
    path: PathBuf,
    cache: Mutex<Option<(SystemTime, Vec<u8>)>>,
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::at(bridge_secret_path())
    }
}

impl SecretStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        SecretStore {
            path: path.into(),
            cache: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The shared secret, or `None` when the file is missing or unreadable.
    ///
    /// `create` (the bridge) writes a fresh secret if none exists, with
    /// owner-only permissions where the platform honours them. The client
    /// never creates it, so a missing file simply means "trust no bridge".
    pub fn get(&self, create: bool) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        match fs::metadata(&self.path) {
            Ok(meta) => {
                let mtime = meta.modified().ok()?;
                if let Some((at, secret)) = cache.as_ref() {
                    if *at == mtime {
                        return Some(secret.clone());
                    }
                }
                let data = fs::read(&self.path).ok()?;
                let data = data.trim_ascii().to_vec();
                if data.len() >= MIN_SECRET_LEN {
                    *cache = Some((mtime, data.clone()));
                    return Some(data);
                }
                if !create {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return None;
                }
            }
            Err(_) => return None,
        }
        let data = self.write_new().ok()?;
        let mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok()?;
        *cache = Some((mtime, data.clone()));
        Some(data)
    }

    fn write_new(&self) -> std::io::Result<Vec<u8>> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).map_err(std::io::Error::other)?;
        let data = hex(&raw).into_bytes();
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
        f.write_all(&data)?;
        Ok(data)
    }
}

fn mac(secret: &[u8], nonce: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts a key of any length");
    m.update(nonce.as_bytes());
    m.update(b".");
    m.update(body);
    m
}

/// The `X-WireView-Auth` value for `body` answered to `nonce`.
pub fn bridge_sign(secret: &[u8], nonce: &str, body: &[u8]) -> String {
    hex(&mac(secret, nonce, body).finalize().into_bytes())
}

/// Constant-time check of a received tag.
pub fn bridge_verify(secret: &[u8], nonce: &str, body: &[u8], tag: &str) -> bool {
    match unhex(tag.trim()) {
        Some(raw) => mac(secret, nonce, body).verify_slice(&raw).is_ok(),
        None => false,
    }
}

/// 16 to 64 lowercase hex digits.
pub fn valid_nonce(nonce: &str) -> bool {
    (16..=64).contains(&nonce.len()) && nonce.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn new_nonce() -> std::io::Result<String> {
    let mut raw = [0u8; NONCE_BYTES];
    getrandom::fill(&mut raw).map_err(std::io::Error::other)?;
    Ok(hex(&raw))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    b.chunks(2).map(|p| Some(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_the_python_implementation() {
        // hmac.new(b"k" * 32, b"ab" * 16 + b"." + b'{"ok":true}', hashlib.sha256).hexdigest()
        let tag = bridge_sign(&[b'k'; 32], &"ab".repeat(16), br#"{"ok":true}"#);
        assert_eq!(tag, "1568a70d265af5b8f1c0b55c795f2ae778bac70091d750802b231fb6fff360de");
    }

    #[test]
    fn verify_accepts_only_the_right_tag() {
        let secret = [7u8; 40];
        let tag = bridge_sign(&secret, "00112233445566778899aabbccddeeff", b"body");
        assert!(bridge_verify(&secret, "00112233445566778899aabbccddeeff", b"body", &tag));
        assert!(!bridge_verify(&secret, "00112233445566778899aabbccddeeff", b"bodx", &tag));
        assert!(!bridge_verify(&secret, "00112233445566778899aabbccddeef0", b"body", &tag));
        assert!(!bridge_verify(&secret, "00112233445566778899aabbccddeeff", b"body", ""));
        assert!(!bridge_verify(&secret, "00112233445566778899aabbccddeeff", b"body", "zz"));
    }

    #[test]
    fn nonce_rules() {
        assert!(valid_nonce(&"ab".repeat(8)));
        assert!(valid_nonce(&"0f".repeat(32)));
        assert!(!valid_nonce("ZZ"));
        assert!(!valid_nonce(&"ab".repeat(7)));
        assert!(!valid_nonce(&"AB".repeat(8)));
        assert!(!valid_nonce(&"ab".repeat(33)));
        assert!(valid_nonce(&new_nonce().unwrap()));
    }

    #[test]
    fn client_never_creates_the_secret_and_the_bridge_does() {
        let dir = std::env::temp_dir().join(format!("wireview-auth-{}", new_nonce().unwrap()));
        let store = SecretStore::at(dir.join("nested").join("bridge.secret"));
        assert_eq!(store.get(false), None);
        assert!(!store.path().exists());
        let secret = store.get(true).expect("created");
        assert_eq!(secret.len(), 64);
        assert_eq!(store.get(false), Some(secret.clone()));
        assert_eq!(SecretStore::at(store.path()).get(false), Some(secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // A too-short file is not a secret; only the bridge replaces it.
        fs::write(store.path(), b"short").unwrap();
        let fresh = SecretStore::at(store.path());
        assert_eq!(fresh.get(false), None);
        assert_eq!(fresh.get(true).map(|s| s.len()), Some(64));
        fs::remove_dir_all(dir).unwrap();
    }
}
