//! Runs the managed tunnel's connector (`cloudflared`) for the stored runtime config
//! (`cloud/ManagedEndpointRuntime.ts`): starts and stops it, restarts it when it exits (with a
//! crash-loop backoff), reads its output to learn when the tunnel is connected or was rejected,
//! and asks for a replacement tunnel ("recovery") when it never connects.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::relay_client::{RelayClientResolver, RelayClientStatus};
use crate::types::{ProviderKind, RuntimeConfig};

/// A connector that exits before running this long is part of a crash loop; one that stays up at
/// least this long earns an immediate restart again.
pub const RELAY_RESTART_STABLE_UPTIME: Duration = Duration::from_secs(30);
const RELAY_RESTART_BACKOFF_BASE_MS: u64 = 1_000;
const RELAY_RESTART_BACKOFF_MAX_MS: u64 = 60_000;
/// Newly created tunnels can fail authorization briefly while Cloudflare propagates their token.
pub const TUNNEL_AUTHORIZATION_FAILURES_BEFORE_RECOVERY: u32 = 4;
/// A connector that never registers a connection may hold a token for a tunnel that no longer
/// exists. Recovery is requested after this long, and again at this interval while it stays
/// unconnected.
pub const CONNECTOR_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(3 * 60);

/// `CloudManagedEndpointRuntimeStatus`. Serialises exactly as the original's JSON; this is the
/// `endpointRuntimeStatus` clients see in `/api/connect/*` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum EndpointStatus {
    Disabled,
    #[serde(rename_all = "camelCase")]
    Failed {
        provider_kind: ProviderKind,
        failure: FailureKind,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tunnel_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tunnel_name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Running {
        provider_kind: ProviderKind,
        pid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        tunnel_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tunnel_name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Unsupported {
        provider_kind: ProviderKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FailureKind {
    #[serde(rename = "unsupported-platform")]
    UnsupportedPlatform,
    #[serde(rename = "not-installed")]
    NotInstalled,
    #[serde(rename = "spawn-failed")]
    SpawnFailed,
}

impl EndpointStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("status json")
    }

    /// Connector startup failures can clear after installation or a later spawn attempt;
    /// an unsupported runtime or platform never will (`isRetryableManagedEndpointRuntimeStatus`).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                failure: FailureKind::NotInstalled | FailureKind::SpawnFailed,
                ..
            }
        )
    }
}

/// A snapshot for the status stream: the runtime's status plus whether the tunnel is carrying
/// traffic and how often the connector restarted since the config was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSnapshot {
    pub status: EndpointStatus,
    /// The connector reported "Registered tunnel connection" since it last started.
    pub connected: bool,
    pub restarts: u32,
}

/// `isRetryableManagedEndpointRuntimeStatus` for a status as untyped JSON.
pub fn is_retryable_status_json(status: &serde_json::Value) -> bool {
    status.get("status").and_then(|s| s.as_str()) == Some("failed")
        && matches!(
            status.get("failure").and_then(|s| s.as_str()),
            Some("not-installed" | "spawn-failed")
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputClass {
    Connected,
    Warning,
    Debug,
}

/// How a line of connector output reads (`classifyRelayClientOutput`).
pub fn classify_relay_client_output(line: &str) -> OutputClass {
    let lower = line.to_lowercase();
    if lower.contains("registered tunnel connection") {
        return OutputClass::Connected;
    }
    // zerolog level tokens; FTL and PNC are more severe than ERR and must surface at least as loudly.
    let has_level = line
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| matches!(word, "ERR" | "WRN" | "FTL" | "PNC"));
    if has_level {
        OutputClass::Warning
    } else {
        OutputClass::Debug
    }
}

/// Cloudflare's edge rejects a connector whose tunnel was deleted or whose token no longer
/// matches, in wording that has changed over time (`isRejectedRelayClientTunnelOutput`).
pub fn is_rejected_relay_client_tunnel_output(line: &str) -> bool {
    static REJECTION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let rejection = REJECTION.get_or_init(|| {
        regex::RegexBuilder::new(
            r#"error="(?:Unauthorized:[^"]*|Failed to get tunnel|Tunnel not found|Record for tunnel not found|Invalid tunnel secret)""#,
        )
        .case_insensitive(true)
        .build()
        .expect("rejection pattern")
    });
    line.to_lowercase()
        .contains("register tunnel error from server side")
        && rejection.is_match(line)
}

/// A queue holding at most one item: a new offer replaces one nobody took yet
/// (`Queue.sliding(1)`).
pub struct SlidingQueue<T> {
    slot: Mutex<Option<T>>,
    notify: Notify,
}

impl<T> Default for SlidingQueue<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            notify: Notify::new(),
        }
    }
}

