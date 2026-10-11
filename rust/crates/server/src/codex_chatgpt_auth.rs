//! Signing in to managed Codex with ChatGPT (`provider/CodexChatGptAuth.ts`, `CodexChatGptHandoff.ts`, `CodexChatGptSessionLock.ts`,
//! `CodexAuthCallbackPage.ts`).
//!
//! The environment registers itself with OpenAI as a dynamic client, signs the user in with the authorization-code flow and PKCE on a
//! loopback port, verifies the identity token against the issuer's keys, and keeps the tokens in the server's secret store under the keys
//! the original uses, so a state directory is shared between the two. Tokens are refreshed when asked for (`access`), a connection can be
//! exported as a profile and imported on another environment, and a profile's registration outlives its tokens.
//!
//! Not ported: the analytics events the original records around a sign-in and a transfer.
use crate::{
    codex_auth_handoff::{codex_callback_url, provider_auth_return_url},
    provider_auth_flow::{AuthBackend, AuthContext, AuthResult, Complete},
    server_secret_store::ServerSecretStore,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::future::BoxFuture;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::*;
use tokio::sync::{mpsc, oneshot};

const RESOURCE: &str = "https://api.openai.com/v1";
pub const REQUIRED_SCOPE: &str = "chatgpt.tokens.use.direct";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

// ---- where the tokens live ---------------------------------------------------------------------------------------------------------

/// Bytes by name: the server's secret store, or memory for a handoff, which keeps nothing on disk.
pub trait Secrets: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn set(&self, key: &str, value: &[u8]) -> Result<(), String>;
    fn remove(&self, key: &str) -> Result<(), String>;
    /// Where the files are, if they are files (the sign-in lock lives beside them).
    fn directory(&self) -> Option<&Path>;
}
impl Secrets for ServerSecretStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        ServerSecretStore::get(self, key).map_err(|error| error.to_string())
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        ServerSecretStore::set(self, key, value).map_err(|error| error.to_string())
    }
    fn remove(&self, key: &str) -> Result<(), String> {
        ServerSecretStore::remove(self, key).map_err(|error| error.to_string())
    }
    fn directory(&self) -> Option<&Path> {
        Some(ServerSecretStore::directory(self))
    }
}
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);
impl Secrets for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.to_owned(), value.to_vec());
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
    fn directory(&self) -> Option<&Path> {
        None
    }
}

/// `ProviderCredentialStore.make`: the binding is hashed so an arbitrary instance id cannot escape or overrun a file name.
fn binding_key(driver: &str, binding: &str) -> String {
    let digest = Sha256::digest(format!("{}:{driver}{binding}", driver.len()).as_bytes());
    format!("provider-auth-{}", digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

/// A connection's tokens (`CodexChatGptCredentials`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: f64,
    pub earliest_refresh_at: Option<f64>,
    pub scopes: Vec<String>,
    pub subject: String,
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sessions {
    active_client_id: Option<String>,
    sessions: Vec<Credentials>,
}
/// What outlives the tokens: the client OpenAI registered for this environment and who signed in with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub email: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registrations {
    profiles: Vec<Registration>,
    last_client_id: Option<String>,
}

fn failure(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError { tag: ProviderSetupErrorTag::ProviderSetupError, instance_id: instance.parse().expect("a validated instance"), operation: operation.into(), detail: detail.into(), cause: None }
}

// ---- the lock `proper-lockfile` takes ------------------------------------------------------------------------------------------------

/// `withChatGptSessionLock`: rotating tokens and their active profile change atomically across servers sharing a secret directory. The
/// lock is the directory `<key>.bin.lock`, refreshed every ten seconds and stale after two minutes, as the original's library keeps it.
pub struct SessionLock(Option<PathBuf>, Option<tokio::task::JoinHandle<()>>);
impl SessionLock {
    pub async fn acquire(directory: Option<&Path>, key: &str, instance: &str) -> AuthResult<Self> {
        let Some(directory) = directory else { return Ok(Self(None, None)) };
        let lock = directory.join(format!("{key}.bin.lock"));
        let fail = || failure(instance, "credential-lock", "Could not lock the ChatGPT connection for an update. Try again.");
        for attempt in 0..=80 {
            match std::fs::create_dir(&lock) {
                Ok(()) => {
                    let path = lock.clone();
                    let keep = tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            let _ = std::fs::File::open(&path).and_then(|file| file.set_modified(std::time::SystemTime::now()));
                        }
                    });
                    return Ok(Self(Some(lock), Some(keep)));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock).and_then(|metadata| metadata.modified()).ok().and_then(|modified| modified.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(120));
                    if stale {
                        let _ = std::fs::remove_dir(&lock);
                        continue;
                    }
                    if attempt == 80 {
                        return Err(fail());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(_) => return Err(fail()),
            }
        }
        Err(fail())
    }
}
impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(keep) = self.1.take() {
            keep.abort();
        }
        if let Some(lock) = self.0.take() {
            let _ = std::fs::remove_dir(lock);
        }
    }
}

// ---- identity tokens -----------------------------------------------------------------------------------------------------------------

