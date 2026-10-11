//! The secret store this crate reads and writes, shaped like `ServerSecretStore` in
//! `t3-server` (`server_secret_store.rs`): named byte values, `create` fails when the name exists.
//! The server implements [`SecretStore`] by delegating to its own store.

use std::collections::BTreeMap;
use std::sync::Mutex;

pub const CLOUD_MINT_PUBLIC_KEY: &str = "cloud-mint-ed25519-public-key";
pub const CLOUD_ENDPOINT_RUNTIME_CONFIG: &str = "cloud-endpoint-runtime-config";
pub const CLOUD_ENDPOINT_CONFIRMED_ORIGIN: &str = "cloud-endpoint-confirmed-origin";
pub const CLOUD_LINKED_USER_ID: &str = "cloud-linked-user-id";
pub const RELAY_URL_SECRET: &str = "cloud-relay-url";
pub const RELAY_ISSUER_SECRET: &str = "cloud-relay-issuer";
pub const RELAY_ENVIRONMENT_CREDENTIAL_SECRET: &str = "cloud-relay-environment-credential";
pub const PUBLISH_AGENT_ACTIVITY_SECRET: &str = "cloud-publish-agent-activity";
pub const HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET: &str = "cloud-hold-webhooks-while-offline";
/// What `t3 connect link` wants: `managed`, `publish_only`, or the legacy literal `true`.
pub const CLOUD_CLI_DESIRED_LINK_SECRET: &str = "cloud-cli-desired-link";
/// The CLI's OAuth token, as JSON ([`crate::cli_token::PersistedToken`]).
pub const CLOUD_CLI_OAUTH_TOKEN_SECRET: &str = "cloud-cli-oauth-token";
/// The environment's Ed25519 key pair as one JSON secret.
pub const CLOUD_LINK_KEY_PAIR: &str = "cloud-link-ed25519-key-pair";
/// Two secrets an earlier version wrote separately; migrated into [`CLOUD_LINK_KEY_PAIR`].
pub const CLOUD_LINK_PRIVATE_KEY: &str = "cloud-link-ed25519-private-key";
pub const CLOUD_LINK_PUBLIC_KEY: &str = "cloud-link-ed25519-public-key";

pub const CLOUD_MINT_NONCE_PREFIX: &str = "cloud-mint-nonce-";
pub const CLOUD_MINT_JTI_PREFIX: &str = "cloud-mint-jti-";
pub const CLOUD_HEALTH_NONCE_PREFIX: &str = "cloud-health-nonce-";
pub const CLOUD_HEALTH_JTI_PREFIX: &str = "cloud-health-jti-";
/// Secret name prefixes of the one-shot replay markers; the server prunes expired ones.
pub const CLOUD_REPLAY_MARKER_PREFIXES: [&str; 4] = [
    CLOUD_MINT_NONCE_PREFIX,
    CLOUD_MINT_JTI_PREFIX,
    CLOUD_HEALTH_NONCE_PREFIX,
    CLOUD_HEALTH_JTI_PREFIX,
];

#[derive(Debug, Clone, thiserror::Error)]
#[error("secret store: {message}")]
pub struct SecretError {
    pub message: String,
    /// `create` found the name already taken. The only failure callers tell apart.
    pub already_exists: bool,
}