impl<T> SlidingQueue<T> {
    pub fn offer(&self, item: T) {
        *self.slot.lock().expect("queue") = Some(item);
        self.notify.notify_one();
    }

    pub async fn take(&self) -> T {
        loop {
            if let Some(item) = self.slot.lock().expect("queue").take() {
                return item;
            }
            self.notify.notified().await;
        }
    }
}

/// A started connector process.
pub struct SpawnedConnector {
    pub pid: u32,
    /// Lines of stdout and stderr, merged. Closes when the process's output ends.
    pub output: mpsc::UnboundedReceiver<String>,
    /// Resolves with the exit code (None when unknown) once the process has exited.
    pub exit: oneshot::Receiver<Option<i32>>,
    pub control: Arc<dyn ConnectorControl>,
}

pub trait ConnectorControl: Send + Sync {
    fn is_running(&self) -> bool;
    /// Asks the process to stop. Idempotent.
    fn kill(&self);
}

#[async_trait]
pub trait ConnectorSpawner: Send + Sync {
    /// Starts `executable tunnel --no-autoupdate --loglevel info --output default run` with the
    /// token in `TUNNEL_TOKEN`. Err carries the reason text.
    async fn spawn(
        &self,
        executable: &str,
        connector_token: &str,
    ) -> Result<SpawnedConnector, String>;
}

/// The arguments the original passes to `cloudflared`.
pub const CONNECTOR_ARGS: [&str; 7] = [
    "tunnel",
    "--no-autoupdate",
    "--loglevel",
    "info",
    "--output",
    "default",
    "run",
];

/// Spawns the real executable with tokio.
pub struct ProcessConnectorSpawner;

struct ProcessControl {
    running: Arc<AtomicBool>,
    kill: Arc<Notify>,
}

impl ConnectorControl for ProcessControl {
    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn kill(&self) {
        self.kill.notify_one();
    }
}

#[async_trait]
impl ConnectorSpawner for ProcessConnectorSpawner {
    async fn spawn(
        &self,
        executable: &str,
        connector_token: &str,
    ) -> Result<SpawnedConnector, String> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = tokio::process::Command::new(executable)
            .args(CONNECTOR_ARGS)
            .env("TUNNEL_TOKEN", connector_token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| error.to_string())?;
        let pid = child.id().unwrap_or(0);
        let (line_tx, output) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = line_tx;
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        let running = Arc::new(AtomicBool::new(true));
        let kill = Arc::new(Notify::new());
        let (exit_tx, exit) = oneshot::channel();
        {
            let running = running.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let code = tokio::select! {
                    status = child.wait() => status.ok().and_then(|s| s.code()),
                    () = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await.ok().and_then(|s| s.code())
                    }
                };
                running.store(false, Ordering::SeqCst);
                let _ = exit_tx.send(code);
            });
        }
        Ok(SpawnedConnector {
            pid,
            output,
            exit,
            control: Arc::new(ProcessControl { running, kill }),
        })
    }
}

struct ActiveConnector {
    id: u64,
    pid: u32,
    control: Arc<dyn ConnectorControl>,
    config_key: String,
    config: RuntimeConfig,
    started: Instant,
    connected: Arc<AtomicBool>,
    /// The output reader and the registration watch; the supervisor stops itself.
    side_tasks: Vec<JoinHandle<()>>,
    supervisor: Option<JoinHandle<()>>,
}

fn runtime_config_key(config: &RuntimeConfig) -> String {
    serde_json::json!({
        "providerKind": config.provider_kind,
        "connectorToken": config.connector_token.as_str(),
        "tunnelId": config.tunnel_id.as_ref().map(|v| v.as_str()),
        "tunnelName": config.tunnel_name.as_ref().map(|v| v.as_str()),
    })
    .to_string()
}