/// What `jwtVerify` checks for these tokens: RS256 or ES256, the issuer's key by `kid`, issuer, audience, and the times with a five-second
/// tolerance. Returns the claims.
pub fn verify_id_token(token: &str, jwks: &Value, issuer: &str, audience: &str, now_seconds: i64) -> Result<Value, String> {
    use ring::signature;
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature_part] = parts.as_slice() else { return Err("malformed".into()) };
    let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| "malformed".to_owned());
    let header_json: Value = serde_json::from_slice(&decode(header)?).map_err(|_| "malformed")?;
    let algorithm = header_json["alg"].as_str().unwrap_or("");
    if !matches!(algorithm, "RS256" | "ES256") {
        return Err("algorithm".into());
    }
    let keys: Vec<&Value> = jwks["keys"].as_array().into_iter().flatten().filter(|key| {
        let kid_matches = header_json["kid"].as_str().is_none_or(|kid| key["kid"].as_str() == Some(kid));
        let kty = if algorithm == "RS256" { "RSA" } else { "EC" };
        key["kty"] == kty && kid_matches && key["use"].as_str().is_none_or(|usage| usage == "sig")
    }).collect();
    let signed = format!("{header}.{payload}");
    let sig = decode(signature_part)?;
    let verified = keys.iter().any(|key| match algorithm {
        "RS256" => match (key["n"].as_str().map(&decode), key["e"].as_str().map(&decode)) {
            (Some(Ok(n)), Some(Ok(e))) => signature::RsaPublicKeyComponents { n: &n, e: &e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig).is_ok(),
            _ => false,
        },
        _ => match (key["crv"].as_str(), key["x"].as_str().map(&decode), key["y"].as_str().map(&decode)) {
            (Some("P-256"), Some(Ok(x)), Some(Ok(y))) => {
                let mut point = vec![4u8];
                point.extend(x);
                point.extend(y);
                signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(signed.as_bytes(), &sig).is_ok()
            }
            _ => false,
        },
    });
    if !verified {
        return Err("signature".into());
    }
    let claims: Value = serde_json::from_slice(&decode(payload)?).map_err(|_| "malformed")?;
    if claims["iss"] != issuer {
        return Err("issuer".into());
    }
    let audiences: Vec<&str> = match &claims["aud"] {
        Value::String(single) => vec![single],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !audiences.contains(&audience) {
        return Err("audience".into());
    }
    if let Some(exp) = claims["exp"].as_f64() {
        if now_seconds as f64 - 5.0 >= exp {
            return Err("expired".into());
        }
    }
    if let Some(nbf) = claims["nbf"].as_f64() {
        if nbf > now_seconds as f64 + 5.0 {
            return Err("not yet valid".into());
        }
    }
    Ok(claims)
}

fn random(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// `codexAuthCallbackPage`: what the browser shows once the sign-in ends. The nonce lets the one script it carries run.
pub fn callback_page(success: bool, return_url: Option<&str>, nonce: &str) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;");
    let destination = provider_auth_return_url(return_url);
    let title = if success { "You're signed in" } else { "Sign-in couldn't finish" };
    let description = match (success, &destination) {
        (true, Some(_)) => "Returning to T3 Code. You're ready to continue.",
        (true, None) => "Return to T3 Code to continue. You can close this tab.",
        (false, _) => "Return to T3 Code and try signing in again.",
    };
    let (refresh, link) = match &destination {
        Some(destination) => (if success { format!("<meta http-equiv=\"refresh\" content=\"1;url={}\">", escape(destination)) } else { String::new() }, format!("<a href=\"{}\">Return to T3 Code</a>", escape(destination))),
        None => (String::new(), String::new()),
    };
    let (status_background, status_color, dark_background, dark_color) = if success { ("#ecfdf5", "#059669", "#064e3b", "#34d399") } else { ("#fef2f2", "#dc2626", "#450a0a", "#f87171") };
    let icon = if success { "<path d=\"m5 12 4 4L19 6\"/>" } else { "<path d=\"m6 6 12 12M6 18 18 6\"/>" };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta name=\"color-scheme\" content=\"light dark\"><title>{title} · T3 Code</title>\n{refresh}\n<style>\n:root{{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif;color:#18181b;background:#fafafa;color-scheme:light dark}}\n*{{box-sizing:border-box}}body{{margin:0;min-height:100svh;display:grid;place-items:center;padding:24px}}\nmain{{width:100%;max-width:440px;background:#fff;border:1px solid #e4e4e7;border-radius:20px;padding:32px;box-shadow:0 12px 40px #00000008}}\n.brand{{font-size:18px;letter-spacing:-.5px;margin-bottom:36px;color:#71717a}}.brand strong{{color:#18181b}}\n.status{{width:40px;height:40px;border-radius:50%;display:grid;place-items:center;background:{status_background};color:{status_color};margin-bottom:20px}}\nh1{{font-size:24px;font-weight:600;letter-spacing:-.7px;line-height:1.25;margin:0 0 12px}}p{{font-size:15px;line-height:1.6;color:#71717a;margin:0}}\na{{display:inline-flex;align-items:center;justify-content:center;margin-top:28px;padding:10px 16px;border-radius:8px;background:#2563eb;color:white;font-size:14px;font-weight:500;text-decoration:none}}a:focus-visible{{outline:3px solid #93c5fd;outline-offset:3px}}\n@media(prefers-color-scheme:dark){{:root{{color:#fafafa;background:#09090b}}main{{background:#18181b;border-color:#27272a;box-shadow:0 12px 40px #0003}}.brand,p{{color:#a1a1aa}}.brand strong{{color:#fafafa}}.status{{background:{dark_background};color:{dark_color}}}}}\n</style></head><body><main><div class=\"brand\"><strong>T3</strong> Code</div>\n<div class=\"status\" aria-hidden=\"true\"><svg width=\"22\" height=\"22\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\">{icon}</svg></div>\n<h1>{title}</h1><p>{description}</p>\n{link}\n</main><script nonce=\"{nonce}\">history.replaceState(null,\"\",\"/auth/callback\");</script></body></html>",
        title = escape(title),
        nonce = escape(nonce),
    )
}

// ---- the sign-in ----------------------------------------------------------------------------------------------------------------------

/// What OpenAI's discovery document says (`Discovery`).
#[derive(Debug, Clone, Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}
#[derive(Debug, Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    token_type: String,
    expires_in: i64,
    scope: String,
    #[serde(default)]
    earliest_refresh_at: Option<Value>,
}