impl SecretError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            already_exists: false,
        }
    }

    pub fn already_exists(name: &str) -> Self {
        Self {
            message: format!("{name} already exists"),
            already_exists: true,
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError>;
    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError>;
    /// Fails with [`SecretError::already_exists`] when the name is taken.
    fn create(&self, name: &str, value: &[u8]) -> Result<(), SecretError>;
    fn remove(&self, name: &str) -> Result<(), SecretError>;
}

/// A secret as text; `None` when absent. Invalid UTF-8 is replaced, like `TextDecoder`.
pub fn get_string(store: &dyn SecretStore, name: &str) -> Result<Option<String>, SecretError> {
    Ok(store
        .get(name)?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// A non-empty secret as text, or `None` when missing, empty or unreadable
/// (`readSecretString` in `cloud/config.ts`).
pub fn get_nonempty_string(store: &dyn SecretStore, name: &str) -> Option<String> {
    match store.get(name) {
        Ok(Some(bytes)) if !bytes.is_empty() => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    }
}

/// The relay URL and environment credential, or `None` when not linked to T3 Connect.
pub fn read_relay_connection(store: &dyn SecretStore) -> Option<RelayConnection> {
    let url = get_nonempty_string(store, RELAY_URL_SECRET)?;
    let environment_credential = get_nonempty_string(store, RELAY_ENVIRONMENT_CREDENTIAL_SECRET)?;
    Some(RelayConnection {
        url,
        environment_credential,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConnection {
    pub url: String,
    pub environment_credential: String,
}

/// Whether this environment opted in to T3 Connect holding webhooks while it is offline.
pub fn read_hold_webhooks_while_offline(store: &dyn SecretStore) -> bool {
    get_nonempty_string(store, HOLD_WEBHOOKS_WHILE_OFFLINE_SECRET).as_deref() == Some("true")
}

pub fn is_agent_activity_publishing_enabled_value(value: Option<&str>) -> bool {
    value == Some("true")
}

/// Whether agent-activity publishes currently leave this environment: the opt-in is on and the
/// link credentials exist. The descriptor capability uses it so it never advertises publishing the
/// publisher would skip.
pub fn read_agent_activity_publishing_active(store: &dyn SecretStore) -> bool {
    let read = |name| get_string(store, name).ok().flatten();
    let enabled = read(PUBLISH_AGENT_ACTIVITY_SECRET);
    let url = read(RELAY_URL_SECRET);
    let credential = read(RELAY_ENVIRONMENT_CREDENTIAL_SECRET);
    is_agent_activity_publishing_enabled_value(enabled.as_deref())
        && url.as_deref().is_some_and(|value| !value.is_empty())
        && credential.as_deref().is_some_and(|value| !value.is_empty())
}

/// An in-memory store for tests and tools. `fail_*` switches reproduce store outages.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<BTreeMap<String, Vec<u8>>>,
    failures: Mutex<Failures>,
}

#[derive(Default)]
struct Failures {
    get: Vec<String>,
    set: Vec<String>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(entries: &[(&str, &str)]) -> Self {
        let store = Self::new();
        for (name, value) in entries {
            store.set(name, value.as_bytes()).expect("memory store");
        }
        store
    }

    /// Makes every later `get` of `name` fail.
    pub fn fail_get(&self, name: &str) {
        self.failures
            .lock()
            .expect("lock")
            .get
            .push(name.to_owned());
    }

    /// Makes every later `set` of `name` fail.
    pub fn fail_set(&self, name: &str) {
        self.failures
            .lock()
            .expect("lock")
            .set
            .push(name.to_owned());
    }

    pub fn names(&self) -> Vec<String> {
        self.values.lock().expect("lock").keys().cloned().collect()
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.values
            .lock()
            .expect("lock")
            .get(name)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
        if self
            .failures
            .lock()
            .expect("lock")
            .get
            .iter()
            .any(|n| n == name)
        {
            return Err(SecretError::new(format!("cannot read {name}")));
        }
        Ok(self.values.lock().expect("lock").get(name).cloned())
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        if self
            .failures
            .lock()
            .expect("lock")
            .set
            .iter()
            .any(|n| n == name)
        {
            return Err(SecretError::new(format!("cannot write {name}")));
        }
        self.values
            .lock()
            .expect("lock")
            .insert(name.to_owned(), value.to_vec());
        Ok(())
    }

    fn create(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        let mut values = self.values.lock().expect("lock");
        if values.contains_key(name) {
            return Err(SecretError::already_exists(name));
        }
        values.insert(name.to_owned(), value.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), SecretError> {
        self.values.lock().expect("lock").remove(name);
        Ok(())
    }
}
