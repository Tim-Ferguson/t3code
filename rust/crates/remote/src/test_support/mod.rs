//! Shared test support: the one hang bound, the fixture reader, and fakes of everything outside
//! the crate (a relay on loopback, a connector process, the server's services).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use t3_contracts::ExecutionEnvironmentDescriptor;
use crate::cloud_link::*;
use crate::endpoint_runtime::*;
pub mod ssh_fakes;

use crate::relay_client::{
    InstallError, InstallProgressStage, RelayClientResolver, RelayClientStatus,
};

/// The only bound on any wait in these tests. A test must pass with any value of it: it is a
/// guard against hanging, never a pacing mechanism.
pub const HANG_BOUND: Duration = Duration::from_secs(60);

/// Awaits `future`, ending the test binary with a message instead of hanging. The watchdog runs on
/// an OS thread and real time, so it keeps its meaning when a test pauses tokio's clock (where a
/// tokio timeout would fire as soon as the test waits on a virtual timer).
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    let (done, waiting) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = waiting.recv_timeout(HANG_BOUND) {
            eprintln!("a test waited longer than the hang bound ({HANG_BOUND:?}); ending the run");
            std::process::exit(101);
        }
    });
    let value = future.await;
    drop(done);
    value
}

pub fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("t3code-rust-port-test-{name}-"))
        .tempdir()
        .expect("temp dir")
}

/// The rows of the oracle fixture (`rust/tools/generate_remote_fixtures.mjs`) of one kind.
pub fn fixture_rows(kind: &str) -> Vec<Value> {
    let text = include_str!("../../tests/fixtures/remote.txt");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture row"))
        .filter(|row| row["kind"] == kind)
        .collect()
}

pub fn descriptor(environment_id: &str) -> ExecutionEnvironmentDescriptor {
    serde_json::from_value(json!({
        "environmentId": environment_id,
        "label": "Fixture",
        "platform": {"os": "linux", "arch": "x64"},
        "serverVersion": "test",
        "orchestrationProtocolVersion": 2,
        "capabilities": {"repositoryIdentity": false, "connectionProbe": true}
    }))
    .expect("descriptor")
}

pub struct TestEnvironment(pub String);

impl EnvironmentIdentity for TestEnvironment {
    fn environment_id(&self) -> String {
        self.0.clone()
    }
    fn descriptor(&self) -> ExecutionEnvironmentDescriptor {
        descriptor(&self.0)
    }
}

/// Counts catch-up requests.
pub struct CountingAwareness {
    pub count: Mutex<u32>,
    changed: tokio::sync::watch::Sender<u32>,
}

impl Default for CountingAwareness {
    fn default() -> Self {
        Self {
            count: Mutex::new(0),
            changed: tokio::sync::watch::channel(0).0,
        }
    }
}

impl CountingAwareness {
    pub fn get(&self) -> u32 {
        *self.count.lock().unwrap()
    }

    /// Waits until at least `n` requests were made.
    pub async fn wait_for(&self, n: u32) {
        let mut rx = self.changed.subscribe();
        within(async {
            while *rx.borrow_and_update() < n {
                rx.changed().await.unwrap();
            }
        })
        .await;
    }
}

impl AwarenessCatchUp for CountingAwareness {
    fn request_catch_up(&self) {
        let mut count = self.count.lock().unwrap();
        *count += 1;
        self.changed.send_replace(*count);
    }
}

#[derive(Default)]
pub struct FakeUpdates {
    pub desktop: Mutex<bool>,
    pub pending: Mutex<bool>,
}

#[async_trait]
impl UpdateHandoff for FakeUpdates {
    async fn desktop_update_restart_pending(&self) -> bool {
        std::mem::take(&mut *self.desktop.lock().unwrap())
    }
    async fn pending_update_handoff_exists(&self) -> bool {
        *self.pending.lock().unwrap()
    }
}