pub struct Options {
    pub instance_id: String,
    /// The environment's identity, which OpenAI is told the client belongs to (`ext_agent_host_id`).
    pub environment_id: String,
    pub discovery_url: Option<String>,
    pub resource: Option<String>,
    pub default_return_url: Option<String>,
    pub reconnect_profile: Option<ChatGptReconnectProfile>,
    pub secrets: Arc<dyn Secrets>,
    pub http: reqwest::Client,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

struct Inner {
    options: Options,
    credentials_key: String,
    registration_key: String,
    metadata: tokio::sync::Mutex<Option<Metadata>>,
    /// One sign-in or refresh at a time per credential, within this process.
    gate: Arc<tokio::sync::Mutex<()>>,
}

/// The sign-in of one provider instance. Cloning shares it.
#[derive(Clone)]
pub struct ChatGptAuth(Arc<Inner>);

fn gates() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

fn earliest(value: &Option<Value>) -> Option<f64> {
    match value {
        None => None,
        Some(Value::Number(number)) => number.as_f64().map(|seconds| seconds * 1000.0),
        Some(Value::String(text)) => chrono::DateTime::parse_from_rfc3339(text).ok().map(|time| time.timestamp_millis() as f64),
        _ => None,
    }
}
fn scopes_of(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_owned).collect()
}

impl ChatGptAuth {
    pub fn new(options: Options) -> AuthResult<Self> {
        let credentials_key = binding_key("codex-chatgpt", &options.instance_id);
        let registration_key = binding_key("codex-chatgpt-registration", &options.instance_id);
        let gate_name = format!("{}\0{credentials_key}", options.secrets.directory().map_or_else(|| format!("memory-{:p}", Arc::as_ptr(&options.secrets)), |directory| directory.to_string_lossy().into_owned()));
        let gate = gates().lock().unwrap().entry(gate_name).or_default().clone();
        let this = Self(Arc::new(Inner { options, credentials_key, registration_key, metadata: Default::default(), gate }));
        // A profile handed over by the primary becomes this connection's registration before anything else happens.
        if let Some(profile) = this.0.options.reconnect_profile.clone() {
            let mut registration = serde_json::to_value(&profile).map_err(|_| this.fail("registration", "Could not save the ChatGPT sign-in registration. Try again."))?;
            registration.as_object_mut().map(|map| map.remove("idTokenHint"));
            let registration: Registration = serde_json::from_value(registration).map_err(|_| this.fail("registration", "Could not save the ChatGPT sign-in registration. Try again."))?;
            this.save_registration(registration)?;
        }
        Ok(this)
    }

    fn instance(&self) -> &str {
        &self.0.options.instance_id
    }
    fn fail(&self, operation: &str, detail: &str) -> ProviderSetupError {
        failure(self.instance(), operation, detail)
    }
    fn now(&self) -> i64 {
        (self.0.options.clock)()
    }
    fn resource(&self) -> String {
        self.0.options.resource.clone().unwrap_or_else(|| RESOURCE.to_owned())
    }

    // -- storage --

    fn read_sessions(&self) -> AuthResult<Sessions> {
        let bytes = self.0.options.secrets.get(&self.0.credentials_key).map_err(|_| self.fail("read", "Could not read the saved ChatGPT connection."))?;
        let Some(bytes) = bytes else { return Ok(Sessions { active_client_id: None, sessions: vec![] }) };
        let invalid = || self.fail("read", "The saved ChatGPT connection is invalid. Sign in again.");
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if value.get("sessions").is_some() {
            serde_json::from_value(value).map_err(|_| invalid())
        } else {
            let record: Credentials = serde_json::from_value(value).map_err(|_| invalid())?;
            Ok(Sessions { active_client_id: Some(record.client_id.clone()), sessions: vec![record] })
        }
    }
    /// The active connection's tokens.
    pub fn read(&self) -> AuthResult<Option<Credentials>> {
        let saved = self.read_sessions()?;
        Ok(saved.sessions.into_iter().find(|session| Some(&session.client_id) == saved.active_client_id.as_ref()))
    }
    fn write_sessions(&self, saved: &Sessions) -> AuthResult<()> {
        let json = serde_json::to_vec(saved).map_err(|_| self.fail("save", "Could not save the ChatGPT connection."))?;
        self.0.options.secrets.set(&self.0.credentials_key, &json).map_err(|_| self.fail("save", "Could not save the ChatGPT connection."))
    }
    fn read_registrations(&self) -> AuthResult<Registrations> {
        let bytes = self.0.options.secrets.get(&self.0.registration_key).map_err(|_| self.fail("registration", "Could not read the ChatGPT sign-in registration. Try again."))?;
        let Some(bytes) = bytes else { return Ok(Registrations { profiles: vec![], last_client_id: None }) };
        let invalid = || self.fail("registration", "The saved ChatGPT sign-in registration is invalid.");
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        let saved: Registrations = if value.get("profiles").is_some() {
            serde_json::from_value(value).map_err(|_| invalid())?
        } else {
            let single: Registration = serde_json::from_value(value).map_err(|_| invalid())?;
            Registrations { last_client_id: Some(single.client_id.clone()), profiles: vec![single] }
        };
        // The last connection first; a connection without a label is numbered by where it was saved.
        let labelled: Vec<Registration> = saved.profiles.iter().enumerate().map(|(index, profile)| Registration { connection_label: Some(profile.connection_label.clone().unwrap_or_else(|| format!("Connection {}", index + 1))), ..profile.clone() }).collect();
        let mut ordered = labelled.clone();
        if let Some(position) = labelled.iter().position(|profile| Some(&profile.client_id) == saved.last_client_id.as_ref()) {
            let last = ordered.remove(position);
            ordered.insert(0, last);
        }
        Ok(Registrations { profiles: ordered, last_client_id: saved.last_client_id })
    }
    fn save_registration(&self, mut profile: Registration) -> AuthResult<()> {
        let saved = self.read_registrations()?;
        profile.connection_label = saved.profiles.iter().find(|entry| entry.client_id == profile.client_id).and_then(|entry| entry.connection_label.clone()).or(profile.connection_label.clone()).or_else(|| Some(format!("Connection {}", saved.profiles.len() + 1)));
        let mut profiles = vec![profile.clone()];
        profiles.extend(saved.profiles.into_iter().filter(|entry| entry.client_id != profile.client_id));
        let json = serde_json::to_vec(&Registrations { profiles, last_client_id: Some(profile.client_id) }).map_err(|_| self.fail("registration", "Could not save the ChatGPT sign-in registration. Try again."))?;
        self.0.options.secrets.set(&self.0.registration_key, &json).map_err(|_| self.fail("registration", "Could not save the ChatGPT sign-in registration. Try again."))
    }
    /// The active pointer and every connection's tokens change in one write.
    fn save(&self, record: Credentials, activate: bool) -> AuthResult<()> {
        let saved = self.read_sessions()?;
        let mut sessions: Vec<Credentials> = saved.sessions.into_iter().filter(|session| session.client_id != record.client_id).collect();
        let active_client_id = if activate { Some(record.client_id.clone()) } else { saved.active_client_id };
        sessions.push(record);
        self.write_sessions(&Sessions { active_client_id, sessions })
    }
    fn clear_tokens(&self) -> AuthResult<()> {
        let saved = self.read_sessions()?;
        let sessions: Vec<Credentials> = saved.sessions.into_iter().filter(|session| Some(&session.client_id) != saved.active_client_id.as_ref()).collect();
        if sessions.is_empty() {
            self.0.options.secrets.remove(&self.0.credentials_key).map_err(|_| self.fail("disconnect", "Could not clear the ChatGPT connection. Try again."))
        } else {
            self.write_sessions(&Sessions { active_client_id: None, sessions })
        }
    }
    /// Forgets the active connection's tokens, keeping its identity in the registration for the next sign-in.
    fn remove(&self) -> AuthResult<()> {
        if let Some(credentials) = self.read()? {
            let saved = self.read_registrations()?;
            if let Some(profile) = saved.profiles.iter().find(|entry| entry.client_id == credentials.client_id) {
                if profile.subject.is_none() || profile.email.is_none() {
                    self.save_registration(Registration { subject: Some(credentials.subject.clone()), email: Some(credentials.email.clone()), ..profile.clone() })?;
                }
            }
        }
        self.clear_tokens()
    }
    async fn locked(&self) -> AuthResult<SessionLock> {
        SessionLock::acquire(self.0.options.secrets.directory(), &self.0.credentials_key, self.instance()).await
    }

