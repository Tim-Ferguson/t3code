//! T3 Connect in the server: the cloud link, the managed tunnel and the relay client of `t3-remote`, with the services the
//! server provides them (its secret store, its environment descriptor, its pairing links, the agent-activity publisher, the
//! update handoff markers), started when the routes serve and shut down with the server. The wiring specification is the
//! crate-root documentation of `t3-remote`; the HTTP routes are `connect_http`, the two RPCs `connect_rpc`.
//!
//! The original always builds these services (`CloudLink.layer`), so the routes exist, and answer, on a server nobody linked;
//! what a fresh clone lacks is the public configuration (relay URL, Clerk keys), and then the server makes no request to a
//! relay: startup only reads what is stored (nothing), and every outbound call needs a link or a CLI sign-in first.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use async_trait::async_trait;
use serde_json::Value;
use t3_contracts::ExecutionEnvironmentDescriptor;
use t3_remote::{
    backoff::{Random, ThreadRandom},
    clock::{Clock, SystemClock},
    cloud_link::{AwarenessCatchUp, EnvironmentIdentity, IssuedPairing, PairingIssuer, PairingRequest, UpdateHandoff},
    cli_token::CliTokenManager,
    config::{BuildDefaults, CloudCliOAuthConfig, cloud_cli_oauth_config, has_cloud_public_config, hosted_app_url_config, relay_url_config, DEFAULT_HOSTED_APP_URL},
    control_plane::{ControlPlane, HttpControlPlane},
    endpoint_runtime::{ConnectorSpawner, ProcessConnectorSpawner},
    relay_client::{Cloudflared, CloudflaredOptions, HostPlatform, RelayClientResolver},
    runtime::{RemoteConfig, RemoteServices},
    secrets::{SecretError, SecretStore, read_agent_activity_publishing_active},
};

use crate::{
    agent_awareness::AgentAwareness,
    auth::AuthService,
    persistence::Store,
    server_secret_store::{ServerSecretStore, SecretStoreError},
};

/// The server's secret store, as `t3-remote` reads and writes it (`ServerSecretStore` of the original).
pub struct ConnectSecrets(pub ServerSecretStore);

fn secret_error(error: SecretStoreError) -> SecretError {
    SecretError { already_exists: error.already_exists(), message: error.to_string() }
}

impl SecretStore for ConnectSecrets {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
        self.0.get(name).map_err(secret_error)
    }
    fn set(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        self.0.set(name, value).map_err(secret_error)
    }
    fn create(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        self.0.create(name, value).map_err(secret_error)
    }
    fn remove(&self, name: &str) -> Result<(), SecretError> {
        self.0.remove(name).map_err(secret_error)
    }
}

/// Who this environment is. The capability `agentActivityPublishing` changes at runtime (`t3 connect publish`, the client's
/// toggle), so the descriptor is read per request rather than fixed at startup (`ServerEnvironment.getDescriptor`).
struct ServerEnvironmentIdentity {
    environment: Value,
    secrets: Arc<ConnectSecrets>,
}

fn descriptor_of(environment: &Value, secrets: &dyn SecretStore) -> Value {
    let mut descriptor = environment.clone();
    if let Some(capabilities) = descriptor.get_mut("capabilities").and_then(Value::as_object_mut) {
        capabilities.insert("agentActivityPublishing".into(), Value::Bool(read_agent_activity_publishing_active(secrets)));
    }
    descriptor
}

impl EnvironmentIdentity for ServerEnvironmentIdentity {
    fn environment_id(&self) -> String {
        self.environment["environmentId"].as_str().unwrap_or_default().to_owned()
    }
    fn descriptor(&self) -> ExecutionEnvironmentDescriptor {
        serde_json::from_value(descriptor_of(&self.environment, self.secrets.as_ref())).expect("the environment descriptor is valid")
    }
}

/// `EnvironmentAuth.createPairingLink` for the clients the relay vouches for.
struct ServerPairing {
    auth: AuthService,
}