struct Inner {
    resolver: Arc<dyn RelayClientResolver>,
    spawner: Arc<dyn ConnectorSpawner>,
    active: Mutex<Option<ActiveConnector>>,
    desired: Mutex<Option<RuntimeConfig>>,
    restart_delay_ms: Mutex<u64>,
    next_id: AtomicU64,
    restarts: Mutex<u32>,
    reconcile: tokio::sync::Mutex<()>,
    link_state: tokio::sync::Mutex<()>,
    recovery: SlidingQueue<RuntimeConfig>,
    tunnel_connections: SlidingQueue<()>,
    snapshot: watch::Sender<EndpointSnapshot>,
}

/// The managed tunnel's runtime. Cheap to clone; clones share one connector.
#[derive(Clone)]
pub struct ManagedEndpointRuntime {
    inner: Arc<Inner>,
}

impl ManagedEndpointRuntime {
    pub fn new(resolver: Arc<dyn RelayClientResolver>, spawner: Arc<dyn ConnectorSpawner>) -> Self {
        let (snapshot, _) = watch::channel(EndpointSnapshot {
            status: EndpointStatus::Disabled,
            connected: false,
            restarts: 0,
        });
        Self {
            inner: Arc::new(Inner {
                resolver,
                spawner,
                active: Mutex::new(None),
                desired: Mutex::new(None),
                restart_delay_ms: Mutex::new(0),
                next_id: AtomicU64::new(1),
                restarts: Mutex::new(0),
                reconcile: tokio::sync::Mutex::new(()),
                link_state: tokio::sync::Mutex::new(()),
                recovery: SlidingQueue::default(),
                tunnel_connections: SlidingQueue::default(),
                snapshot,
            }),
        }
    }

    /// The status stream: the latest snapshot, and every change after it.
    pub fn subscribe(&self) -> watch::Receiver<EndpointSnapshot> {
        self.inner.snapshot.subscribe()
    }

    pub fn current(&self) -> EndpointSnapshot {
        self.inner.snapshot.borrow().clone()
    }

    /// Serialises updates to the persisted link state across startup, the connect routes and
    /// shutdown. Not re-entrant: do not call it from inside `f` (`withLinkStateLock`).
    pub async fn with_link_state_lock<T>(&self, f: impl Future<Output = T>) -> T {
        let _guard = self.inner.link_state.lock().await;
        f.await
    }

    /// Takes the next recovery request: the connector exited, was rejected, or never connected.
    /// At most one is kept; a newer one replaces it.
    pub async fn next_recovery_request(&self) -> RuntimeConfig {
        self.inner.recovery.take().await
    }

    pub fn request_recovery(&self, config: RuntimeConfig) {
        self.inner.recovery.offer(config);
    }

    /// Waits for the next time the connector registered a tunnel connection, i.e. the relay can
    /// reach us again. Registrations that happen while nobody waits collapse into one.
    pub async fn next_tunnel_connection(&self) {
        self.inner.tunnel_connections.take().await;
    }

    fn publish(&self, status: EndpointStatus) {
        let connected = self
            .inner
            .active
            .lock()
            .expect("active")
            .as_ref()
            .is_some_and(|a| a.connected.load(Ordering::SeqCst));
        let restarts = *self.inner.restarts.lock().expect("restarts");
        self.inner.snapshot.send_replace(EndpointSnapshot {
            status,
            connected,
            restarts,
        });
    }

    fn publish_connected(&self) {
        let mut next = self.current();
        next.connected = true;
        self.inner.snapshot.send_replace(next);
    }

    /// Starts, keeps, replaces or stops the connector so it matches `config`; `None` stops it.
    pub async fn apply_config(&self, config: Option<&RuntimeConfig>) -> EndpointStatus {
        let guard = self.inner.reconcile.lock().await;
        {
            // A real config change starts over with a fresh backoff. Recovery that hands back the
            // same tunnel and token must keep the delay, or a crash-looping connector respawns
            // on every recovery round trip.
            let mut desired = self.inner.desired.lock().expect("desired");
            let unchanged = match (&*desired, config) {
                (Some(current), Some(next)) => {
                    runtime_config_key(current) == runtime_config_key(next)
                }
                _ => false,
            };
            if !unchanged {
                *self.inner.restart_delay_ms.lock().expect("delay") = 0;
            }
            *desired = config.cloned();
        }
        let status = self.reconcile_locked(config, &guard).await;
        self.publish(status.clone());
        status
    }

