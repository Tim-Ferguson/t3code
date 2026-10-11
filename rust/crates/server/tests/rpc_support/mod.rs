//! A real server on a loopback socket over a temporary state directory, shared by the project,
//! keybinding and thread-search flows. Nothing here reads or writes outside the temp directory.
#![allow(dead_code)]
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    server_settings::SettingsService,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

pub type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Harness {
    /// Canonical, so paths compare with what the server reports.
    pub base: PathBuf,
    pub _directory: tempfile::TempDir,
    pub address: std::net::SocketAddr,
    pub store: Store,
    /// The server's own authentication service, to issue sessions of any scopes and to look at what it holds.
    pub auth: AuthService,
    /// Every operation scope a flow here needs, short of administration.
    pub operator: String,
    /// `orchestration:read` only.
    pub reader: String,
    pub settings: Option<SettingsService>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

/// Where the Rust store lives relative to the harness base: `<base>/userdata/rust-state.sqlite`,
/// so the T3 home is `<base>`, as in a real install.
pub async fn harness() -> Harness {
    start(false, false).await
}

/// The T3 home is itself a Git work tree, where Scratch is not offered.
pub async fn harness_in_checkout() -> Harness {
    start(false, true).await
}

/// With a settings service and a native configuration, as `server.getConfig` and
/// `subscribeServerConfig` need. Real providers are disabled so no CLI on the PATH ever runs.
pub async fn harness_with_config() -> Harness {
    start(true, false).await
}

/// Both of the above: a native configuration, in a T3 home that is a Git work tree.
pub async fn harness_with_config_in_checkout() -> Harness {
    start(true, true).await
}

async fn start(with_config: bool, checkout: bool) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    if checkout {
        assert!(std::process::Command::new("git").args(["init", "--quiet"]).current_dir(&base).status().unwrap().success());
    }
    let state_dir = base.join("userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(
        store.clone(),
        [7; 32],
        "t3_session_rpc".into(),
        "loopback-browser".into(),
    )
    .unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session(
            "fixture",
            "bearer-access-token",
            scopes,
            json!({"deviceType":"unknown"}),
            Utc::now(),
            chrono::Duration::hours(1),
        )
        .unwrap()
        .1
    };
    let operator = issue(vec![
        AuthEnvironmentScope::OrchestrationRead,
        AuthEnvironmentScope::OrchestrationOperate,
        AuthEnvironmentScope::SettingsWrite,
        AuthEnvironmentScope::SourceControlWrite,
        AuthEnvironmentScope::DiagnosticsRead,
    ]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let environment = json!({"environmentId":"rpc-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    let (mut config, mut settings, mut providers) = (None, None, None);
    if with_config {
        use t3_server::{
            config::NativeConfig,
            server_secret_store::ServerSecretStore,
            server_settings::SettingsOptions,
        };
        let path = state_dir.join("settings.json");
        // Both real providers off: the registry then never resolves or runs a provider binary.
        std::fs::write(&path, json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","enabled":false}}}).to_string()).unwrap();
        let mut options = SettingsOptions::file(
            path,
            ServerSecretStore::open(state_dir.join("secrets")).unwrap(),
        );
        options.watch = false;
        let service = SettingsService::start(options).await.unwrap();
        let native = NativeConfig::from_settings(
            service.snapshot().await.unwrap(),
            &state_dir,
            &base,
            &environment,
            &auth.descriptor(),
        )
        .await
        .unwrap();
        providers = Some(native.providers.clone());
        config = Some(native.snapshot);
        settings = Some(service);
    }
    let state = ApiState {
        store: store.clone(),
        auth: auth.clone(),
        environment,
        config,
        settings: settings.clone(),
        cors_origins: None,
        assets: None,
        providers,
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state).into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Harness {
        base,
        _directory: directory,
        address,
        store,
        auth,
        operator,
        reader,
        settings,
        stop: Some(stop),
        server: Some(server),
    }
}

impl Harness {
    pub async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    pub async fn finish(mut self) {
        if let Some(settings) = &self.settings {
            settings.shutdown().await;
        }
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }
}

pub async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
pub async fn next(socket: &mut Socket) -> Value {
    let message = socket.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

/// One unary request and its `Exit`. Boxed: debug-build futures of a long flow would otherwise
/// overflow its thread stack.
pub fn rpc<'a>(
    socket: &'a mut Socket,
    id: u64,
    tag: &'a str,
    payload: Value,
) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        send(
            socket,
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}),
        )
        .await;
        loop {
            let value = next(socket).await;
            if value["_tag"] == "Exit" && value["requestId"] == id {
                return value["exit"].clone();
            }
        }
    })
}
pub fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}
/// The wire error of a typed failure.
pub fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}
/// The defect text of a payload that failed to decode.
pub fn defect(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Die", "{exit}");
    &exit["cause"][0]["defect"]
}