/// Hands out a fixed credential and records what it was asked to issue.
#[derive(Default)]
pub struct FakePairing {
    pub requests: Mutex<Vec<String>>,
}

#[async_trait]
impl PairingIssuer for FakePairing {
    async fn create_pairing_link(&self, request: PairingRequest) -> Result<IssuedPairing, String> {
        assert_eq!(request.subject, "cloud-connect");
        assert_eq!(request.label, "T3 Connect connect");
        assert_eq!(request.ttl, Duration::from_secs(120));
        self.requests
            .lock()
            .unwrap()
            .push(request.proof_key_thumbprint.clone());
        Ok(IssuedPairing {
            credential: "pairing-credential".into(),
            expires_at_ms: 1_700_000_120_000,
        })
    }
}

pub struct FakeCredentials(pub Option<crate::cli_token::PersistedToken>);

#[async_trait]
impl CliCredentials for FakeCredentials {
    async fn get_existing(
        &self,
    ) -> Result<Option<crate::cli_token::PersistedToken>, crate::cli_token::CliTokenError>
    {
        Ok(self.0.clone())
    }
}

pub fn token(access: &str) -> crate::cli_token::PersistedToken {
    crate::cli_token::PersistedToken {
        access_token: access.into(),
        refresh_token: "refresh".into(),
        expires_at_epoch_ms: i64::MAX / 2,
        identity: Some("me@example.test".into()),
    }
}

/// A relay client resolver with a fixed answer.
pub struct FixedResolver(pub Mutex<RelayClientStatus>);

impl FixedResolver {
    pub fn available() -> Arc<Self> {
        Arc::new(Self(Mutex::new(RelayClientStatus::Available {
            executable_path: "/fake/cloudflared".into(),
            source: crate::relay_client::ExecutableSource::Override,
            version: "test".into(),
        })))
    }
    pub fn set(&self, status: RelayClientStatus) {
        *self.0.lock().unwrap() = status;
    }
}

#[async_trait]
impl RelayClientResolver for FixedResolver {
    async fn resolve(&self) -> RelayClientStatus {
        self.0.lock().unwrap().clone()
    }
    async fn install_with_progress(
        &self,
        report: &(dyn Fn(InstallProgressStage) + Send + Sync),
    ) -> Result<RelayClientStatus, InstallError> {
        report(InstallProgressStage::Checking);
        Ok(self.0.lock().unwrap().clone())
    }
}

/// What a fake connector was asked to do, and what the test can make it do.
pub struct FakeConnectorHandle {
    pub token: String,
    pub executable: String,
    pub lines: tokio::sync::mpsc::UnboundedSender<String>,
    exit: Mutex<Option<tokio::sync::oneshot::Sender<Option<i32>>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl FakeConnectorHandle {
    pub fn say(&self, line: &str) {
        let _ = self.lines.send(line.to_owned());
    }
    pub fn exit(&self, code: i32) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(tx) = self.exit.lock().unwrap().take() {
            let _ = tx.send(Some(code));
        }
    }
    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct FakeControl {
    running: Arc<std::sync::atomic::AtomicBool>,
    handle: Arc<FakeConnectorHandle>,
}

impl ConnectorControl for FakeControl {
    fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn kill(&self) {
        self.handle.exit(137);
    }
}

pub struct FakeSpawner {
    pub spawned: Mutex<Vec<Arc<FakeConnectorHandle>>>,
    pub spawn_times: Mutex<Vec<tokio::time::Instant>>,
    pub fail_with: Mutex<Option<String>>,
    next_pid: std::sync::atomic::AtomicU32,
    counter: tokio::sync::watch::Sender<usize>,
}

impl FakeSpawner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            spawned: Mutex::new(vec![]),
            spawn_times: Mutex::new(vec![]),
            fail_with: Mutex::new(None),
            next_pid: std::sync::atomic::AtomicU32::new(1000),
            counter: tokio::sync::watch::channel(0).0,
        })
    }

    /// Waits until at least `count` connectors have been started.
    pub async fn wait_for_spawns(&self, count: usize) {
        let mut rx = self.counter.subscribe();
        within(async {
            while *rx.borrow_and_update() < count {
                rx.changed().await.expect("spawner alive");
            }
        })
        .await;
    }

    /// Seconds between consecutive spawns, in tokio's (possibly virtual) time.
    pub fn spawn_gaps(&self) -> Vec<u64> {
        let times = self.spawn_times.lock().unwrap();
        times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect()
    }
    pub fn count(&self) -> usize {
        self.spawned.lock().unwrap().len()
    }
    pub fn last(&self) -> Arc<FakeConnectorHandle> {
        self.spawned
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a spawned connector")
    }
}

