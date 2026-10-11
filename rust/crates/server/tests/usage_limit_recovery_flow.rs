#![cfg(unix)]
//! Usage-limit recovery end to end through the real WebSocket transport, against the scripted fake Codex
//! app-server: a run stopped by a limit blocks its queue, a recovery armed for it resumes the thread once
//! the reset passes (the manual clock is moved by the test), and a recovery that went stale does nothing.
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
    reset: String,
    next: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness(auto_resume: bool, snooze: bool) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    // The fixture's turn ends at 1800000001 and its limit resets an hour after.
    let reset_seconds: i64 = 1_800_003_600;
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"autoResumeLimitedThreads":auto_resume,"snoozeLimitedThreads":snooze,"providerInstances":{
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
    let reset = chrono::DateTime::from_timestamp(reset_seconds, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Harness { _directory: directory, store, execution, clock, thread, socket, reset, next: 0, stop: Some(stop), server: Some(server) }
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
    /// The thread as a client's shell lists it.
    fn shell(&self) -> Value {
        t3_server::thread::shell(&self.view())
    }
    /// The limited run's recovery is armed, once the failure shows.
    async fn limited(&self) -> Value {
        self.until("the usage limit", |view| (view["runs"][0]["status"] == "failed" && view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "error")).then(|| t3_server::thread::shell(view))).await
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limited_thread_reports_why_and_arming_a_recovery_is_checked_against_the_limit() {
    let mut h = harness(false, false).await;
    h.send("[usage-limit] first").await;
    let shell = h.limited().await;
    assert_eq!((shell["lastErrorClass"].as_str(), shell["usageLimitResetAt"].as_str(), shell["status"].as_str()), (Some("usage_limit"), Some(h.reset.as_str()), Some("failed")));
    let run = h.view()["runs"][0]["id"].clone();

    // A recovery for a limit the thread does not have is refused with the original's reason.
    let exit = h.exit(json!({"type":"thread.metadata.update","commandId":"arm-wrong","threadId":h.thread,"limitRecovery":{"runId":"run:other","resetAt":h.reset,"autoResume":true}})).await;
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["error"]["message"], "The provider limit changed before recovery could be configured.");

    // The one for the thread's limit is kept, with the command that armed it as its request.
    h.command(json!({"type":"thread.metadata.update","commandId":"arm","threadId":h.thread,"limitRecovery":{"runId":run,"resetAt":h.reset,"autoResume":true,"snooze":true}})).await;
    let thread = h.view()["thread"].clone();
    assert_eq!((thread["limitRecovery"]["requestId"].as_str(), thread["snoozedUntil"].as_str()), (Some("arm"), Some(h.reset.as_str())));

    // Clearing it wakes the thread it had put to sleep.
    h.command(json!({"type":"thread.metadata.update","commandId":"disarm","threadId":h.thread,"limitRecovery":null})).await;
    let thread = h.view()["thread"].clone();
    assert_eq!((thread["limitRecovery"].is_null(), thread["snoozedUntil"].is_null()), (true, true));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_armed_recovery_resumes_the_thread_once_the_reset_passes_and_not_before() {
    let mut h = harness(true, false).await;
    h.send("[usage-limit] first").await;
    h.limited().await;
    // The sweep arms the recovery on its own because the setting asks for automatic resumes.
    let armed = h.until("the recovery to be armed", |view| view["thread"]["limitRecovery"]["autoResume"].as_bool().filter(|armed| *armed).map(|_| view["thread"]["limitRecovery"].clone())).await;
    assert_eq!(armed["resetAt"], h.reset.as_str());
    // Before the reset nothing starts.
    let reset_ms = 1_800_003_600_000;
    h.clock.set(reset_ms - 1_000);
    h.execution.drained(&h.thread).await;
    assert_eq!(h.view()["runs"].as_array().unwrap().len(), 1);
    // At the reset the continuation runs: "Continue where you left off." on the same thread.
    h.clock.set(reset_ms + 1_000);
    let view = h.until("the continuation run", |view| (view["runs"].as_array().unwrap().len() >= 2).then(|| view.clone())).await;
    let resumed = view["messages"].as_array().unwrap().iter().find(|message| message["text"] == "Continue where you left off.").unwrap();
    assert_eq!((resumed["createdBy"].as_str(), resumed["creationSource"].as_str()), (Some("user"), Some("server")));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limited_thread_sleeps_until_the_reset_when_the_setting_asks_for_it_and_is_not_resumed() {
    let mut h = harness(false, true).await;
    h.send("[usage-limit] first").await;
    h.limited().await;
    let thread = h.until("the thread to be snoozed", |view| (!view["thread"]["snoozedUntil"].is_null()).then(|| view["thread"].clone())).await;
    assert_eq!((thread["snoozedUntil"].as_str(), thread["limitRecovery"]["snooze"].as_bool(), thread["limitRecovery"]["autoResume"].as_bool()), (Some(h.reset.as_str()), Some(true), Some(false)));
    // Without automatic resumes the reset only wakes the thread; it does not send anything.
    h.clock.set(1_800_003_600_000 + 1_000);
    h.execution.drained(&h.thread).await;
    assert_eq!(h.view()["runs"].as_array().unwrap().len(), 1);
    h.finish().await;
}