    // -- OpenAI --

    async fn discover(&self) -> AuthResult<Metadata> {
        if let Some(metadata) = self.0.metadata.lock().await.clone() {
            return Ok(metadata);
        }
        let unreachable = || self.fail("discover", "Could not reach ChatGPT sign-in. Try again.");
        let response = self.0.options.http.get(self.0.options.discovery_url.as_deref().unwrap_or(DISCOVERY)).send().await.map_err(|_| unreachable())?;
        if !response.status().is_success() {
            return Err(unreachable());
        }
        let metadata: Metadata = response.json().await.map_err(|_| unreachable())?;
        if self.0.options.discovery_url.is_none() {
            let origin = |url: &str| url::Url::parse(url).map(|url| url.origin().ascii_serialization()).unwrap_or_default();
            let endpoints = [Some(&metadata.authorization_endpoint), Some(&metadata.token_endpoint), Some(&metadata.jwks_uri), metadata.revocation_endpoint.as_ref()];
            if metadata.issuer != ISSUER || endpoints.into_iter().flatten().any(|endpoint| origin(endpoint) != metadata.issuer) {
                return Err(self.fail("discover", "ChatGPT sign-in configuration could not be verified."));
            }
        }
        *self.0.metadata.lock().await = Some(metadata.clone());
        Ok(metadata)
    }
    async fn verify_identity(&self, token: &str, metadata: &Metadata, audience: &str) -> Result<Value, String> {
        let jwks: Value = self.0.options.http.get(&metadata.jwks_uri).send().await.map_err(|error| error.to_string())?.json().await.map_err(|error| error.to_string())?;
        verify_id_token(token, &jwks, &metadata.issuer, audience, self.now() / 1000)
    }
    async fn exchange(&self, body: &[(&str, String)]) -> AuthResult<Tokens> {
        let metadata = self.discover().await?;
        let refreshing = body.iter().any(|(key, value)| *key == "grant_type" && value == "refresh_token");
        let granting = body.iter().any(|(key, value)| *key == "grant_type" && value == "authorization_code");
        let response = self.0.options.http.post(&metadata.token_endpoint).header("accept", "application/json").form(body).send().await.map_err(|_| self.fail("exchange", "ChatGPT sign-in is temporarily unavailable. Try again."))?;
        let ok = response.status().is_success();
        let raw: Value = response.json().await.map_err(|_| self.fail("exchange", "ChatGPT sign-in is temporarily unavailable. Try again."))?;
        if !ok {
            let error = raw["error"].as_str().ok_or_else(|| self.fail("exchange", "ChatGPT did not accept this sign-in. Try again."))?;
            if refreshing && ["invalid_grant", "invalid_refresh_token", "token_expired", "refresh_token_expired", "refresh_token_invalidated", "refresh_token_reused"].contains(&error) {
                self.remove()?;
                return Err(self.fail("refresh", "Your ChatGPT connection expired or was disconnected. Sign in again."));
            }
            if error == "invalid_client" {
                return Err(self.fail("client", "OpenAI rejected this app's client registration. Check the ChatGPT connection configuration."));
            }
            if granting && error == "invalid_grant" {
                return Err(self.fail("code-expired", "This sign-in code expired. Start again."));
            }
            return Err(self.fail("exchange", if error == "access_denied" { "ChatGPT sign-in was declined. Sign in again when you are ready." } else { "ChatGPT could not complete sign-in. Try again." }));
        }
        let tokens: Tokens = serde_json::from_value(raw).map_err(|_| self.fail("exchange", "ChatGPT returned an invalid token response. Sign in again."))?;
        if tokens.access_token.is_empty() || tokens.refresh_token.as_deref() == Some("") || tokens.expires_in <= 0 {
            return Err(self.fail("exchange", "ChatGPT returned an invalid token response. Sign in again."));
        }
        Ok(tokens)
    }

