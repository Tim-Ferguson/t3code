//! A server with T3 Connect wired in as `main.rs` wires it: the store, the authentication service, `Connect` over the real secret
//! store of a temporary state directory, the router, and `Connect::start` once the listener serves. The relay is a loopback server
//! whose answers the test writes, the relay client and the connector are fakes (no `cloudflared` runs), and the clock is fixed, so
//! nothing leaves the machine and nothing waits on real time. The fakes are the ones `t3-remote`'s own tests use.
#![allow(dead_code, unused_imports)]
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use t3_contracts::AuthEnvironmentScope;
use t3_remote::{
    backoff::FixedRandom,
    jwt::KeyPair,
    secrets::SecretStore,
};
use t3_server::{
    auth::AuthService,
    connect::{Connect, ConnectOptions},
    persistence::Store,
    transport::{ApiState, router},
};

#[path = "../../../remote/tests/support/mod.rs"]
pub mod remote;
pub use remote::{FakeRelay, FakeSpawner, FixedClock, FixedResolver, NOW_MS, RecordedRequest, within};

pub const ENVIRONMENT_ID: &str = "env_123";

pub struct Options {
    pub relay: Box<dyn Fn(&RecordedRequest) -> (u16, Value) + Send + Sync>,
    /// Secrets present before the server starts (a link left by an earlier run).
    pub secrets: Vec<(String, String)>,
    pub public_config: bool,
    /// Whether `Connect::start` runs. A test that sets up state through the routes first leaves it off.
    pub start: bool,
    pub oauth: Option<t3_remote::config::CloudCliOAuthConfig>,
    /// Replaces the HTTP control plane (which talks to `relay`) with one that answers in process.
    pub plane: Option<Arc<dyn t3_remote::control_plane::ControlPlane>>,
    pub clock_ms: i64,
    /// Replaces the fixed clock; a flow on tokio's paused time passes a clock that moves with it.
    pub clock: Option<Arc<dyn t3_remote::clock::Clock>>,
}

impl Options {
    pub fn new(relay: impl Fn(&RecordedRequest) -> (u16, Value) + Send + Sync + 'static) -> Self {
        Self { relay: Box::new(relay), secrets: vec![], public_config: true, start: true, oauth: None, plane: None, clock_ms: NOW_MS, clock: None }
    }
    pub fn secret(mut self, name: &str, value: impl Into<String>) -> Self {
        self.secrets.push((name.to_owned(), value.into()));
        self
    }
}

