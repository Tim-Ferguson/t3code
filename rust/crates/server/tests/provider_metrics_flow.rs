#![cfg(unix)]
//! The provider counters and timer of the original (`t3_provider_turns_total`, `t3_provider_turn_duration`, `t3_provider_sessions_total`)
//! over the real WebSocket transport, against the scripted fake Codex app-server: a turn starts a session, a quiet window releases it, a
//! new turn opens another, a stop is a turn operation, and the end of the server releases what is left. Time is a manual clock that only
//! the test moves; the test waits on persisted events and on the idle decisions, never on sleeps. A process of its own, so what the
//! flow records is all there is.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    test_support::HANG_BOUND,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    clock: std::sync::Arc<ManualClock>,
    thread: String,
    socket: Socket,
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
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")}}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let clock = ManualClock::new(Utc::now().timestamp_millis());
    providers.idle_release().configure(clock.clone(), IDLE_TIMEOUT_MS, MAX_PIN_MS);
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"gpt-fixture"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_metrics".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"metrics-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    Harness { _directory: directory, store, execution, providers, clock, thread, socket, next: 0, stop: Some(stop), server: Some(server) }
}

impl Harness {
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    fn command(&mut self, command: Value) -> futures_util::future::BoxFuture<'_, Value> {
        self.next += 1;
        let id = self.next;
        Box::pin(async move {
            self.socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"orchestration.dispatchCommand","payload":command,"headers":[]}).to_string().into())).await.unwrap();
            loop {
                let message = tokio::time::timeout(HANG_BOUND, self.socket.next()).await.expect("no reply to the command").unwrap().unwrap();
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
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }
    async fn settled(&self, runs: usize) {
        self.until("the runs to settle", |view| (view["runs"].as_array().unwrap().len() >= runs && view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled")))).then_some(())).await;
        self.execution.drained(&self.thread).await;
    }
    /// A quiet window longer than the idle timeout, decided by the effect loop.
    async fn pass_idle_window(&self) {
        let mut decisions = self.providers.idle_release().decisions();
        let before = *decisions.borrow_and_update();
        self.providers.idle_release().caught_up(&self.store).await;
        self.clock.advance(31 * 60_000);
        tokio::time::timeout(HANG_BOUND, decisions.wait_for(|count| *count > before)).await.expect("the idle window was decided").unwrap();
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

/// The data points of a metric by attributes; a timer's value is its observation count.
fn points(metrics: &[Value], name: &str) -> BTreeMap<String, f64> {
    let Some(metric) = metrics.iter().find(|metric| metric["name"] == name) else { return BTreeMap::new() };
    let kind = if metric.get("sum").is_some() { "sum" } else { "histogram" };
    metric[kind]["dataPoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|point| {
            let mut attributes: Vec<String> = point["attributes"].as_array().unwrap().iter().map(|attribute| format!("{}={}", attribute["key"].as_str().unwrap(), attribute["value"]["stringValue"].as_str().unwrap())).collect();
            attributes.retain(|attribute| attribute != "time_unit=milliseconds");
            (attributes.join(","), point.get("asDouble").or_else(|| point.get("count")).and_then(Value::as_f64).unwrap())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn turns_and_sessions_are_counted_where_the_adapters_are_dispatched() {
    t3_server::otlp::record_for_flows();
    let mut harness = harness().await;

    // A turn opens a session; a quiet window releases it.
    harness.send("first").await;
    harness.settled(1).await;
    harness.pass_idle_window().await;
    harness.until("the session to be released", |view| view["providerSessions"].as_array().unwrap().iter().all(|session| session["status"] == "stopped").then_some(())).await;

    // The next turn opens another, runs until it is stopped, and the stop is an operation of its own.
    harness.send("[hold] work for a long time").await;
    harness.until("the run to be running", |view| (view["runs"][1]["status"] == "running").then_some(())).await;
    let run = harness.view()["runs"][1]["id"].clone();
    harness.command(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run})).await;
    harness.settled(2).await;

    // The server ending releases the session still open, after all work is done: the metrics below are complete once it has.
    harness.finish().await;
    let metrics = t3_server::otlp::recorded_for_flows();
    let turns = points(&metrics, "t3_provider_turns_total");
    assert_eq!(turns.get("provider=codex,operation=send,modelFamily=gpt,outcome=success"), Some(&2.0), "{turns:?}");
    assert_eq!(turns.get("provider=codex,operation=interrupt,outcome=success"), Some(&1.0), "{turns:?}");
    assert!(turns.keys().all(|key| !key.contains("outcome=interrupt")), "nothing was cut off: {turns:?}");
    // Only the start of a turn is timed, and the timer has no outcome.
    assert_eq!(points(&metrics, "t3_provider_turn_duration"), BTreeMap::from([("provider=codex,operation=send,modelFamily=gpt".to_owned(), 2.0)]));
    let sessions = points(&metrics, "t3_provider_sessions_total");
    assert_eq!(sessions.get("provider=codex,operation=open,outcome=success"), Some(&2.0), "{sessions:?}");
    assert_eq!(sessions.get("provider=codex,operation=release,reason=idle_timeout,outcome=success"), Some(&1.0), "{sessions:?}");
    assert_eq!(sessions.get("provider=codex,operation=release,reason=server_shutdown,outcome=success"), Some(&1.0), "{sessions:?}");
}