    /// `access`: the active connection's tokens, renewed when they are about to lapse.
    pub async fn access(&self) -> AuthResult<Credentials> {
        let _gate = self.0.gate.lock().await;
        let _lock = self.locked().await?;
        let mut record = self.read()?.ok_or_else(|| self.fail("access", "Sign in with ChatGPT to use managed Codex."))?;
        if !record.scopes.iter().any(|scope| scope == REQUIRED_SCOPE) {
            return Err(self.fail("sharing", "Token sharing is disabled. Sign in with ChatGPT and enable token sharing."));
        }
        let now = self.now() as f64;
        if record.expires_at - now > 60_000.0 {
            return Ok(record);
        }
        if record.earliest_refresh_at.is_some_and(|at| at > now) {
            if record.expires_at > now {
                return Ok(record);
            }
            return Err(self.fail("refresh", "ChatGPT cannot renew this connection yet. Try again shortly."));
        }
        let Some(refresh_token) = record.refresh_token.clone() else {
            self.remove()?;
            return Err(self.fail("refresh", "Your ChatGPT connection expired. Sign in again."));
        };
        let body = [("grant_type", "refresh_token".to_owned()), ("client_id", record.client_id.clone()), ("refresh_token", refresh_token), ("resource", self.resource())];
        let tokens = match tokio::time::timeout(Duration::from_secs(20), self.exchange(&body)).await {
            Ok(Ok(tokens)) => tokens,
            Ok(Err(error)) if error.operation == "client" => return Err(error),
            // TODO(original CodexChatGptAuth.ts): every other failure, even the "expired or was disconnected" answer that has just signed
            // the connection out, is reported as this one, so the user is told to retry something that was forgotten.
            _ => return Err(self.fail("refresh", "Could not renew the ChatGPT connection. Retry, or sign in again.")),
        };
        let Some(refresh_token) = tokens.refresh_token.clone().filter(|_| tokens.token_type.eq_ignore_ascii_case("bearer")) else {
            return Err(self.fail("refresh", "ChatGPT returned an invalid renewal. Sign in again."));
        };
        record.access_token = tokens.access_token.clone();
        record.refresh_token = Some(refresh_token);
        record.expires_at = self.now() as f64 + tokens.expires_in as f64 * 1000.0;
        record.earliest_refresh_at = earliest(&tokens.earliest_refresh_at);
        record.scopes = scopes_of(&tokens.scope);
        self.save(record.clone(), true)?;
        if !record.scopes.iter().any(|scope| scope == REQUIRED_SCOPE) {
            return Err(self.fail("sharing", "ChatGPT token sharing is no longer enabled. Sign in again."));
        }
        Ok(record)
    }

    /// `logout`: revokes the refresh token with OpenAI (three tries on a server error), then forgets the tokens whatever came of it. The
    /// answer is a warning when the revocation could not be confirmed.
    pub async fn logout(&self) -> AuthResult<Option<String>> {
        let _gate = self.0.gate.lock().await;
        let _lock = self.locked().await?;
        let credentials = self.read()?;
        let mut confirmed = credentials.as_ref().is_none_or(|credentials| credentials.refresh_token.is_none());
        if let Some(Credentials { refresh_token: Some(refresh_token), client_id, .. }) = &credentials {
            for attempt in 0..3 {
                let outcome = tokio::time::timeout(Duration::from_secs(10), async {
                    // A lookup or a request that fails is tried again; a document with no revocation endpoint is not.
                    let Ok(metadata) = self.discover().await else { return (false, true) };
                    let Some(endpoint) = metadata.revocation_endpoint else { return (false, false) };
                    match self.0.options.http.post(endpoint).form(&[("token", refresh_token.as_str()), ("token_type_hint", "refresh_token"), ("client_id", client_id.as_str())]).send().await {
                        Ok(response) => (response.status().as_u16() == 200, response.status().as_u16() >= 500),
                        Err(_) => (false, true),
                    }
                })
                .await;
                let (done, retry) = outcome.unwrap_or((false, true));
                confirmed = done;
                if confirmed || !retry || attempt == 2 {
                    break;
                }
                tokio::time::sleep(if attempt == 0 { Duration::from_millis(250) } else { Duration::from_secs(1) }).await;
            }
        }
        self.remove()?;
        Ok((!confirmed).then(|| "Signed out locally. Remote revocation could not be confirmed. Disconnect the app in ChatGPT Settings.".to_owned()))
    }
}

// ---- the browser round trip ------------------------------------------------------------------------------------------------------

/// A callback received: its URL, and where to say how it went (the browser waits for the page until the sign-in is verified).
type Received = (url::Url, Option<oneshot::Sender<bool>>);

struct Callbacks {
    used: std::sync::atomic::AtomicBool,
    received: mpsc::Sender<Received>,
    return_url: Option<String>,
    page_nonce: String,
}

async fn answer_callback(axum::extract::State(callbacks): axum::extract::State<Arc<Callbacks>>, request: axum::http::Request<axum::body::Body>) -> axum::response::Response {
    use axum::{body::Body, http::{StatusCode, header}, response::Response};
    let plain = |status: StatusCode, text: &'static str| Response::builder().status(status).body(Body::from(text)).unwrap();
    if request.method() != axum::http::Method::GET || request.uri().path() != "/auth/callback" {
        return plain(StatusCode::NOT_FOUND, "");
    }
    if callbacks.used.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return plain(StatusCode::GONE, "Sign-in is no longer active.");
    }
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{}", request.uri().path_and_query().map_or("/", |target| target.as_str()))) else { return plain(StatusCode::BAD_REQUEST, "") };
    let (reply, outcome) = oneshot::channel();
    if callbacks.received.send((url, Some(reply))).await.is_err() {
        return plain(StatusCode::GONE, "Sign-in is no longer active.");
    }
    let success = outcome.await.unwrap_or(false);
    let nonce = &callbacks.page_nonce;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header("content-security-policy", format!("default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-{nonce}'; base-uri 'none'; frame-ancestors 'none'"))
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(callback_page(success, callbacks.return_url.as_deref(), nonce)))
        .unwrap()
}

