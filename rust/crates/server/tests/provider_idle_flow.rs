#![cfg(unix)]
//! Idle release of a provider process (`provider_idle.rs`) over the real WebSocket transport, against the
//! scripted fake Codex app-server. Time is a manual clock that only the test moves; the test waits on
//! persisted events and on the number of idle windows decided, never on sleeps.
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
const MINUTE: i64 = 60_000;

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    clock: std::sync::Arc<ManualClock>,
    thread: String,
    socket: Socket,
    requests: std::path::PathBuf,
    /// The `initialize` calls the provider probes made before any thread ran; they are not thread processes.
    probes: usize,
    next: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness() -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let requests = cwd.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":requests}]}}})).unwrap();
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
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_idle".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"idle-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    let mut harness = Harness { _directory: directory, store, execution, providers, clock, thread, socket, requests, probes: 0, next: 0, stop: Some(stop), server: Some(server) };
    harness.probes = harness.initializations();
    harness
}

impl Harness {
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    /// One command over the socket, as a client sends it.
    fn command(&mut self, command: Value) -> futures_util::future::BoxFuture<'_, Value> {
        self.next += 1;
        let id = self.next;
        Box::pin(async move {
            self.socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"orchestration.dispatchCommand","payload":command,"headers":[]}).to_string().into())).await.unwrap();
            loop {
                let message = tokio::time::timeout(HANG_BOUND, self.socket.next()).await.expect("no reply to the command within 30s").unwrap().unwrap();
                let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                if value["_tag"] == "Exit" && value["requestId"] == id {
                    assert_eq!(value["exit"]["_tag"], "Success", "{value}");
                    return value["exit"]["value"].clone();
                }
            }
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
        .unwrap_or_else(|_| panic!("timed out waiting for {what}; runs {}; sessions {}", self.view()["runs"], self.view()["providerSessions"]))
    }
    /// Every run is terminal and the thread has no effect left to run.
    async fn settled(&self, runs: usize) {
        self.until("the runs to settle", |view| (view["runs"].as_array().unwrap().len() >= runs && view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled")))).then_some(())).await;
        self.execution.drained(&self.thread).await;
    }
    async fn released(&self) {
        self.until("the provider session to be released", |view| {
            let sessions = view["providerSessions"].as_array().unwrap();
            (!sessions.is_empty() && sessions.iter().all(|session| session["status"] == "stopped")).then_some(())
        })
        .await;
    }
    /// Moves time once the effect loop has taken everything committed so far into account.
    async fn advance(&self, minutes: i64) {
        self.providers.idle_release().caught_up(&self.store).await;
        self.clock.advance(minutes * MINUTE);
    }
    /// Moves time and waits until the effect loop has decided the window that ended, whichever way.
    async fn pass(&self, minutes: i64) {
        let mut decisions = self.providers.idle_release().decisions();
        let before = *decisions.borrow_and_update();
        self.advance(minutes).await;
        tokio::time::timeout(HANG_BOUND, decisions.wait_for(|count| *count > before)).await.unwrap_or_else(|_| panic!("no idle decision within 30s after moving the clock {minutes} minutes (decisions so far {before}, tracked deadline unknown)")).unwrap();
    }
    fn methods(&self) -> Vec<String> {
        std::fs::read_to_string(&self.requests).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].as_str().unwrap().to_owned()).collect()
    }
    fn initializations(&self) -> usize {
        self.methods().iter().filter(|method| *method == "initialize").count()
    }
    /// Provider processes started for the thread's turns.
    fn processes_started(&self) -> usize {
        self.initializations() - self.probes
    }
    fn session_statuses(&self) -> Vec<String> {
        self.view()["providerSessions"].as_array().unwrap().iter().map(|session| session["status"].as_str().unwrap().to_owned()).collect()
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_provider_process_is_released_after_the_idle_window_and_the_next_message_resumes_the_conversation() {
    let mut harness = harness().await;
    harness.send("first").await;
    harness.settled(1).await;
    assert_eq!(harness.processes_started(), 1);

    // Activity restarts the window: 29 quiet minutes, a turn, 29 more, a turn, and the process is the same.
    harness.advance(29).await;
    harness.send("second").await;
    harness.settled(2).await;
    harness.advance(29).await;
    harness.send("third").await;
    harness.settled(3).await;
    assert_eq!(harness.processes_started(), 1, "windows that never ran out released nothing: {:?}", harness.methods());
    assert!(!harness.session_statuses().contains(&"stopped".to_owned()), "{:?}", harness.session_statuses());

    // 31 quiet minutes release it: the session is stopped and the process is gone.
    harness.pass(31).await;
    harness.released().await;
    assert_eq!(harness.processes_started(), 1);

    // The next message starts a new process that loads the native thread again, as after a restart.
    let native = harness.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"].clone();
    harness.send("fourth").await;
    harness.settled(4).await;
    let methods = harness.methods();
    assert_eq!(harness.processes_started(), 2, "{methods:?}");
    let second_start = methods.iter().enumerate().filter(|(_, method)| *method == "initialize").nth(harness.probes + 1).unwrap().0;
    assert!(methods[second_start..].iter().any(|method| method == "thread/resume"), "{methods:?}");
    assert_eq!(harness.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"], native, "the same native thread continues");
    assert_eq!(harness.view()["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(), vec![json!("completed"); 4]);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_turn_keeps_its_process_however_long_it_takes() {
    let mut harness = harness().await;
    harness.send("[hold] work for a long time").await;
    harness.until("the run to be running with its first words persisted", |view| {
        let running = view["runs"][0]["status"] == "running";
        (running && view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "assistant_message")).then_some(())
    })
    .await;
    harness.pass(31).await;
    harness.pass(31).await;
    assert_eq!(harness.view()["runs"][0]["status"], "running");
    assert!(!harness.session_statuses().contains(&"stopped".to_owned()), "{:?}", harness.session_statuses());
    assert_eq!(harness.processes_started(), 1);
    // Stopping the turn makes the thread quiet, and a full window after that releases it.
    let run = harness.view()["runs"][0]["id"].clone();
    harness.command(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run})).await;
    harness.settled(1).await;
    harness.pass(31).await;
    harness.released().await;
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_work_keeps_the_process_for_further_windows_but_not_beyond_the_pin_cap() {
    let mut harness = harness().await;
    harness.send("[background] start the server").await;
    harness.settled(1).await;
    harness.until("the command to run on after the turn", |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").map(|_| ())).await;

    // Pending work defers each ended window, for up to four hours in all.
    harness.pass(31).await;
    harness.pass(120).await;
    assert!(!harness.session_statuses().contains(&"stopped".to_owned()), "{:?}", harness.session_statuses());
    // The process is the one still running the command: the next turn is served by it and ends the command.
    harness.send("[finish-background] go on").await;
    harness.settled(2).await;
    assert_eq!(harness.processes_started(), 1);
    harness.until("the command to end", |view| view["turnItems"].as_array().unwrap().iter().all(|item| item["type"] != "command_execution" || item["status"] != "running").then_some(())).await;
    harness.pass(31).await;
    harness.released().await;
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_work_that_outlasts_the_pin_cap_is_released_anyway() {
    let mut harness = harness().await;
    harness.send("[background] start the server").await;
    harness.settled(1).await;
    harness.until("the command to run on after the turn", |view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").map(|_| ())).await;
    harness.pass(31).await;
    assert!(!harness.session_statuses().contains(&"stopped".to_owned()));
    // The first deferral began the pin; four hours later it has run out.
    harness.pass(4 * 60).await;
    harness.released().await;
    harness.finish().await;
}
