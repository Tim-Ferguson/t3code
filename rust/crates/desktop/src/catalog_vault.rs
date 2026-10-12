//! The file that holds the saved connections on the desktop app: the catalog document (which carries the bearer tokens of the
//! environments the person paired) encrypted with a key the operating system keeps. The original does the same with Electron's
//! `safeStorage` (`apps/desktop/src/app/DesktopConnectionCatalogStore.ts`): `connection-catalog.json` holds
//! `{"version":1,"encryptedCatalog":<base64>}`, and a system that cannot encrypt saves nothing rather than saving plain text.
//!
//! Here the encryption is AES-256-GCM under a 32-byte key. Where the key lives is a [`KeyStore`]: the macOS keychain
//! ([`Keychain`]) in the app as a person runs it, or a file beside the catalog ([`FileKey`]) for a profile that was given its own
//! data directory (`T3_UI_DATA_DIR`: a test, a second profile), so that such a profile never touches the person's keychain. The
//! file key protects nothing from another process of the same user; it exists so an isolated profile is isolated. The two formats
//! are not the original's: a catalog written by Electron cannot be read here (its key is Chromium's), nor the reverse.
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const KEY_LEN: usize = 32;
const CONTEXT: &[u8] = b"t3code-rust connection catalog v1";

/// Where the encryption key is kept. Reading it creates it the first time.
pub trait KeyStore: Send + Sync {
    fn key(&self) -> Result<[u8; KEY_LEN], String>;
}

fn random_key() -> Result<[u8; KEY_LEN], String> {
    let mut key = [0u8; KEY_LEN];
    SystemRandom::new().fill(&mut key).map_err(|_| "The system could not produce a random key.".to_owned())?;
    Ok(key)
}

/// A key in a file readable by this user only. See the module note on what that does and does not protect.
pub struct FileKey(pub PathBuf);

impl KeyStore for FileKey {
    fn key(&self) -> Result<[u8; KEY_LEN], String> {
        match std::fs::read(&self.0) {
            Ok(bytes) => <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| format!("{} does not hold a key.", self.0.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = random_key()?;
                write_private(&self.0, &key).map_err(|error| format!("Cannot save the key to {}: {error}", self.0.display()))?;
                Ok(key)
            }
            Err(error) => Err(format!("Cannot read the key at {}: {error}", self.0.display())),
        }
    }
}

/// The key in the macOS keychain, as a generic password of this app's.
#[cfg(target_os = "macos")]
pub struct Keychain {
    pub service: String,
    pub account: String,
}

#[cfg(target_os = "macos")]
impl KeyStore for Keychain {
    fn key(&self) -> Result<[u8; KEY_LEN], String> {
        use security_framework::passwords::{get_generic_password, set_generic_password};
        /// `errSecItemNotFound`.
        const NOT_FOUND: i32 = -25300;
        match get_generic_password(&self.service, &self.account) {
            Ok(bytes) => <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| "The keychain item for the saved connections is not a key.".to_owned()),
            Err(error) if error.code() == NOT_FOUND => {
                let key = random_key()?;
                set_generic_password(&self.service, &self.account, &key).map_err(|error| format!("The keychain would not keep the key for the saved connections: {error}"))?;
                Ok(key)
            }
            Err(error) => Err(format!("The keychain could not be read: {error}")),
        }
    }
}

/// The key store for this run: the profile's own file when it was given its own data directory, else the system's.
pub fn key_store(root: &Path, own_data_directory: bool) -> Option<Arc<dyn KeyStore>> {
    if own_data_directory {
        return Some(Arc::new(FileKey(root.join("connection-catalog.key"))));
    }
    #[cfg(target_os = "macos")]
    return Some(Arc::new(Keychain { service: "T3 Code (Rust) saved connections".into(), account: "catalog-key".into() }));
    // No other platform's secret service is wired up: the app saves nothing there rather than saving it in the open.
    #[cfg(not(target_os = "macos"))]
    None
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// Why the file could not be read: the system would not give the key (nothing is wrong with the file), or the file is not something
/// this key opens (it is damaged, changed, or from another key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Unavailable(String),
    Unreadable(String),
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::Unavailable(message) | Self::Unreadable(message)) = self;
        formatter.write_str(message)
    }
}

/// The encrypted catalog file.
pub struct Vault {
    path: PathBuf,
    keys: Arc<dyn KeyStore>,
}

impl Vault {
    pub fn new(path: PathBuf, keys: Arc<dyn KeyStore>) -> Self {
        Self { path, keys }
    }