    fn stop_active(&self, from_supervisor: bool) {
        let active = self.inner.active.lock().expect("active").take();
        if let Some(active) = active {
            Self::stop_connector(active, from_supervisor);
        }
    }

    fn stop_connector(mut active: ActiveConnector, from_supervisor: bool) {
        for task in active.side_tasks.drain(..) {
            task.abort();
        }
        if !from_supervisor {
            if let Some(supervisor) = active.supervisor.take() {
                supervisor.abort();
            }
        }
        active.control.kill();
        tracing::info!(pid = active.pid, "Relay client stopped");
    }

    async fn reconcile_locked(
        &self,
        config: Option<&RuntimeConfig>,
        _held: &tokio::sync::MutexGuard<'_, ()>,
    ) -> EndpointStatus {
        let Some(config) = config.filter(|c| c.provider_kind == ProviderKind::CloudflareTunnel)
        else {
            self.stop_active(false);
            return match config {
                Some(config) => EndpointStatus::Unsupported {
                    provider_kind: config.provider_kind,
                },
                None => EndpointStatus::Disabled,
            };
        };
        let tunnel_id = config.tunnel_id.as_ref().map(|v| v.as_str().to_owned());
        let tunnel_name = config.tunnel_name.as_ref().map(|v| v.as_str().to_owned());
        let key = runtime_config_key(config);
        {
            let active = self.inner.active.lock().expect("active");
            if let Some(active) = active.as_ref().filter(|a| a.config_key == key) {
                if active.control.is_running() {
                    return EndpointStatus::Running {
                        provider_kind: ProviderKind::CloudflareTunnel,
                        pid: active.pid,
                        tunnel_id: active
                            .config
                            .tunnel_id
                            .as_ref()
                            .map(|v| v.as_str().to_owned()),
                        tunnel_name: active
                            .config
                            .tunnel_name
                            .as_ref()
                            .map(|v| v.as_str().to_owned()),
                    };
                }
            }
        }
        self.stop_active(false);
        let executable = match self.inner.resolver.resolve().await {
            RelayClientStatus::Available {
                executable_path, ..
            } => executable_path,
            other => {
                let unsupported = matches!(other, RelayClientStatus::Unsupported { .. });
                let reason = match &other {
                    RelayClientStatus::Unsupported { platform, arch, .. } => {
                        format!("Relay client is unsupported on {platform}-{arch}.")
                    }
                    _ => "The relay client is not installed.".to_owned(),
                };
                return EndpointStatus::Failed {
                    provider_kind: ProviderKind::CloudflareTunnel,
                    failure: if unsupported {
                        FailureKind::UnsupportedPlatform
                    } else {
                        FailureKind::NotInstalled
                    },
                    reason,
                    tunnel_id,
                    tunnel_name,
                };
            }
        };
        let spawned = match self
            .inner
            .spawner
            .spawn(&executable, config.connector_token.as_str())
            .await
        {
            Ok(spawned) => spawned,
            Err(reason) => {
                tracing::warn!(tunnel_id = ?tunnel_id, "Failed to start relay client");
                return EndpointStatus::Failed {
                    provider_kind: ProviderKind::CloudflareTunnel,
                    failure: FailureKind::SpawnFailed,
                    reason,
                    tunnel_id,
                    tunnel_name,
                };
            }
        };
        tracing::info!(
            pid = spawned.pid,
            "Relay client process started; waiting for tunnel connection"
        );
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let connected = Arc::new(AtomicBool::new(false));
        let observe = tokio::spawn(self.clone().observe_output(
            id,
            config.clone(),
            spawned.output,
            connected.clone(),
        ));
        let watch = tokio::spawn(
            self.clone()
                .watch_registration(config.clone(), connected.clone()),
        );
        let supervise = tokio::spawn(self.clone().supervise(id, key.clone(), spawned.exit));
        let status = EndpointStatus::Running {
            provider_kind: ProviderKind::CloudflareTunnel,
            pid: spawned.pid,
            tunnel_id,
            tunnel_name,
        };
        *self.inner.active.lock().expect("active") = Some(ActiveConnector {
            id,
            pid: spawned.pid,
            control: spawned.control,
            config_key: key,
            config: config.clone(),
            started: Instant::now(),
            connected,
            side_tasks: vec![observe, watch],
            supervisor: Some(supervise),
        });
        status
    }