#[async_trait]
impl PairingIssuer for ServerPairing {
    async fn create_pairing_link(&self, request: PairingRequest) -> Result<IssuedPairing, String> {
        let now = chrono::Utc::now();
        let ttl = chrono::Duration::from_std(request.ttl).map_err(|error| error.to_string())?;
        let issued = self
            .auth
            .issue_pairing_link_bound(request.scopes, request.subject, Some(request.label), now, ttl, Some(&request.proof_key_thumbprint))
            .map_err(|error| error.to_string())?;
        Ok(IssuedPairing { credential: issued.credential, expires_at_ms: issued.expires_at.timestamp_millis() })
    }
}

/// The markers a launcher or the desktop app leaves under `<home>/runtime` (`cloud/updateHandoff.ts`).
struct Handoff {
    runtime_dir: PathBuf,
    clock: Arc<dyn Clock>,
}

/// The desktop app stops its backends within seconds of writing the marker.
const DESKTOP_UPDATE_RESTART_MARKER_TTL_MS: i64 = 60_000;
const SERVICE_STATE_FILE: &str = "service-state.json";
const SERVICE_STOP_MARKER_FILE: &str = ".service-stopping";

impl Handoff {
    fn pending_service_update_exists(&self) -> bool {
        std::fs::read_to_string(self.runtime_dir.join(SERVICE_STATE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|state| state["update"]["status"] == "pending")
    }
}

#[async_trait]
impl UpdateHandoff for Handoff {
    /// Reading consumes the marker, so shutdown checks it first. Only a fresh one counts, so a marker the server never read (a
    /// hard kill) cannot keep the tunnel on a later quit.
    async fn desktop_update_restart_pending(&self) -> bool {
        let marker = self.runtime_dir.join(t3_contracts::DESKTOP_UPDATE_RESTART_MARKER_FILE);
        let Ok(metadata) = std::fs::metadata(&marker) else { return false };
        let _ = std::fs::remove_file(&marker);
        let Some(written) = metadata.modified().ok().and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok()) else { return false };
        self.clock.now_ms() - (written.as_millis() as i64) < DESKTOP_UPDATE_RESTART_MARKER_TTL_MS
    }

    /// A pending update alone is not proof a replacement server is coming: an explicit launcher stop during the pending window
    /// also tears this server down. The launcher marks that case just before it signals the child.
    async fn pending_update_handoff_exists(&self) -> bool {
        self.pending_service_update_exists() && !self.runtime_dir.join(SERVICE_STOP_MARKER_FILE).exists()
    }
}

/// The publisher is woken by the link service after a link or a preference change.
struct CatchUp(Mutex<Option<AgentAwareness>>);

impl AwarenessCatchUp for CatchUp {
    fn request_catch_up(&self) {
        if let Some(awareness) = self.0.lock().unwrap().as_ref() {
            awareness.request_catch_up();
        }
    }
}

/// Everything the T3 Connect services depend on. [`ConnectOptions::production`] is what `main.rs` uses; tests pass fakes for
/// the relay client, the connector and the control plane, so nothing leaves the machine.
pub struct ConnectOptions {
    pub store: Store,
    pub auth: AuthService,
    /// The environment descriptor (without the runtime capability).
    pub environment: Value,
    /// The T3 home: `runtime/` for the update markers, `tools/` for the managed relay client.
    pub base_dir: PathBuf,
    pub secrets_dir: PathBuf,
    pub relay_url: Option<String>,
    pub has_cloud_public_config: bool,
    /// The CLI's OAuth application (Clerk) and the hosted app that fronts its sign-in page.
    pub cli_oauth: CloudCliOAuthConfig,
    pub hosted_app_url: String,
    pub local_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    pub control_plane: Arc<dyn ControlPlane>,
    pub relay_client: Arc<dyn RelayClientResolver>,
    pub connector_spawner: Arc<dyn ConnectorSpawner>,
    pub clock: Arc<dyn Clock>,
    pub random: Arc<dyn Random>,
    /// Lets a relay be a plain `http://` loopback address. Reachable only from tests and `--fixture` (see
    /// [`ConnectOptions::fixture`]); a normal start leaves it off, so a relay URL must be a secure origin.
    pub allow_loopback_http_relay: bool,
}