    /// The document text, none when nothing was saved yet.
    pub fn read(&self) -> Result<Option<String>, ReadError> {
        let damaged = || ReadError::Unreadable("The saved connections file is damaged.".to_owned());
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReadError::Unavailable(format!("Cannot read the saved connections at {}: {error}", self.path.display()))),
        };
        let envelope: serde_json::Value = serde_json::from_str(&raw).map_err(|_| damaged())?;
        if envelope["version"] != 1 {
            return Err(ReadError::Unreadable("The saved connections file is from a version this app does not know.".into()));
        }
        let sealed = STANDARD.decode(envelope["encryptedCatalog"].as_str().unwrap_or_default()).map_err(|_| damaged())?;
        if sealed.len() < NONCE_LEN {
            return Err(damaged());
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let mut buffer = ciphertext.to_vec();
        let cipher = self.cipher().map_err(ReadError::Unavailable)?;
        let plain = cipher
            .open_in_place(Nonce::try_assume_unique_for_key(nonce).expect("a nonce of the right length"), Aad::from(CONTEXT), &mut buffer)
            .map_err(|_| ReadError::Unreadable("The saved connections could not be decrypted. The key they were saved with is gone or the file was changed.".to_owned()))?;
        String::from_utf8(plain.to_vec()).map(Some).map_err(|_| damaged())
    }

    /// Replaces the file with `document`, encrypted. Nothing is written when the key cannot be had.
    pub fn write(&self, document: &str) -> Result<(), String> {
        let cipher = self.cipher()?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| "The system could not produce a random nonce.".to_owned())?;
        let mut buffer = document.as_bytes().to_vec();
        cipher.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(CONTEXT), &mut buffer).map_err(|_| "The saved connections could not be encrypted.".to_owned())?;
        let mut sealed = nonce.to_vec();
        sealed.extend_from_slice(&buffer);
        let envelope = serde_json::json!({"version":1,"encryptedCatalog":STANDARD.encode(sealed)});
        write_private(&self.path, format!("{envelope}\n").as_bytes()).map_err(|error| format!("Cannot save the connections to {}: {error}", self.path.display()))
    }

    /// Sets a file that cannot be read aside, keeping it, so that a fresh catalog does not overwrite what might still be recovered.
    pub fn quarantine(&self, stamp: u128) -> Result<(), String> {
        let aside = self.path.with_file_name(format!("{}.corrupt-{stamp}", self.path.file_name().and_then(|name| name.to_str()).unwrap_or("connection-catalog.json")));
        std::fs::rename(&self.path, aside).map_err(|error| format!("Cannot set the unreadable saved connections aside: {error}"))
    }

    fn cipher(&self) -> Result<LessSafeKey, String> {
        let key = self.keys.key()?;
        Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "The key for the saved connections is not usable.".to_owned())?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(directory: &Path) -> Vault {
        Vault::new(directory.join("connection-catalog.json"), Arc::new(FileKey(directory.join("connection-catalog.key"))))
    }

    #[test]
    fn a_saved_document_reads_back_and_the_file_holds_no_plain_text() {
        let directory = tempfile::tempdir().unwrap();
        let vault = vault(directory.path());
        assert_eq!(vault.read().unwrap(), None);
        let document = r#"{"schemaVersion":1,"credentials":[{"credential":{"token":"very-secret-token"}}]}"#;
        vault.write(document).unwrap();
        assert_eq!(vault.read().unwrap().as_deref(), Some(document));
        let on_disk = std::fs::read_to_string(directory.path().join("connection-catalog.json")).unwrap();
        assert!(!on_disk.contains("very-secret-token") && !on_disk.contains("schemaVersion"), "{on_disk}");
        let envelope: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(envelope["version"], 1);
        // Every write is sealed afresh.
        vault.write(document).unwrap();
        assert_ne!(on_disk, std::fs::read_to_string(directory.path().join("connection-catalog.json")).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["connection-catalog.json", "connection-catalog.key"] {
                assert_eq!(std::fs::metadata(directory.path().join(name)).unwrap().permissions().mode() & 0o777, 0o600, "{name}");
            }
        }
    }

    #[test]
    fn a_changed_file_or_another_key_is_refused_and_can_be_set_aside() {
        let directory = tempfile::tempdir().unwrap();
        let vault = vault(directory.path());
        vault.write("{}").unwrap();
        let path = directory.path().join("connection-catalog.json");
        let mut envelope: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut sealed = STANDARD.decode(envelope["encryptedCatalog"].as_str().unwrap()).unwrap();
        *sealed.last_mut().unwrap() ^= 1;
        envelope["encryptedCatalog"] = STANDARD.encode(sealed).into();
        std::fs::write(&path, envelope.to_string()).unwrap();
        assert!(vault.read().unwrap_err().to_string().contains("could not be decrypted"));

        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(vault.read().unwrap_err(), ReadError::Unreadable(message) if message.contains("damaged")));
        vault.quarantine(7).unwrap();
        assert!(!path.exists() && directory.path().join("connection-catalog.json.corrupt-7").exists());
        assert_eq!(vault.read().unwrap(), None);

        vault.write("{}").unwrap();
        std::fs::remove_file(directory.path().join("connection-catalog.key")).unwrap();
        assert!(matches!(vault.read().unwrap_err(), ReadError::Unreadable(_)), "a new key cannot open what the old one sealed");
    }

    #[test]
    fn a_key_store_that_cannot_keep_a_key_saves_nothing() {
        struct Broken;
        impl KeyStore for Broken {
            fn key(&self) -> Result<[u8; KEY_LEN], String> {
                Err("no secret service".into())
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let vault = Vault::new(directory.path().join("connection-catalog.json"), Arc::new(Broken));
        assert_eq!(vault.write("{}").unwrap_err(), "no secret service");
        assert!(!directory.path().join("connection-catalog.json").exists());
        // Reading, too, says the system is at fault and not the file, so the file is not set aside.
        std::fs::write(directory.path().join("connection-catalog.json"), r#"{"version":1,"encryptedCatalog":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#).unwrap();
        assert_eq!(vault.read().unwrap_err(), ReadError::Unavailable("no secret service".into()));
    }

    #[test]
    fn an_isolated_profile_never_gets_the_keychain() {
        let directory = tempfile::tempdir().unwrap();
        let keys = key_store(directory.path(), true).unwrap();
        let first = keys.key().unwrap();
        assert_eq!(first, keys.key().unwrap());
        assert!(directory.path().join("connection-catalog.key").exists());
    }
}