#[async_trait]
impl ConnectorSpawner for FakeSpawner {
    async fn spawn(
        &self,
        executable: &str,
        connector_token: &str,
    ) -> Result<SpawnedConnector, String> {
        if let Some(reason) = self.fail_with.lock().unwrap().clone() {
            return Err(reason);
        }
        let (lines, output) = tokio::sync::mpsc::unbounded_channel();
        let (exit_tx, exit) = tokio::sync::oneshot::channel();
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handle = Arc::new(FakeConnectorHandle {
            token: connector_token.to_owned(),
            executable: executable.to_owned(),
            lines,
            exit: Mutex::new(Some(exit_tx)),
            running: running.clone(),
        });
        self.spawned.lock().unwrap().push(handle.clone());
        self.spawn_times
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        self.counter.send_modify(|count| *count += 1);
        let pid = self
            .next_pid
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(SpawnedConnector {
            pid,
            output,
            exit,
            control: Arc::new(FakeControl { running, handle }),
        })
    }
}

// ---- a relay on loopback --------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub content_type: Option<String>,
    pub body: Value,
}

type Handler = dyn Fn(&RecordedRequest) -> (u16, Value, Vec<(String, String)>) + Send + Sync;

/// A relay answering on 127.0.0.1. Requests are recorded; `handler` decides each answer.
pub struct FakeRelay {
    pub url: String,
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl FakeRelay {
    pub async fn start(
        handler: impl Fn(&RecordedRequest) -> (u16, Value) + Send + Sync + 'static,
    ) -> Self {
        Self::start_with_headers(move |request| {
            let (status, body) = handler(request);
            (status, body, vec![])
        })
        .await
    }

    pub async fn start_with_headers(
        handler: impl Fn(&RecordedRequest) -> (u16, Value, Vec<(String, String)>)
        + Send
        + Sync
        + 'static,
    ) -> Self {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{HeaderMap, Method, StatusCode, Uri};
        use axum::response::IntoResponse;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state: (Arc<Mutex<Vec<RecordedRequest>>>, Arc<Handler>) =
            (requests.clone(), Arc::new(handler));
        let app = axum::Router::new()
            .fallback(
                |State((requests, handler)): State<(
                    Arc<Mutex<Vec<RecordedRequest>>>,
                    Arc<Handler>,
                )>,
                 method: Method,
                 uri: Uri,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    let recorded = RecordedRequest {
                        method: method.to_string(),
                        path: uri.path().to_owned(),
                        authorization: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        content_type: headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    };
                    requests.lock().unwrap().push(recorded.clone());
                    let (status, body, extra) = handler(&recorded);
                    let mut response =
                        (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
                    for (name, value) in extra {
                        response.headers_mut().insert(
                            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                            axum::http::HeaderValue::from_str(&value).unwrap(),
                        );
                    }
                    response
                },
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            url,
            requests,
            shutdown: Some(shutdown),
        }
    }

    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn recorded_for(&self, method: &str, path: &str) -> Vec<RecordedRequest> {
        self.recorded()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl FakeRelay {
    /// A form-encoded endpoint (OAuth): `handler` gets the request path and the decoded fields, and
    /// the raw body is recorded as `{"_raw": ...}`.
    pub async fn start_raw_form(
        handler: impl Fn(&str, &BTreeMap<String, String>) -> (u16, Value) + Send + Sync + 'static,
    ) -> Self {
        Self::start_raw(move |request| {
            let raw = request.body["_raw"].as_str().unwrap_or("").to_owned();
            let fields = url::form_urlencoded::parse(raw.as_bytes())
                .into_owned()
                .collect();
            handler(&request.path, &fields)
        })
        .await
    }

    async fn start_raw(
        handler: impl Fn(&RecordedRequest) -> (u16, Value) + Send + Sync + 'static,
    ) -> Self {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{HeaderMap, Method, StatusCode, Uri};
        use axum::response::IntoResponse;
        type Shared = (
            Arc<Mutex<Vec<RecordedRequest>>>,
            Arc<dyn Fn(&RecordedRequest) -> (u16, Value) + Send + Sync>,
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state: Shared = (requests.clone(), Arc::new(handler));
        let app = axum::Router::new()
            .fallback(
                |State((requests, handler)): State<Shared>,
                 method: Method,
                 uri: Uri,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    let recorded = RecordedRequest {
                        method: method.to_string(),
                        path: uri.path().to_owned(),
                        authorization: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        content_type: headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        body: json!({"_raw": String::from_utf8_lossy(&body)}),
                    };
                    requests.lock().unwrap().push(recorded.clone());
                    let (status, body) = handler(&recorded);
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body.to_string(),
                    )
                        .into_response()
                },
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            url,
            requests,
            shutdown: Some(shutdown),
        }
    }
}

impl FakeRelay {
    /// Serves the same bytes for every GET (a release download).
    pub async fn start_bytes(body: &'static [u8]) -> Self {
        use axum::extract::State;
        use axum::http::{Method, Uri};
        type Shared = Arc<Mutex<Vec<RecordedRequest>>>;
        let requests: Shared = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .fallback(
                move |State(requests): State<Shared>, method: Method, uri: Uri| async move {
                    requests.lock().unwrap().push(RecordedRequest {
                        method: method.to_string(),
                        path: uri.path().to_owned(),
                        authorization: None,
                        content_type: None,
                        body: Value::Null,
                    });
                    body
                },
            )
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            url,
            requests,
            shutdown: Some(shutdown),
        }
    }
}

pub fn header_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

// ---- clock, scripted control plane, harness ------------------------------------------------

use crate::backoff::{FixedRandom, Random};
use crate::clock::Clock;
use crate::control_plane::{ControlPlane, HttpControlPlane, RelayResult};
use crate::jwt::KeyPair;
use crate::secrets::MemorySecretStore;
use crate::types::*;

pub const NOW_MS: i64 = 1_700_000_000_000;

/// A settable clock.
pub struct FixedClock(pub std::sync::atomic::AtomicI64);

impl FixedClock {
    pub fn at(ms: i64) -> Arc<Self> {
        Arc::new(Self(std::sync::atomic::AtomicI64::new(ms)))
    }
    pub fn advance_ms(&self, ms: i64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub struct Harness {
    pub secrets: Arc<MemorySecretStore>,
    pub spawner: Arc<FakeSpawner>,
    pub resolver: Arc<FixedResolver>,
    pub runtime: ManagedEndpointRuntime,
    pub link: CloudLink,
    pub clock: Arc<FixedClock>,
    pub awareness: Arc<CountingAwareness>,
    pub updates: Arc<FakeUpdates>,
    pub pairing: Arc<FakePairing>,
    pub environment_id: String,
}

pub struct HarnessOptions {
    pub secrets: Vec<(&'static str, String)>,
    pub token: Option<crate::cli_token::PersistedToken>,
    pub relay_url: Option<String>,
    pub control_plane: Arc<dyn ControlPlane>,
    pub port: Option<u16>,
}

impl HarnessOptions {
    pub fn new(control_plane: Arc<dyn ControlPlane>) -> Self {
        Self {
            secrets: vec![],
            token: Some(token("cli-access")),
            relay_url: None,
            control_plane,
            port: Some(3773),
        }
    }
    pub fn secret(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.secrets.push((name, value.into()));
        self
    }
}

impl Harness {
    pub fn new(options: HarnessOptions) -> Self {
        let secrets = Arc::new(MemorySecretStore::new());
        for (name, value) in &options.secrets {
            use crate::secrets::SecretStore;
            secrets.set(name, value.as_bytes()).unwrap();
        }
        let spawner = FakeSpawner::new();
        let resolver = FixedResolver::available();
        let runtime = ManagedEndpointRuntime::new(resolver.clone(), spawner.clone());
        let clock = FixedClock::at(NOW_MS);
        let awareness = Arc::new(CountingAwareness::default());
        let updates = Arc::new(FakeUpdates::default());
        let pairing = Arc::new(FakePairing::default());
        let environment_id = "env_123".to_owned();
        let port = options.port;
        let link = CloudLink::new(CloudLinkDeps {
            secrets: secrets.clone(),
            environment: Arc::new(TestEnvironment(environment_id.clone())),
            runtime: runtime.clone(),
            pairing: pairing.clone(),
            credentials: Arc::new(FakeCredentials(options.token)),
            control_plane: options.control_plane,
            awareness: awareness.clone(),
            updates: updates.clone(),
            clock: clock.clone(),
            random: Arc::new(FixedRandom(0.5)) as Arc<dyn Random>,
            relay_url: options.relay_url,
            local_port: Arc::new(move || port),
            allow_loopback_http_relay: true,
        });
        Self {
            secrets,
            spawner,
            resolver,
            runtime,
            link,
            clock,
            awareness,
            updates,
            pairing,
            environment_id,
        }
    }
}

pub fn http_control_plane() -> Arc<dyn ControlPlane> {
    Arc::new(HttpControlPlane::new())
}

/// The relay's mint key pair and a stored-link secret set that trusts it.
pub fn linked_secrets(relay_url: &str, relay_keys: &KeyPair) -> Vec<(&'static str, String)> {
    use crate::secrets::*;
    vec![
        (CLOUD_LINKED_USER_ID, "user-123".into()),
        (RELAY_URL_SECRET, relay_url.into()),
        (RELAY_ISSUER_SECRET, relay_url.into()),
        (
            RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
            "environment-credential".into(),
        ),
        (CLOUD_MINT_PUBLIC_KEY, relay_keys.public_key.clone()),
    ]
}

pub fn config_json(token: &str, tunnel_id: &str) -> String {
    format!(
        r#"{{"providerKind":"cloudflare_tunnel","connectorToken":"{token}","tunnelId":"{tunnel_id}"}}"#
    )
}

/// An in-process control plane: each call pops the next scripted answer (or the default).
pub struct ScriptedControlPlane {
    pub calls: Mutex<Vec<String>>,
    changed: tokio::sync::watch::Sender<usize>,
    pub register: Mutex<std::collections::VecDeque<RelayResult<RecoveryRegistrationResponse>>>,
    pub recover: Mutex<std::collections::VecDeque<RelayResult<RecoveryResponse>>>,
    pub challenge: Mutex<std::collections::VecDeque<RelayResult<LinkChallengeResponse>>>,
    pub link: Mutex<std::collections::VecDeque<RelayResult<LinkResponse>>>,
    pub release: Mutex<std::collections::VecDeque<RelayResult<OkResponse>>>,
    pub wake: Mutex<std::collections::VecDeque<RelayResult<WakeHeldHooksResponse>>>,
    pub preferences: Mutex<std::collections::VecDeque<RelayResult<LinkPreferencesRequest>>>,
}

impl ScriptedControlPlane {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(vec![]),
            changed: tokio::sync::watch::channel(0).0,
            register: Default::default(),
            recover: Default::default(),
            challenge: Default::default(),
            link: Default::default(),
            release: Default::default(),
            wake: Default::default(),
            preferences: Default::default(),
        })
    }