/// The CLI's OAuth application when no Clerk values are configured. Its endpoints are empty, so any use of it fails at once
/// as a request with no address; nothing is sent. (The original fails such a use with the missing-configuration error.)
pub(crate) fn unconfigured_oauth() -> CloudCliOAuthConfig {
    CloudCliOAuthConfig {
        token_endpoint: String::new(),
        device_authorization_endpoint: String::new(),
        client_id: String::new(),
        loopback_port: t3_remote::config::CLOUD_CLI_OAUTH_LOOPBACK_PORT,
        redirect_uri: t3_remote::config::connect_loopback_redirect_uri(t3_remote::config::CLOUD_CLI_OAUTH_LOOPBACK_PORT),
        scopes: t3_remote::config::CONNECT_OAUTH_SCOPES,
    }
}

/// The T3 home for a state directory: the directory above `userdata`, or the state directory itself when it is not one.
pub fn base_dir_of(state_dir: &Path) -> PathBuf {
    match (state_dir.file_name().and_then(|name| name.to_str()), state_dir.parent()) {
        (Some("userdata"), Some(parent)) => parent.to_owned(),
        _ => state_dir.to_owned(),
    }
}

impl ConnectOptions {
    /// What a normal start uses: the real relay client resolver, connector process and HTTP control plane, configured from the
    /// environment (`T3CODE_RELAY_URL`, `T3CODE_CLOUDFLARED_PATH`). Without public configuration nothing is requested.
    pub fn production(store: Store, auth: AuthService, environment: Value, state_dir: &Path, local_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>) -> Self {
        let env = |name: &str| std::env::var(name).ok();
        let defaults = BuildDefaults::default();
        let base_dir = base_dir_of(state_dir);
        Self {
            store,
            auth,
            environment,
            relay_url: relay_url_config(&env, &defaults.relay_url).ok(),
            has_cloud_public_config: has_cloud_public_config(&env, &defaults),
            cli_oauth: cloud_cli_oauth_config(&env, &defaults).unwrap_or_else(|_| unconfigured_oauth()),
            hosted_app_url: hosted_app_url_config(&env).unwrap_or_else(|_| DEFAULT_HOSTED_APP_URL.to_owned()),
            local_port,
            control_plane: Arc::new(HttpControlPlane::new()),
            relay_client: Arc::new(Cloudflared::new(CloudflaredOptions {
                base_dir: base_dir.clone(),
                host: HostPlatform::current(),
                executable_override: env("T3CODE_CLOUDFLARED_PATH"),
                path_env: env("PATH"),
                release_asset: None,
            })),
            connector_spawner: Arc::new(ProcessConnectorSpawner),
            clock: Arc::new(SystemClock),
            random: Arc::new(ThreadRandom),
            allow_loopback_http_relay: false,
            secrets_dir: state_dir.join("secrets"),
            base_dir,
        }
    }
}

/// T3 Connect, built once the listener has an address.
pub struct Connect {
    pub services: RemoteServices,
    pub secrets: Arc<ConnectSecrets>,
    /// The CLI's stored sign-in (`cloud-cli-oauth-token`).
    pub tokens: Arc<CliTokenManager>,
    pub awareness: AgentAwareness,
    environment: Value,
    environment_id: String,
    clock: Arc<dyn Clock>,
    secrets_dir: PathBuf,
    sweep: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Connect {
    pub fn new(options: ConnectOptions) -> Result<Arc<Self>, SecretStoreError> {
        let secrets = Arc::new(ConnectSecrets(ServerSecretStore::open(&options.secrets_dir)?));
        let environment_id = options.environment["environmentId"].as_str().unwrap_or_default().to_owned();
        let catch_up = Arc::new(CatchUp(Mutex::new(None)));
        let tokens = Arc::new(CliTokenManager::new(secrets.clone(), options.clock.clone(), options.cli_oauth, options.hosted_app_url));
        let awareness = AgentAwareness::new(options.store.clone(), secrets.clone(), options.control_plane.clone(), options.clock.clone(), environment_id.clone());
        *catch_up.0.lock().unwrap() = Some(awareness.clone());
        let services = RemoteServices::new(RemoteConfig {
            secrets: secrets.clone(),
            environment: Arc::new(ServerEnvironmentIdentity { environment: options.environment.clone(), secrets: secrets.clone() }),
            pairing: Arc::new(ServerPairing { auth: options.auth }),
            awareness: catch_up,
            updates: Arc::new(Handoff { runtime_dir: options.base_dir.join("runtime"), clock: options.clock.clone() }),
            credentials: tokens.clone(),
            control_plane: options.control_plane,
            relay_client: options.relay_client,
            connector_spawner: options.connector_spawner,
            clock: options.clock.clone(),
            random: options.random,
            relay_url: options.relay_url,
            has_cloud_public_config: options.has_cloud_public_config,
            local_port: options.local_port,
            allow_loopback_http_relay: options.allow_loopback_http_relay,
        });
        Ok(Arc::new(Self {
            services,
            secrets,
            tokens,
            awareness,
            environment: options.environment,
            environment_id,
            clock: options.clock,
            secrets_dir: options.secrets_dir,
            sweep: Mutex::new(None),
        }))
    }

    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }

    /// The environment descriptor with the capabilities as they are now.
    pub fn descriptor(&self) -> Value {
        descriptor_of(&self.environment, self.secrets.as_ref())
    }

    /// The relay's signed proof of a webhook delivery: the delivery id and receive time it vouches for, or none when the request
    /// carries no proof this environment's link accepts (`RelayDeliveryProof.verify`). Anyone can call the webhook URL directly, so
    /// without a valid proof a request is a direct one.
    pub fn verify_delivery(&self, headers: &std::collections::BTreeMap<String, String>, hook_id: &str) -> Option<t3_remote::delivery_proof::VerifiedDelivery> {
        t3_remote::delivery_proof::verify_relay_delivery(self.secrets.as_ref(), &self.environment_id, self.clock.as_ref(), headers, hook_id)
    }

    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// Starts the background work once routes serve: the link reconciliation and tunnel, the agent-activity publisher, and the
    /// hourly sweep of replay markers (`replayMarkers.layer`).
    pub fn start(self: &Arc<Self>) {
        self.services.start();
        self.awareness.start();
        let directory = self.secrets_dir.clone();
        *self.sweep.lock().unwrap() = Some(tokio::spawn(async move {
            loop {
                match t3_remote::replay::prune_expired_replay_markers(&directory, SystemTime::now()) {
                    Ok(report) if report.failed > 0 => tracing::warn!(failed = report.failed, "Failed to prune some replay markers"),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "Failed to prune expired replay markers"),
                }
                tokio::time::sleep(t3_remote::replay::REPLAY_MARKER_SWEEP_INTERVAL).await;
            }
        }));
    }

    /// The server's finalizer: the publisher, the sweep, then the link services (which release a CLI-managed tunnel, within ten
    /// seconds, unless the server is going down for an update, and stop the connector).
    pub async fn shutdown(&self) {
        if let Some(sweep) = self.sweep.lock().unwrap().take() {
            sweep.abort();
        }
        self.awareness.shutdown().await;
        self.services.shutdown().await;
    }
}

/// Puts the runtime capability into a config document's environment (the snapshot is fixed at startup; this one is not).
pub fn overlay_config(store: &Store, config: &mut Value) {
    if let Some(connect) = current(store) {
        if let Some(capabilities) = config.get_mut("environment").and_then(|environment| environment.get_mut("capabilities")).and_then(Value::as_object_mut) {
            capabilities.insert("agentActivityPublishing".into(), Value::Bool(read_agent_activity_publishing_active(connect.secrets.as_ref())));
        }
    }
}

/// The server's one T3 Connect, found from the store its routes and RPCs are served over, so the routes need no field in the
/// server's state. Empty until [`install`]; a server that never installed one answers `404` on the connect routes.
#[derive(Default)]
struct Slot(Mutex<Option<Arc<Connect>>>);

pub fn install(store: &Store, connect: Arc<Connect>) {
    *store.extension(Slot::default).0.lock().unwrap() = Some(connect);
}

pub fn current(store: &Store) -> Option<Arc<Connect>> {
    store.extension(Slot::default).0.lock().unwrap().clone()
}

