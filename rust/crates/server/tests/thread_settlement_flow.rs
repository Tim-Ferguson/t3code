#![cfg(unix)]
//! Settling a thread end to end through the real WebSocket transport, against the scripted fake Codex
//! app-server: a settle releases the provider session, a message brings the thread back, and the sweep
//! settles a thread nobody touched once the manual clock has moved past the configured days.
//! Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::Store,
    project::ProjectService,
    provider_idle::{IDLE_TIMEOUT_MS, MAX_PIN_MS},
    provider_registry::ProviderRegistry,
    scheduled_tasks::ManualClock,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    clock: std::sync::Arc<ManualClock>,
    thread: String,
    socket: Socket,
    next: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness(after_days: Option<u32>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"sidebarAutoSettleAfterDays":after_days,"sidebarAutoSettleOnMerge":false,"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")}}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let clock = ManualClock::new(Utc::now().timestamp_millis());
    providers.idle_release().configure(clock.clone(), IDLE_TIMEOUT_MS, MAX_PIN_MS);
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_limit".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"limit-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {operator}").parse().unwrap());
    let socket = tokio_tungstenite::connect_async(request).await.unwrap().0;
    Harness { _directory: directory, store, execution, clock, thread, socket, next: 0, stop: Some(stop), server: Some(server) }
}

impl Harness {
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    fn exit(&mut self, command: Value) -> futures_util::future::BoxFuture<'_, Value> {
        self.next += 1;
        let id = self.next;
        Box::pin(async move {
            self.socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"orchestration.dispatchCommand","payload":command,"headers":[]}).to_string().into())).await.unwrap();
            loop {
                let message = tokio::time::timeout(HANG_BOUND, self.socket.next()).await.expect("no reply to the command within 30s").unwrap().unwrap();
                let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                if value["_tag"] == "Exit" && value["requestId"] == id {
                    return value["exit"].clone();
                }
            }
        })
    }
    fn command(&mut self, command: Value) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            let exit = self.exit(command).await;
            assert_eq!(exit["_tag"], "Success", "{exit}");
        })
    }
    fn send<'a>(&'a mut self, text: &'a str) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let id = format!("message-{}", self.next + 1);
            self.command(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        })
    }
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}; runs {}; thread {}", self.view()["runs"], self.view()["thread"]))
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}


#[tokio::test(flavor = "multi_thread")]
async fn settling_releases_the_provider_session_and_a_message_brings_the_thread_back() {
    let mut h = harness(None).await;
    h.send("hello").await;
    h.until("the run to complete", |view| (view["runs"][0]["status"] == "completed").then_some(())).await;
    h.execution.drained(&h.thread).await;
    assert!(h.view()["providerSessions"].as_array().unwrap().iter().any(|session| session["status"] != "stopped"));
    h.command(json!({"type":"thread.settle","commandId":"settle","threadId":h.thread})).await;
    let thread = h.view()["thread"].clone();
    assert_eq!(thread["settledOverride"], "settled");
    // The session the thread held is released, with the reason the original gives.
    h.until("the session to be released", |view| view["providerSessions"].as_array().unwrap().is_empty().then_some(())).await;
    let detached: Vec<Value> = h.store.events(0, None, Some("thread"), Some(&h.thread), 10_000).unwrap().into_iter().filter(|event| event.event.event_type == "provider-session.detached").map(|event| event.event.payload).collect();
    assert_eq!((detached.len(), detached[0]["reason"].as_str()), (1, Some("Thread settled.")));

    // Writing to a settled thread un-settles it, and the run starts.
    h.send("again").await;
    let view = h.until("the second run", |view| (view["runs"].as_array().unwrap().len() == 2 && view["runs"][1]["status"] == "completed").then(|| view.clone())).await;
    assert_eq!((view["thread"]["settledOverride"].is_null(), view["thread"]["settledAt"].is_null()), (true, true));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_settles_a_thread_nobody_touched_for_the_configured_days_and_leaves_a_pinned_one() {
    let mut h = harness(Some(2)).await;
    h.send("hello").await;
    let view = h.until("the run to complete", |view| (view["runs"][0]["status"] == "completed").then(|| view.clone())).await;
    let finished = view["runs"][0]["completedAt"].as_str().unwrap().to_owned();
    h.execution.drained(&h.thread).await;
    h.command(json!({"type":"thread.pin","commandId":"pin","threadId":h.thread})).await;
    let now = Utc::now().timestamp_millis();
    // Two days have not passed: nothing settles.
    h.clock.set(now + 86_400_000);
    h.execution.drained(&h.thread).await;
    assert!(h.view()["thread"]["settledOverride"].is_null());
    // Three days have, but a pinned thread is not stale.
    h.clock.set(now + 3 * 86_400_000);
    h.execution.drained(&h.thread).await;
    assert!(h.view()["thread"]["settledOverride"].is_null(), "pinned");
    h.command(json!({"type":"thread.unpin","commandId":"unpin","threadId":h.thread})).await;
    h.clock.set(now + 4 * 86_400_000);
    let thread = h.until("the sweep to settle the thread", |view| (view["thread"]["settledOverride"] == "settled").then(|| view["thread"].clone())).await;
    // It settles at its last activity, not when the sweep noticed.
    let settled_at = chrono::DateTime::parse_from_rfc3339(thread["settledAt"].as_str().unwrap()).unwrap();
    let activity = chrono::DateTime::parse_from_rfc3339(&finished).unwrap();
    assert!(settled_at <= activity + chrono::Duration::seconds(5), "{settled_at} {activity}");
    h.finish().await;
}