    fn record(&self, name: impl Into<String>) {
        let mut calls = self.calls.lock().unwrap();
        calls.push(name.into());
        self.changed.send_replace(calls.len());
    }

    /// Waits until at least `n` calls named `name` were made.
    pub async fn wait_for(&self, name: &str, n: usize) {
        let mut rx = self.changed.subscribe();
        within(async {
            loop {
                if self.count(name) >= n {
                    return;
                }
                rx.changed().await.unwrap();
                let _ = rx.borrow_and_update();
            }
        })
        .await;
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    pub fn count(&self, name: &str) -> usize {
        self.calls().iter().filter(|c| c.as_str() == name).count()
    }
    fn pop<T: Clone>(
        &self,
        queue: &Mutex<std::collections::VecDeque<RelayResult<T>>>,
        name: &str,
        default: RelayResult<T>,
    ) -> RelayResult<T> {
        self.record(name);
        let mut queue = queue.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue.front().cloned().unwrap_or(default)
        }
    }
}

pub fn unavailable() -> crate::relay_error::RelayRequestError {
    crate::relay_error::RelayRequestError::transport("TransportError")
}

pub fn forbidden() -> crate::relay_error::RelayRequestError {
    crate::relay_error::filter_relay_response(403, None, b"").unwrap_err()
}

#[async_trait]
impl ControlPlane for ScriptedControlPlane {
    async fn create_link_challenge(
        &self,
        _: &str,
        _: &str,
        _: &LinkChallengeRequest,
    ) -> RelayResult<LinkChallengeResponse> {
        self.pop(&self.challenge, "challenge", Err(unavailable()))
    }
    async fn link_environment(
        &self,
        _: &str,
        _: &str,
        _: &LinkRequest,
    ) -> RelayResult<LinkResponse> {
        self.pop(&self.link, "link", Err(unavailable()))
    }
    async fn unlink_environment(&self, _: &str, _: &str, _: &str) -> RelayResult<OkResponse> {
        self.record("unlink");
        Ok(OkResponse { ok: true })
    }
    async fn release_environment_tunnel(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> RelayResult<OkResponse> {
        self.pop(&self.release, "release", Ok(OkResponse { ok: true }))
    }
    async fn register_tunnel_recovery(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &RecoveryRegistrationRequest,
    ) -> RelayResult<RecoveryRegistrationResponse> {
        self.pop(
            &self.register,
            "register",
            Ok(RecoveryRegistrationResponse {
                status: RegistrationStatus::Ready,
            }),
        )
    }
    async fn recover_tunnel(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &RecoveryRequest,
    ) -> RelayResult<RecoveryResponse> {
        self.pop(&self.recover, "recover", Err(unavailable()))
    }
    async fn update_link_preferences(
        &self,
        _: &str,
        _: &str,
        _: &str,
        request: &LinkPreferencesRequest,
    ) -> RelayResult<LinkPreferencesRequest> {
        self.record(format!(
            "preferences:{}",
            request.hold_webhooks_while_offline
        ));
        let mut queue = self.preferences.lock().unwrap();
        if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue.front().cloned().unwrap_or(Ok(request.clone()))
        }
    }
    async fn wake_held_hooks(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> RelayResult<WakeHeldHooksResponse> {
        self.pop(
            &self.wake,
            "wake",
            Ok(WakeHeldHooksResponse { pending: false }),
        )
    }
    async fn publish_agent_activity(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &AgentActivityPublishRequest,
    ) -> RelayResult<PublishResponse> {
        self.record("publish");
        Ok(PublishResponse {
            ok: true,
            deliveries: vec![],
        })
    }
}

/// Writes an executable script at an absolute path in `dir`.
#[cfg(unix)]
pub fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