pub struct Server {
    pub base: PathBuf,
    pub state_dir: PathBuf,
    pub address: std::net::SocketAddr,
    pub store: Store,
    pub auth: AuthService,
    pub connect: Arc<Connect>,
    pub spawner: Arc<FakeSpawner>,
    pub resolver: Arc<FixedResolver>,
    pub relay: FakeRelay,
    pub relay_keys: KeyPair,
    pub clock: Arc<dyn t3_remote::clock::Clock>,
    /// A bearer session that may operate threads and scheduled tasks.
    pub operator: String,
    /// A bearer session with `relay:write`, with `relay:read`, and with neither.
    pub writer: String,
    pub reader: String,
    pub outsider: String,
    _directory: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

pub fn environment() -> Value {
    json!({"environmentId":ENVIRONMENT_ID,"label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}})
}

pub async fn start(options: Options) -> Server {
    let relay_keys = KeyPair::generate();
    let relay = FakeRelay::start(options.relay).await;
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-connect-").tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let state_dir = base.join("userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_connect".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let writer = issue(vec![AuthEnvironmentScope::RelayRead, AuthEnvironmentScope::RelayWrite]);
    let reader = issue(vec![AuthEnvironmentScope::RelayRead]);
    let outsider = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let spawner = FakeSpawner::new();
    let resolver = FixedResolver::available();
    let clock: Arc<dyn t3_remote::clock::Clock> = options.clock.unwrap_or_else(|| FixedClock::at(options.clock_ms));
    let port = address.port();
    let connect = Connect::new(ConnectOptions {
        store: store.clone(),
        auth: auth.clone(),
        environment: environment(),
        base_dir: base.clone(),
        secrets_dir: state_dir.join("secrets"),
        relay_url: Some(relay.url.clone()).filter(|_| options.public_config),
        has_cloud_public_config: options.public_config,
        cli_oauth: options.oauth.unwrap_or_else(|| t3_remote::config::CloudCliOAuthConfig {
            token_endpoint: String::new(),
            device_authorization_endpoint: String::new(),
            client_id: String::new(),
            loopback_port: 0,
            redirect_uri: String::new(),
            scopes: ["openid", "profile", "email", "offline_access"],
        }),
        hosted_app_url: "https://app.t3.codes".into(),
        local_port: Arc::new(move || Some(port)),
        control_plane: options.plane.unwrap_or_else(|| Arc::new(t3_remote::control_plane::HttpControlPlane::new())),
        relay_client: resolver.clone(),
        connector_spawner: spawner.clone(),
        clock: clock.clone(),
        random: Arc::new(FixedRandom(0.0)),
        allow_loopback_http_relay: true,
    })
    .unwrap();
    for (name, value) in &options.secrets {
        connect.secrets.set(name, value.as_bytes()).unwrap();
    }
    t3_server::connect::install(&store, connect.clone());
    let state = ApiState {
        store: store.clone(),
        auth: auth.clone(),
        environment: environment(),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state).into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    if options.start {
        connect.start();
    }
    Server { base, state_dir, address, store, auth, connect, spawner, resolver, relay, relay_keys, clock, operator, writer, reader, outsider, _directory: directory, stop: Some(stop), server: Some(server) }
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn secret(&self, name: &str) -> Option<String> {
        self.connect.secrets.get(name).unwrap().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The server's finalizers, in the order `main.rs` runs them: the routes stop, then T3 Connect shuts down.
    pub async fn finish(mut self) -> Self {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.connect.shutdown().await;
        self
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// A relay stored link: the secrets a server holds after `t3 connect` linked it, trusting `relay_keys`.
pub fn linked(server_relay_url: &str, relay_keys: &KeyPair) -> Vec<(String, String)> {
    remote::linked_secrets(server_relay_url, relay_keys).into_iter().map(|(name, value)| (name.to_owned(), value)).collect()
}

/// A client's DPoP key, and the proofs it signs.
pub struct DpopClient {
    pair: ring::signature::EcdsaKeyPair,
    pub x: String,
    pub y: String,
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl DpopClient {
    pub fn new() -> Self {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref().to_vec();
        Self { x: b64(&point[1..33]), y: b64(&point[33..65]), pair }
    }

    pub fn thumbprint(&self) -> String {
        t3_server::dpop::jwk_thumbprint("P-256", "EC", &self.x, &self.y)
    }

    /// A proof for `method url` (the URL with or without its query: the proof names it without), hashing `token` when given.
    pub fn proof(&self, method: &str, url: &str, token: Option<&str>, jti: &str) -> String {
        self.proof_at(method, url, token, jti, Utc::now().timestamp())
    }

    pub fn proof_at(&self, method: &str, url: &str, token: Option<&str>, jti: &str, iat: i64) -> String {
        let named = url.split(['?', '#']).next().unwrap();
        let mut claims = json!({"htm":method,"htu":named,"jti":jti,"iat":iat});
        if let Some(token) = token {
            claims["ath"] = t3_server::dpop::access_token_hash(token).into();
        }
        let header = json!({"typ":"dpop+jwt","alg":"ES256","jwk":{"kty":"EC","crv":"P-256","x":self.x,"y":self.y}});
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let signature = self.pair.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
        format!("{signed}.{}", b64(signature.as_ref()))
    }
}


/// A relay that answers in process and keeps what it was asked, in full: the publishes with their states, the wakes, the
/// releases. Time-dependent flows use this one, so a paused clock never races a socket.
#[derive(Default)]
pub struct Recorder {
    pub publishes: std::sync::Mutex<Vec<(String, Value, String)>>,
    pub calls: std::sync::Mutex<Vec<String>>,
    pub wake_pending: std::sync::atomic::AtomicBool,
    changed: tokio::sync::watch::Sender<usize>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn record(&self, name: impl Into<String>) {
        let mut calls = self.calls.lock().unwrap();
        calls.push(name.into());
        self.changed.send_replace(calls.len());
    }
    pub fn count(&self, name: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|call| call.as_str() == name).count()
    }
    /// Waits until at least `n` calls named `name` were made.
    pub async fn wait_for(&self, name: &str, n: usize) {
        let mut changes = self.changed.subscribe();
        within(async {
            loop {
                if self.count(name) >= n {
                    return;
                }
                changes.changed().await.unwrap();
            }
        })
        .await;
    }
}

use remote_types::*;
mod remote_types {
    pub use t3_remote::control_plane::{ControlPlane, RelayResult};
    pub use t3_remote::types::*;
}

#[async_trait::async_trait]
impl ControlPlane for Recorder {
    async fn create_link_challenge(&self, _: &str, _: &str, _: &LinkChallengeRequest) -> RelayResult<LinkChallengeResponse> {
        self.record("challenge");
        Err(remote::unavailable())
    }
    async fn link_environment(&self, _: &str, _: &str, _: &LinkRequest) -> RelayResult<LinkResponse> {
        self.record("link");
        Err(remote::unavailable())
    }
    async fn unlink_environment(&self, _: &str, _: &str, _: &str) -> RelayResult<OkResponse> {
        self.record("unlink");
        Ok(OkResponse { ok: true })
    }
    async fn release_environment_tunnel(&self, _: &str, token: &str, _: &str) -> RelayResult<OkResponse> {
        self.record(format!("release:{token}"));
        self.record("release");
        Ok(OkResponse { ok: true })
    }
    async fn register_tunnel_recovery(&self, _: &str, _: &str, _: &str, _: &RecoveryRegistrationRequest) -> RelayResult<RecoveryRegistrationResponse> {
        self.record("register");
        Ok(RecoveryRegistrationResponse { status: RegistrationStatus::Ready })
    }
    async fn recover_tunnel(&self, _: &str, _: &str, _: &str, _: &RecoveryRequest) -> RelayResult<RecoveryResponse> {
        self.record("recover");
        Err(remote::unavailable())
    }
    async fn update_link_preferences(&self, _: &str, _: &str, _: &str, request: &LinkPreferencesRequest) -> RelayResult<LinkPreferencesRequest> {
        self.record("preferences");
        Ok(request.clone())
    }
    async fn wake_held_hooks(&self, _: &str, token: &str, _: &str) -> RelayResult<WakeHeldHooksResponse> {
        self.record(format!("wake:{token}"));
        self.record("wake");
        Ok(WakeHeldHooksResponse { pending: self.wake_pending.load(std::sync::atomic::Ordering::SeqCst) })
    }
    async fn publish_agent_activity(&self, _: &str, token: &str, _: &str, thread: &str, request: &AgentActivityPublishRequest) -> RelayResult<PublishResponse> {
        self.publishes.lock().unwrap().push((thread.to_owned(), serde_json::to_value(&request.state).unwrap(), request.proof.as_str().to_owned()));
        self.record(format!("publish:{token}"));
        self.record("publish");
        Ok(PublishResponse { ok: true, deliveries: vec![] })
    }
}

impl Server {
    /// A stored CLI sign-in that does not expire within the test.
    pub fn sign_in_cli(&self) {
        let token = json!({"accessToken":"cli-access","refreshToken":"cli-refresh","expiresAtEpochMs":self.clock.now_ms() + 24 * 3_600_000,"identity":"me@example.test"});
        self.connect.secrets.set(CLI_TOKEN, token.to_string().as_bytes()).unwrap();
    }
}

pub const CLI_TOKEN: &str = t3_remote::secrets::CLOUD_CLI_OAUTH_TOKEN_SECRET;
