//! Attachment storage handle shared by uploads, claims, and asset URLs. The files live
//! beside the native database (`<state>/attachments`) and URLs are signed with the
//! environment's `asset-access-signing-key` secret, exactly where the original server
//! keeps them, so one state directory serves either implementation.
use crate::{persistence::Store, server_secret_store::ServerSecretStore};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

/// Upload and download tokens share one key; their signed `kind` tells them apart.
const SIGNING_SECRET_NAME: &str = "asset-access-signing-key";
const PENDING_ATTACHMENT_SWEEP_INTERVAL_MS: i64 = 15 * 60_000;
static LAST_PENDING_SWEEP: LazyLock<Mutex<HashMap<PathBuf, i64>>> = LazyLock::new(Default::default);

#[derive(Clone, Debug)]
pub struct AttachmentService {
    attachments_dir: Arc<PathBuf>,
    secrets_dir: Arc<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to load the attachment upload signing key.")]
pub struct SigningKeyError(pub String);

impl AttachmentService {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            attachments_dir: Arc::new(state_dir.join("attachments")),
            secrets_dir: Arc::new(state_dir.join("secrets")),
        }
    }
    /// None for the in-memory store, which has no state directory to keep files in.
    pub fn for_store(store: &Store) -> Option<Self> {
        store.state_dir().map(|directory| Self::new(directory))
    }
    pub fn attachments_dir(&self) -> &Path {
        &self.attachments_dir
    }
    fn secret(&self) -> Result<Vec<u8>, SigningKeyError> {
        ServerSecretStore::open(self.secrets_dir.as_ref())
            .and_then(|store| store.get_or_create_random(SIGNING_SECRET_NAME, 32))
            .map_err(|error| SigningKeyError(error.to_string()))
    }
    pub(crate) fn sign_payload(secret: &[u8], payload: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts every key length");
        mac.update(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }
    /// `<base64url(claims json)>.<hmac>`; claims serialize in the key order the caller built.
    pub(crate) fn sign_claims(&self, claims: &Value) -> Result<String, SigningKeyError> {
        let secret = self.secret()?;
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        Ok(format!(
            "{payload}.{}",
            Self::sign_payload(&secret, &payload)
        ))
    }
    /// Checks the signature and decodes the payload. Expiry is the caller's decision.
    pub(crate) fn verify_claims(&self, encoded_payload: &str, signature: &str) -> Option<Value> {
        let secret = match self.secret() {
            Ok(secret) => secret,
            Err(error) => {
                tracing::error!(error=%error,"Failed to load the attachment signing key.");
                return None;
            }
        };
        let presented = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).ok()?;
        mac.update(encoded_payload.as_bytes());
        mac.verify_slice(&presented).ok()?;
        let decoded = URL_SAFE_NO_PAD.decode(encoded_payload).ok()?;
        serde_json::from_slice(&decoded).ok()
    }
    /// Sweeps stale staged uploads at most once per interval per directory.
    pub(crate) fn sweep_if_due(&self, now_ms: i64) -> usize {
        {
            let mut last = LAST_PENDING_SWEEP.lock().unwrap();
            let due = last
                .get(self.attachments_dir.as_ref())
                .is_none_or(|previous| now_ms - previous >= PENDING_ATTACHMENT_SWEEP_INTERVAL_MS);
            if !due {
                return 0;
            }
            last.insert(self.attachments_dir.as_ref().clone(), now_ms);
        }
        crate::attachment_store::sweep_stale_pending_attachments(
            &self.attachments_dir,
            now_ms as f64,
        )
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn service() -> (tempfile::TempDir, AttachmentService) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("attachments")).unwrap();
        let service = AttachmentService::new(directory.path());
        (directory, service)
    }
    pub fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }
    /// Rows recorded from the original server (generate_attachment_access_fixtures.mjs).
    pub fn access_fixture() -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/attachment-access.json"
            ))
            .unwrap(),
        )
        .unwrap()
    }
    /// A state directory holding the fixture's signing secret and staged attachment files.
    pub fn fixture_service(fixture: &Value) -> (tempfile::TempDir, AttachmentService) {
        let (directory, service) = service();
        ServerSecretStore::open(directory.path().join("secrets"))
            .unwrap()
            .set(
                SIGNING_SECRET_NAME,
                &unhex(fixture["secret"].as_str().unwrap()),
            )
            .unwrap();
        for staged in fixture["stage"].as_array().unwrap() {
            std::fs::write(
                service
                    .attachments_dir()
                    .join(staged["file"].as_str().unwrap()),
                unhex(staged["hex"].as_str().unwrap()),
            )
            .unwrap();
        }
        (directory, service)
    }
}
