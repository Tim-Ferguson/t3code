//! What the Grok flows share: a server over the real WebSocket transport for an execution service that is
//! already running, a client socket, and a stream on a socket of its own whose chunks are acknowledged.
#![allow(dead_code)]
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    persistence::Store,
    provider_auth_service::ProviderAuthService,
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

pub type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Served {
    pub address: std::net::SocketAddr,
    pub token: String,
    /// What `main.rs` attaches beside the execution service: every session of an instance with a sign-in is admitted by it.
    pub provider_auth: ProviderAuthService,
    _caches: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}
impl Served {
    pub async fn connect(&self) -> Socket {
        connect(self.address, &self.token).await
    }
    pub async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.provider_auth.shutdown().await;
    }
}

/// The provider authentication service a restarted execution service needs, built the way `main.rs` builds it.
pub fn attach_auth(execution: &ExecutionService, providers: &ProviderRegistry, directory: &std::path::Path) -> ProviderAuthService {
    ProviderAuthService::for_server(execution, providers, directory.to_owned(), directory)
}

/// Serves `execution` and `providers` on a loopback port with a session that may read and operate, with the
/// provider authentication service attached as `main.rs` attaches it.
pub async fn serve(store: &Store, providers: &ProviderRegistry, execution: &ExecutionService) -> Served {
    let caches = tempfile::tempdir().unwrap();
    let provider_auth = attach_auth(execution, providers, caches.path());
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_grok".into(), "loopback-browser".into()).unwrap();
    let token = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"grok-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: Some(providers.clone()),
        execution: Some(execution.clone()),
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: Some(provider_auth.clone()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    Served { address, token, provider_auth, _caches: caches, stop: Some(stop), server: Some(server) }
}

pub async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// A stream on a socket of its own: every chunk is acknowledged and kept.
pub struct Stream {
    pub chunks: Arc<Mutex<Vec<Value>>>,
    arrived: tokio::sync::watch::Receiver<usize>,
    _task: tokio::task::JoinHandle<()>,
}
impl Stream {
    pub async fn open(served: &Served, tag: &'static str, payload: Value) -> Self {
        let mut socket = served.connect().await;
        socket.send(Message::Text(json!({"_tag":"Request","id":1,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
        let chunks = Arc::new(Mutex::new(vec![]));
        let (count, arrived) = tokio::sync::watch::channel(0);
        let kept = chunks.clone();
        let task = tokio::spawn(async move {
            while let Some(Ok(message)) = socket.next().await {
                let Ok(text) = message.to_text() else { continue };
                let value: Value = serde_json::from_str(text).unwrap();
                if value["_tag"] == "Chunk" && value["requestId"] == 1 {
                    kept.lock().unwrap().extend(value["values"].as_array().unwrap().iter().cloned());
                    count.send_modify(|count| *count += 1);
                    socket.send(Message::Text(json!({"_tag":"Ack","requestId":1}).to_string().into())).await.unwrap();
                }
            }
        });
        Self { chunks, arrived, _task: task }
    }
    /// Resolves once what the stream delivered so far satisfies `check`.
    pub async fn until(&mut self, what: &str, check: impl Fn(&[Value]) -> bool) {
        crate::provider_manage_support::within(format!("the stream to deliver {what}"), async {
            loop {
                if check(&self.chunks.lock().unwrap()) {
                    return;
                }
                self.arrived.changed().await.unwrap();
            }
        })
        .await
    }
}