/// `server-runtime.json` (`serverRuntimeState.ts`): the origin the running server answers at, for a CLI on this machine.
pub fn persist_runtime_state(path: &Path, host: &str, port: u16) {
    let hostname = if host.is_empty() || host == "0.0.0.0" || host == "::" || host == "[::]" {
        "127.0.0.1".to_owned()
    } else if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let state = serde_json::json!({
        "version": 1,
        "pid": std::process::id(),
        "host": host,
        "port": port,
        "origin": format!("http://{hostname}:{port}"),
        "startedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    if let Err(error) = crate::server_secret_store::write_string_atomically(path, &format!("{state}\n")) {
        tracing::warn!(%error, "Failed to persist server runtime state");
    }
}

/// Tailscale Serve for this server (`--tailscale-serve`): configured once the server runs, and switched off again by the
/// finalizer only if this server switched it on. A failure to configure it is logged and the server runs without.
pub struct TailscaleServe {
    task: tokio::task::JoinHandle<Option<u16>>,
    tailscale: Arc<t3_remote::tailscale::Tailscale>,
}

impl TailscaleServe {
    pub fn activate(tailscale: Arc<t3_remote::tailscale::Tailscale>, local_port: u16, serve_port: u16) -> Self {
        let runner = tailscale.clone();
        let task = tokio::spawn(async move {
            match runner.ensure_serve(local_port, Some(serve_port), Some("127.0.0.1")).await {
                Ok(()) => {
                    tracing::info!(local_port, serve_port, "Tailscale Serve configured");
                    Some(serve_port)
                }
                Err(error) => {
                    tracing::warn!(%error, local_port, serve_port, "Failed to configure Tailscale Serve");
                    None
                }
            }
        });
        Self { task, tailscale }
    }

    pub async fn deactivate(self) {
        let Some(serve_port) = self.task.await.ok().flatten() else { return };
        match self.tailscale.disable_serve(Some(serve_port)).await {
            Ok(()) => tracing::info!(serve_port, "Tailscale Serve disabled"),
            Err(error) => tracing::warn!(%error, serve_port, "Failed to disable Tailscale Serve"),
        }
    }
}

/// The relay's hook URL prefix for this environment (`relayHookBaseUrl`): the relay finds the environment by its managed tunnel's
/// key (the tunnel name's last segment), so the URL never reveals the environment id. None without a managed tunnel.
pub fn relay_hook_base_url(store: &Store) -> Option<String> {
    let connect = current(store)?;
    let relay_url = String::from_utf8(connect.secrets.get(t3_remote::secrets::RELAY_URL_SECRET).ok()??).ok()?;
    let config = String::from_utf8(connect.secrets.get(t3_remote::secrets::CLOUD_ENDPOINT_RUNTIME_CONFIG).ok()??).ok()?;
    let tunnel_name = t3_remote::types::decode_runtime_config(&config).and_then(|config| config.tunnel_name).map(|name| name.as_str().to_owned());
    hook_base_url(&relay_url, tunnel_name.as_deref())
}

fn hook_base_url(relay_url: &str, tunnel_name: Option<&str>) -> Option<String> {
    let key = tunnel_name?.rsplit('-').next()?;
    let relay_url = relay_url.trim_end_matches('/');
    let is_key = key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (!relay_url.is_empty() && is_key).then(|| format!("{relay_url}/v1/hooks/{key}"))
}

#[cfg(test)]
mod tests {
    use super::hook_base_url;

    #[test]
    fn the_hook_url_names_the_tunnels_key_and_never_the_environment() {
        assert_eq!(hook_base_url("https://relay.test//", Some("t3-env-0123456789abcdef")).as_deref(), Some("https://relay.test/v1/hooks/0123456789abcdef"));
        assert_eq!(hook_base_url("https://relay.test", Some("t3-env-0123456789ABCDEF")), None);
        assert_eq!(hook_base_url("https://relay.test", Some("t3-env-short")), None);
        assert_eq!(hook_base_url("https://relay.test", None), None);
        assert_eq!(hook_base_url("", Some("0123456789abcdef")), None);
    }
}