struct Listening(tokio::task::JoinHandle<()>);
impl Drop for Listening {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ChatGptAuth {
    /// `methods`: the sign-ins this connection offers: the saved account, a different one, and each saved profile (the wire allows 32).
    pub fn methods(&self) -> AuthResult<Vec<ProviderAuthMethod>> {
        let saved = self.read_registrations()?;
        let active = self.read()?;
        let current = saved.profiles.iter().find(|profile| Some(&profile.client_id) == active.as_ref().map(|active| &active.client_id).or(saved.last_client_id.as_ref()));
        let mut profiles: Vec<&Registration> = current.into_iter().collect();
        profiles.extend(saved.profiles.iter().filter(|profile| Some(&profile.client_id) != current.map(|current| &current.client_id)));
        let email = current.and_then(|profile| profile.email.clone().flatten());
        let mut methods = vec![
            json!({"id":"chatgpt","name":"Sign in with ChatGPT","description":email.as_ref().map(|email| format!("Reconnect {email}.")),"type":"agent"}),
            json!({"id":"chatgpt-change-account","name":"Use a different ChatGPT account","description":"Register a connection for another ChatGPT account.","type":"agent"}),
        ];
        if let (Some(email), Some(first)) = (&email, methods.first_mut()) {
            first["accountEmail"] = json!(email);
        }
        for profile in profiles.into_iter().take(30) {
            let mut method = json!({"id":format!("chatgpt-profile:{}", profile.client_id),"name":format!("{} · {}", profile.email.clone().flatten().unwrap_or_else(|| "ChatGPT account".into()), profile.connection_label.clone().unwrap_or_default()),"description":"Reuse this account's original sign-in registration.","type":"agent"});
            if let Some(email) = profile.email.clone().flatten() {
                method["accountEmail"] = json!(email);
            }
            methods.push(method);
        }
        serde_json::from_value(Value::Array(methods)).map_err(|_| self.fail("methods", "Could not list the ChatGPT sign-in methods."))
    }