    fn is_active(&self, id: u64, key: &str) -> bool {
        self.inner
            .active
            .lock()
            .expect("active")
            .as_ref()
            .is_some_and(|a| a.id == id && a.config_key == key)
    }

    /// Restarts the connector after it exits. The first crash restarts immediately; each further
    /// crash inside the stable-uptime window doubles the wait, up to the cap. The delay runs before
    /// the reconcile lock so a config change is never blocked behind it.
    fn supervise(
        self,
        id: u64,
        key: String,
        exit: oneshot::Receiver<Option<i32>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        // Boxed: the supervisor reconciles, and reconciling starts a supervisor.
        Box::pin(self.supervise_inner(id, key, exit))
    }

    async fn supervise_inner(self, id: u64, key: String, exit: oneshot::Receiver<Option<i32>>) {
        let exit_code = exit.await.ok().flatten();
        let uptime = {
            let active = self.inner.active.lock().expect("active");
            match active.as_ref() {
                Some(a) if a.id == id && a.config_key == key => a.started.elapsed(),
                _ => return,
            }
        };
        let delay_ms = {
            let mut current = self.inner.restart_delay_ms.lock().expect("delay");
            if uptime >= RELAY_RESTART_STABLE_UPTIME {
                *current = 0;
                0
            } else {
                let this = *current;
                *current = if this == 0 {
                    RELAY_RESTART_BACKOFF_BASE_MS
                } else {
                    (this * 2).min(RELAY_RESTART_BACKOFF_MAX_MS)
                };
                this
            }
        };
        if delay_ms > 0 {
            tracing::warn!(delay_ms, "Relay client is crash-looping; delaying restart");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let guard = self.inner.reconcile.lock().await;
        if !self.is_active(id, &key) {
            return;
        }
        let previous = self.inner.active.lock().expect("active").take();
        if let Some(previous) = previous {
            Self::stop_connector(previous, true);
        }
        let desired = self.inner.desired.lock().expect("desired").clone();
        let Some(desired) = desired.filter(|d| {
            d.provider_kind == ProviderKind::CloudflareTunnel && runtime_config_key(d) == key
        }) else {
            return;
        };
        tracing::warn!(?exit_code, "Relay client exited; restarting");
        self.inner.recovery.offer(desired.clone());
        *self.inner.restarts.lock().expect("restarts") += 1;
        let status = self.reconcile_locked(Some(&desired), &guard).await;
        self.publish(status);
    }

    /// Requests recovery while the connector has not registered a connection, once per timeout,
    /// until it connects or is replaced.
    async fn watch_registration(self, config: RuntimeConfig, connected: Arc<AtomicBool>) {
        loop {
            tokio::time::sleep(CONNECTOR_REGISTRATION_TIMEOUT).await;
            if connected.load(Ordering::SeqCst) {
                return;
            }
            tracing::warn!(
                "Relay client has not registered a tunnel connection; requesting recovery"
            );
            self.inner.recovery.offer(config.clone());
        }
    }

    async fn observe_output(
        self,
        _id: u64,
        config: RuntimeConfig,
        mut output: mpsc::UnboundedReceiver<String>,
        connected: Arc<AtomicBool>,
    ) {
        let mut rejected_registrations = 0;
        while let Some(raw) = output.recv().await {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match classify_relay_client_output(line) {
                OutputClass::Connected => {
                    rejected_registrations = 0;
                    connected.store(true, Ordering::SeqCst);
                    tracing::info!("Relay client tunnel connection registered");
                    self.publish_connected();
                    self.inner.tunnel_connections.offer(());
                }
                OutputClass::Warning => {
                    if is_rejected_relay_client_tunnel_output(line) {
                        rejected_registrations += 1;
                        if rejected_registrations >= TUNNEL_AUTHORIZATION_FAILURES_BEFORE_RECOVERY {
                            rejected_registrations = 0;
                            tracing::warn!("Relay client tunnel was rejected; requesting recovery");
                            self.inner.recovery.offer(config.clone());
                            continue;
                        }
                    }
                    tracing::warn!("Relay client reported a transport warning");
                }
                OutputClass::Debug => tracing::debug!("Relay client output"),
            }
        }
    }
}

/// Log lines with the connector token removed (`replaceAll(token, "<redacted>")`).
pub fn redact_connector_token(line: &str, token: &str) -> String {
    line.replace(token, "<redacted>")
}