    /// `authenticate`: one sign-in, from the browser leaving to the tokens being saved.
    pub async fn authenticate(&self, method: &str, context: AuthContext) -> AuthResult<()> {
        let _gate = self.0.gate.lock().await;
        let metadata = self.discover().await?;
        let previous = self.read()?;
        let registrations = self.read_registrations()?;
        let changing_account = method == "chatgpt-change-account";
        let selected_client_id = match method.strip_prefix("chatgpt-profile:") {
            Some(client_id) => Some(client_id.to_owned()),
            None => previous.as_ref().map(|previous| previous.client_id.clone()).or(registrations.last_client_id.clone()),
        };
        let saved_registration = if changing_account { None } else { registrations.profiles.iter().find(|profile| Some(&profile.client_id) == selected_client_id.as_ref()).cloned() };
        // Older development registrations used localhost, which cannot be changed on reauth: register a 127.0.0.1 connection instead.
        let legacy_callback = saved_registration.as_ref().and_then(|registration| registration.redirect_uri.as_deref()).is_some_and(|uri| uri.contains("//localhost:"));
        let registered_client_id = if legacy_callback { None } else { saved_registration.as_ref().map(|registration| registration.client_id.clone()) };
        let selected_tokens = self.read_sessions()?.sessions.into_iter().find(|session| Some(&session.client_id) == selected_client_id.as_ref());
        let (state, nonce, verifier) = (random(32), random(32), random(64));
        let return_url = provider_auth_return_url(context.return_url.as_deref()).or_else(|| provider_auth_return_url(self.0.options.default_return_url.as_deref()));
        let client_callback = context.callback_mode == Some(ProviderAuthCallbackMode::Client);
        let (received_tx, mut received_rx) = mpsc::channel::<Received>(1);
        let callbacks = Arc::new(Callbacks { used: Default::default(), received: received_tx, return_url: return_url.clone(), page_nonce: random(24) });
        // The listener serves the browser's return; with the client delivering the callback instead there is none, and the port is only a name.
        let (port, _listening) = if client_callback {
            (rand::Rng::random_range(&mut rand::rng(), 49_152..=65_535u16), None)
        } else {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|_| self.fail("callback", "Could not start the local sign-in callback. Try again."))?;
            let port = socket.local_addr().map_err(|_| self.fail("callback", "Could not start the local sign-in callback."))?.port();
            let router = axum::Router::new().fallback(answer_callback).with_state(callbacks.clone());
            (port, Some(Listening(tokio::spawn(async move { let _ = axum::serve(socket, router).await; }))))
        };
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let id_token_hint = selected_tokens.as_ref().and_then(|tokens| tokens.id_token.clone()).or_else(|| self.0.options.reconnect_profile.as_ref().and_then(|profile| profile.id_token_hint.as_ref().map(|hint| hint.0.clone())));
        let mut url = url::Url::parse(&metadata.authorization_endpoint).map_err(|_| self.fail("discover", "ChatGPT sign-in configuration could not be verified."))?;
        {
            let mut query = url.query_pairs_mut();
            query.clear();
            query.append_pair("client_id", registered_client_id.as_deref().unwrap_or("dynamic_agent_client"));
            if registered_client_id.is_some() {
                if let Some(email) = saved_registration.as_ref().and_then(|registration| registration.email.clone().flatten()) {
                    query.append_pair("login_hint", &email);
                }
                if let Some(hint) = &id_token_hint {
                    query.append_pair("id_token_hint", hint);
                }
                if saved_registration.as_ref().is_some_and(|registration| registration.sharing_enabled == Some(false)) || selected_tokens.as_ref().is_some_and(|tokens| !tokens.scopes.iter().any(|scope| scope == REQUIRED_SCOPE)) {
                    query.append_pair("prompt", "consent");
                }
            } else {
                query.append_pair("agent_name_hint", "T3 Code");
            }
            query
                .append_pair("ext_agent_host_id", &format!("urn:uuid:{}", self.0.options.environment_id))
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("scope", SCOPE)
                .append_pair("resource", &self.resource())
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())));
        }
        let interaction: ProviderAuthInteraction = serde_json::from_value(json!({"type":"browser","id":context.flow_id,"url":url.to_string(),"requiresConsent":false,"acceptsCallback":true})).map_err(|_| self.fail("callback", "Could not start the local sign-in callback."))?;
        // A callback the client brings (it received the browser's return on another machine) is held to the same checks.
        let complete: Complete = {
            let (callbacks, redirect_uri, state, instance) = (callbacks.clone(), redirect_uri.clone(), state.clone(), self.instance().to_owned());
            Arc::new(move |callback_url: String| {
                let (callbacks, redirect_uri, state, instance) = (callbacks.clone(), redirect_uri.clone(), state.clone(), instance.clone());
                Box::pin(async move {
                    let rejected = || failure(&instance, "complete", "This redirect URL does not belong to the active ChatGPT sign-in.");
                    let returned = codex_callback_url(&callback_url, &redirect_uri, &state).map_err(|_| rejected())?;
                    if callbacks.used.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return Err(rejected());
                    }
                    callbacks.received.send((returned, None)).await.map_err(|_| rejected())
                })
            })
        };
        context.set_interaction(interaction, None, Some(complete));
        let (returned, response) = received_rx.recv().await.ok_or_else(|| self.fail("callback", "ChatGPT sign-in could not be completed."))?;
        let result = self.finish_sign_in(&context, &metadata, &returned, &state, &nonce, &verifier, &redirect_uri, previous.as_ref(), registered_client_id.as_deref(), saved_registration.as_ref(), &registrations).await;
        if let Some(response) = response {
            let _ = response.send(result.is_ok());
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_sign_in(&self, context: &AuthContext, metadata: &Metadata, returned: &url::Url, state: &str, nonce: &str, verifier: &str, redirect_uri: &str, previous: Option<&Credentials>, registered_client_id: Option<&str>, saved_registration: Option<&Registration>, registrations: &Registrations) -> AuthResult<()> {
        let param = |key: &str| returned.query_pairs().find(|(name, _)| name == key).map(|(_, value)| value.into_owned());
        if param("state").as_deref() != Some(state) {
            return Err(self.fail("callback", "ChatGPT sign-in could not be verified. Start again."));
        }
        if let Some(error) = param("error") {
            return Err(self.fail("callback", if error == "access_denied" { "ChatGPT sign-in was declined. Sign in again when you are ready." } else { "ChatGPT sign-in could not be completed. Start again." }));
        }
        let code = param("code");
        let client_id = registered_client_id.map(str::to_owned).or_else(|| param("client_id"));
        let incomplete = || self.fail("callback", "ChatGPT registration is incomplete. Start sign-in again.");
        let (Some(code), Some(client_id)) = (code.filter(|code| !code.is_empty()), client_id) else { return Err(incomplete()) };
        if !client_id.starts_with("oaiapp_") || (registered_client_id.is_some() && param("client_id").is_some_and(|returned| Some(returned.as_str()) != registered_client_id)) {
            return Err(incomplete());
        }
        context.verifying();
        let body = [("grant_type", "authorization_code".to_owned()), ("client_id", client_id.clone()), ("code", code), ("code_verifier", verifier.to_owned()), ("redirect_uri", redirect_uri.to_owned()), ("resource", self.resource())];
        let tokens = match self.exchange(&body).await {
            Ok(tokens) => tokens,
            Err(error) if error.operation == "code-expired" => {
                if registered_client_id.is_none() {
                    let _lock = self.locked().await?;
                    self.save_registration(Registration { client_id: client_id.clone(), subject: None, connection_label: None, sharing_enabled: None, email: None, redirect_uri: Some(redirect_uri.to_owned()) })?;
                }
                return Err(self.fail("exchange", "This sign-in code expired. Reconnect the saved ChatGPT profile to start a fresh sign-in."));
            }
            Err(error) => return Err(error),
        };
        let Some(id_token) = tokens.id_token.clone().filter(|token| !token.is_empty()) else { return Err(self.fail("verify", "ChatGPT did not return a verified identity. Sign in again.")) };
        let unverified = || self.fail("verify", "ChatGPT sign-in could not be verified. Start again.");
        let claims = self.verify_identity(&id_token, metadata, &client_id).await.map_err(|_| unverified())?;
        if claims["nonce"] != nonce || claims["sub"].as_str().is_none_or(str::is_empty) || claims["exp"].is_null() {
            return Err(unverified());
        }
        let subject = claims["sub"].as_str().unwrap_or_default().to_owned();
        let email = claims["email"].as_str().map(str::to_owned);
        let expected_subject = saved_registration.and_then(|registration| registration.subject.clone()).or_else(|| previous.filter(|previous| Some(previous.client_id.as_str()) == registered_client_id).map(|previous| previous.subject.clone()));
        // Subjects are checked within one registration; the migration of a legacy callback registers a new client, whose subject cannot be compared with the old one.
        if registered_client_id.is_some() && expected_subject.as_deref().is_some_and(|expected| expected != subject) {
            return Err(self.fail("verify", "This sign-in returned a different ChatGPT account. Use the different-account sign-in option instead. Your saved connection is unchanged."));
        }
        if let Some(known) = registrations.profiles.iter().find(|profile| profile.client_id == client_id) {
            let moved = known.redirect_uri.as_deref().is_some_and(|original| match (url::Url::parse(original), url::Url::parse(redirect_uri)) {
                (Ok(original), Ok(current)) => original.scheme() != current.scheme() || original.host_str() != current.host_str() || original.path() != current.path(),
                _ => true,
            });
            if moved || known.subject.as_deref().is_some_and(|known| known != subject) {
                return Err(self.fail("verify", "ChatGPT returned a conflicting account registration. Start again."));
            }
        }
        if !tokens.token_type.eq_ignore_ascii_case("bearer") {
            return Err(self.fail("verify", "ChatGPT returned an unsupported connection. Sign in again."));
        }
        let scopes = scopes_of(&tokens.scope);
        let sharing = scopes.iter().any(|scope| scope == REQUIRED_SCOPE);
        {
            let _lock = self.locked().await?;
            self.save_registration(Registration { client_id: client_id.clone(), subject: Some(subject.clone()), connection_label: None, sharing_enabled: Some(sharing), email: Some(email.clone()), redirect_uri: Some(redirect_uri.to_owned()) })?;
            let activate = sharing || self.read()?.is_none();
            self.save(Credentials { client_id: client_id.clone(), access_token: tokens.access_token.clone(), refresh_token: tokens.refresh_token.clone(), expires_at: self.now() as f64 + tokens.expires_in as f64 * 1000.0, earliest_refresh_at: earliest(&tokens.earliest_refresh_at), scopes, subject, email, id_token: Some(id_token), issuer: Some(metadata.issuer.clone()) }, activate)?;
        }
        if !sharing {
            return Err(self.fail("sharing", if previous.is_some_and(|previous| previous.client_id != client_id) { "Token sharing was not enabled for the new account. Your existing ChatGPT connection is unchanged." } else { "Signed in with ChatGPT, but token sharing is disabled. Sign in again and enable token sharing, or use another provider." }));
        }
        Ok(())
    }

    // -- moving a connection between environments --

    /// `reconnectProfile`: the registration a primary hands to the environment that will sign in, with the identity hint of the last tokens.
    /// `None` for the different-account sign-in, and for a default sign-in nothing has registered yet.
    pub fn reconnect_profile(&self, method_id: &str) -> AuthResult<Option<ChatGptReconnectProfile>> {
        if method_id == "chatgpt-change-account" {
            return Ok(None);
        }
        let registrations = self.read_registrations()?;
        let active = self.read()?;
        let client_id = match method_id.strip_prefix("chatgpt-profile:") {
            Some(client_id) => Some(client_id.to_owned()),
            None => active.as_ref().map(|active| active.client_id.clone()).or(registrations.last_client_id.clone()),
        };
        let Some(registration) = registrations.profiles.iter().find(|profile| Some(&profile.client_id) == client_id.as_ref()) else {
            if method_id.starts_with("chatgpt-profile:") {
                return Err(self.fail("export", "This saved connection is no longer available."));
            }
            return Ok(None);
        };
        let session = self.read_sessions()?.sessions.into_iter().find(|session| Some(&session.client_id) == client_id.as_ref());
        let mut profile = serde_json::to_value(registration).map_err(|_| self.fail("export", "This saved connection is no longer available."))?;
        if let Some(hint) = session.and_then(|session| session.id_token) {
            profile["idTokenHint"] = json!(hint);
        }
        serde_json::from_value(profile).map(Some).map_err(|_| self.fail("export", "This saved connection is no longer available."))
    }

    /// `exportProfile`: the finished sign-in as a profile another environment can import.
    pub fn export_profile(&self) -> AuthResult<ChatGptTransferredProfile> {
        let incomplete = || self.fail("export", "Complete ChatGPT sign-in before transferring this connection.");
        let credentials = self.read()?.ok_or_else(incomplete)?;
        let registration = self.read_registrations()?.profiles.into_iter().find(|profile| profile.client_id == credentials.client_id).ok_or_else(incomplete)?;
        if credentials.id_token.is_none() || credentials.issuer.is_none() {
            return Err(incomplete());
        }
        serde_json::from_value(json!({"registration":registration,"credentials":credentials})).map_err(|_| incomplete())
    }

    /// `importProfile`: validates a connection another environment signed in and keeps it as this one's. `stop` ends the sessions that run on
    /// the old tokens first.
    pub async fn import_profile(&self, profile: &ChatGptTransferredProfile, stop: BoxFuture<'static, AuthResult<()>>) -> AuthResult<()> {
        let _gate = self.0.gate.lock().await;
        let metadata = self.discover().await?;
        let credentials = &profile.credentials;
        let expires_at = credentials.expires_at.as_f64().unwrap_or(0.0);
        if credentials.client_id != profile.registration.client_id.as_str() || credentials.issuer != metadata.issuer || !credentials.scopes.iter().any(|scope| scope == REQUIRED_SCOPE) || expires_at <= self.now() as f64 {
            return Err(self.fail("import", "The transferred ChatGPT connection is invalid or expired."));
        }
        let claims = self.verify_identity(&credentials.id_token.0, &metadata, &credentials.client_id).await.map_err(|_| self.fail("import", "The transferred ChatGPT identity could not be verified."))?;
        let mismatched = claims["sub"].as_str() != Some(credentials.subject.as_str()) || profile.registration.subject.as_deref() != Some(credentials.subject.as_str()) || claims["email"].as_str().map(str::to_owned) != credentials.email || profile.registration.email.as_ref().is_some_and(|email| email != &credentials.email);
        if mismatched {
            return Err(self.fail("import", "The transferred ChatGPT identity does not match its profile."));
        }
        let _lock = self.locked().await?;
        let mut registration = serde_json::to_value(&profile.registration).map_err(|_| self.fail("import", "The transferred ChatGPT connection is invalid or expired."))?;
        registration.as_object_mut().map(|map| map.remove("idTokenHint"));
        let registration: Registration = serde_json::from_value(registration).map_err(|_| self.fail("import", "The transferred ChatGPT connection is invalid or expired."))?;
        if self.read_registrations()?.profiles.iter().find(|saved| saved.client_id == registration.client_id).and_then(|saved| saved.subject.as_ref()).is_some_and(|subject| subject != &credentials.subject) {
            return Err(self.fail("import", "The transferred identity conflicts with the saved ChatGPT connection."));
        }
        stop.await?;
        let record: Credentials = serde_json::from_value(serde_json::to_value(credentials).map_err(|_| self.fail("import", "The transferred ChatGPT connection is invalid or expired."))?).map_err(|_| self.fail("import", "The transferred ChatGPT connection is invalid or expired."))?;
        self.save_registration(registration)?;
        self.save(record, true)
    }
}

/// The flow's view of a sign-in.
impl AuthBackend for ChatGptAuth {
    fn methods(&self) -> BoxFuture<'static, AuthResult<Vec<ProviderAuthMethod>>> {
        let this = self.clone();
        Box::pin(async move { ChatGptAuth::methods(&this) })
    }
    fn authenticate(&self, method: String, context: AuthContext) -> BoxFuture<'static, AuthResult<()>> {
        let this = self.clone();
        Box::pin(async move { ChatGptAuth::authenticate(&this, &method, context).await })
    }
    fn cleanup(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
    fn cleanup_methods(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
    fn logout(&self) -> BoxFuture<'static, AuthResult<Option<String>>> {
        let this = self.clone();
        Box::pin(async move { ChatGptAuth::logout(&this).await })
    }
}
