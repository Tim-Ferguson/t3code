#![cfg(unix)]
#![allow(dead_code)]
//! Scheduled tasks end to end over the real WebSocket transport: the eight request methods and the list
//! subscription with their envelopes and permissions, and runs that reach the deterministic Codex
//! provider fixture. Time is a manual clock the test moves; the test then waits on a committed event, a
//! stream message or a task state, never on a sleep. The restart tests drop the first service and store
//! (a crash: nothing is shut down) and open a second over the same database file.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    persistence::Store,
    project::ProjectService,
    provider_registry::ProviderRegistry,
    scheduled_tasks::{ExecutionLauncher, LaunchRequest, ManualClock, RunLauncher, ScheduledTasks},
    scheduled_tasks_schedule::ChronoZone,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// 2026-09-09T12:00:00Z, a Wednesday.
const T0: i64 = 1_788_955_200_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const GUARD: std::time::Duration = std::time::Duration::from_secs(60);

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

struct Flow {
    address: std::net::SocketAddr,
    store: Store,
    execution: ExecutionService,
    service: ScheduledTasks,
    clock: Arc<ManualClock>,
    record: std::path::PathBuf,
    operator: String,
    reader: String,
    outsider: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

/// Which provider fixture the server's registry runs. The others are disabled, so no real CLI on the
/// PATH is ever started.
#[derive(Clone, Copy, PartialEq)]
enum Provider {
    Codex,
    Claude,
    Acp,
}

/// Starts a server over the state directory. `timer` runs the scheduler's own timer; a test that drives
/// ticks itself passes false.
async fn start(directory: &std::path::Path, clock: Arc<ManualClock>, timer: bool) -> Flow {
    start_with(directory, clock, timer, Provider::Codex).await
}
async fn start_with(directory: &std::path::Path, clock: Arc<ManualClock>, timer: bool, provider: Provider) -> Flow {
    let store = Store::open(directory.join("rust-state.sqlite")).unwrap();
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // The project exists once; a restart finds it already.
    if store.projection("project", "project").unwrap().is_none() {
        ProjectService::new(store.clone())
            .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now())
            .unwrap();
    }
    let record = directory.join("codex-requests.jsonl");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let claude = directory.join("claude");
    std::fs::write(&claude, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixtures.join("claude-provider.py").display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":provider==Provider::Claude,"config":{"binaryPath":claude,"homePath":directory.join("claude-home")},"environment":[{"name":"CLAUDE_FIXTURE_SCENARIO","value":"turn"},{"name":"CLAUDE_FIXTURE_LOG","value":directory.join("claude.log")}]},
        "codex":{"driver":"codex","enabled":provider==Provider::Codex,"config":{"binaryPath":fixtures.join("codex-provider.py"),"homePath":directory.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]},
        "local-agent":{"driver":"acpRegistry","enabled":provider==Provider::Acp,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[fixtures.join("acp-provider.py"),"2","normal"]}}}}))
    .unwrap();
    let providers = ProviderRegistry::discover(&settings, directory).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let service = ScheduledTasks::open(store.clone(), clock.clone(), Arc::new(ChronoZone(chrono_tz::UTC))).unwrap();
    service.set_launcher(ExecutionLauncher::new(execution.clone()));
    if timer {
        service.spawn_timer();
    }
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_scheduled".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let outsider = issue(vec![AuthEnvironmentScope::TerminalRead]);
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"scheduled-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: Some(providers),
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
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Flow { address, store, execution, service, clock, record, operator, reader, outsider, stop: Some(stop), server: Some(server) }
}

impl Flow {
    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    /// A clean stop: the server, the timer and the provider runtime.
    async fn finish(mut self) {
        self.service.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
    /// A crash: the server and the provider runtime stop, but nothing tells the scheduler, so a run it
    /// claimed stays claimed. The database file is left as it is.
    async fn crash(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
    /// The threads the server holds, oldest first.
    fn threads(&self) -> Vec<Value> {
        let mut threads = self.store.projections("thread").unwrap();
        threads.sort_by_key(|thread| thread["thread"]["createdAt"].as_str().unwrap().to_owned());
        threads
    }
    /// The prompts the provider fixture was asked to run.
    fn provider_turns(&self) -> Vec<String> {
        let Ok(record) = std::fs::read_to_string(&self.record) else { return vec![] };
        record
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| line["method"] == "turn/start")
            .map(|line| line["params"]["input"][0]["text"].as_str().unwrap().to_owned())
            .collect()
    }
    /// Waits on committed events until `ready` holds for the stored threads.
    async fn threads_where(&self, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let mut events = self.store.subscribe();
        tokio::time::timeout(GUARD, async {
            loop {
                let threads = self.threads();
                if ready(&threads) {
                    return threads;
                }
                events.recv().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the threads never reached the expected state: {:#?}", self.threads().iter().map(|thread| thread["runs"].clone()).collect::<Vec<_>>()))
    }
}

fn settled(thread: &Value) -> bool {
    let runs = thread["runs"].as_array().unwrap();
    !runs.is_empty() && runs.iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted")))
}

async fn send(socket: &mut Socket, value: Value) {
    socket.send(Message::Text(value.to_string().into())).await.unwrap();
}
async fn next(socket: &mut Socket) -> Value {
    let message = tokio::time::timeout(GUARD, socket.next()).await.expect("the server went quiet").unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    send(socket, json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})).await;
    loop {
        let value = next(socket).await;
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}
fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}
/// The typed error of a failed request.
fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}
fn defect(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Die", "{exit}");
    &exit["cause"][0]["defect"]
}
/// Reads one subscription until a list satisfies `wanted`, acknowledging each chunk.
async fn list_where(socket: &mut Socket, id: u64, wanted: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    loop {
        let message = next(socket).await;
        assert_eq!(message["requestId"], id, "{message}");
        assert_eq!(message["_tag"], "Chunk", "{message}");
        send(socket, json!({"_tag":"Ack","requestId":id})).await;
        for value in message["values"].as_array().unwrap() {
            let tasks = value["tasks"].as_array().unwrap().clone();
            if wanted(&tasks) {
                return tasks;
            }
        }
    }
}

fn input(overrides: Value) -> Value {
    let mut base = json!({
        "title": "Review", "prompt": "Review the open pull requests.", "enabled": true, "schedule": {"type": "interval", "everyMs": 300_000}, "projectId": "project",
        "workspaceStrategy": {"type": "root"}, "modelSelection": {"instanceId": "codex", "model": "fixture-model"}, "runtimeMode": "approval-required", "interactionMode": "default",
    });
    for (key, value) in overrides.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}
fn task_of(tasks: &[Value], id: &str) -> Value {
    tasks.iter().find(|task| task["id"] == id).cloned().unwrap_or(Value::Null)
}

#[tokio::test]
async fn every_method_answers_with_the_originals_results_and_envelopes() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), false).await;
    let mut operator = flow.connect(&flow.operator).await;
    let mut watcher = flow.connect(&flow.operator).await;
    send(&mut watcher, json!({"_tag":"Request","id":900,"tag":"scheduledTasks.subscribe","payload":{},"headers":[]})).await;
    assert!(list_where(&mut watcher, 900, |tasks| tasks.is_empty()).await.is_empty());

    assert_eq!(success(&rpc(&mut operator, 1, "scheduledTasks.list", json!({})).await), &json!({"tasks": []}));

    // Create: the result is the stored task, and the subscription shows it.
    let created = success(&rpc(&mut operator, 2, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:review", "createdBy": "agent", "creationSource": "mcp"}))).await).clone();
    let task = &created["task"];
    assert_eq!(task["id"], "scheduled-task:review");
    assert_eq!((task["createdBy"].as_str(), task["creationSource"].as_str(), task["lastRunStatus"].as_str(), task["runCount"].as_u64()), (Some("agent"), Some("mcp"), Some("never"), Some(0)));
    assert_eq!(task["nextRunAt"], iso(T0 + 5 * MINUTE));
    assert_eq!(task["createdAt"], iso(T0));
    assert_eq!(task["threadId"], Value::Null);
    let seen = list_where(&mut watcher, 900, |tasks| !tasks.is_empty()).await;
    assert_eq!(seen[0]["id"], "scheduled-task:review");

    // Failures: a payload that does not decode is a defect; the others are ScheduledTaskError.
    defect(&rpc(&mut operator, 3, "scheduledTasks.upsert", input(json!({"schedule": {"type": "interval", "everyMs": 1000}}))).await);
    defect(&rpc(&mut operator, 4, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:review"})).await);
    defect(&rpc(&mut operator, 5, "scheduledTasks.list", Value::Null).await);
    let missing = rpc(&mut operator, 6, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:ghost", "requireExisting": true}))).await;
    assert_eq!(failure(&missing), &json!({"_tag": "ScheduledTaskError", "message": "Schedule task not found.", "taskId": "scheduled-task:ghost"}));
    let missing = rpc(&mut operator, 7, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:ghost", "enabled": false})).await;
    assert_eq!(failure(&missing)["message"], "Schedule task not found.");

    // Disable and enable: only the flag and the next run change.
    let disabled = success(&rpc(&mut operator, 8, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:review", "enabled": false})).await).clone();
    assert_eq!((disabled["task"]["enabled"].as_bool(), disabled["task"]["nextRunAt"].clone()), (Some(false), Value::Null));
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:review")["enabled"] == false).await;
    flow.clock.set(T0 + HOUR);
    let enabled = success(&rpc(&mut operator, 9, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:review", "enabled": true})).await).clone();
    assert_eq!(enabled["task"]["nextRunAt"], iso(T0 + HOUR + 5 * MINUTE));

    // Run now launches a real thread through the provider fixture and records the run.
    flow.clock.set(T0 + HOUR + MINUTE);
    let ran = success(&rpc(&mut operator, 10, "scheduledTasks.runNow", json!({"id": "scheduled-task:review"})).await).clone();
    assert_eq!((ran["task"]["lastRunStatus"].as_str(), ran["task"]["runCount"].as_u64()), (Some("succeeded"), Some(1)));
    assert_eq!(ran["task"]["lastRunAt"], iso(T0 + HOUR + MINUTE));
    assert_eq!(ran["task"]["nextRunAt"], iso(T0 + HOUR + 6 * MINUTE));
    let threads = flow.threads_where(|threads| threads.len() == 1 && settled(&threads[0])).await;
    assert_eq!(threads[0]["messages"][0]["text"], "Review the open pull requests.");
    assert_eq!(threads[0]["thread"]["title"], "Review");
    // The task was configured by an agent: the thread and its first message are the agent's, and
    // the message and its transcript item name the task that sent them.
    assert_eq!((threads[0]["thread"]["createdBy"].as_str(), threads[0]["thread"]["creationSource"].as_str()), (Some("agent"), Some("mcp")));
    let message = &threads[0]["messages"][0];
    assert_eq!((message["createdBy"].as_str(), message["creationSource"].as_str(), message["scheduledTaskId"].as_str()), (Some("agent"), Some("mcp"), Some("scheduled-task:review")));
    let item = threads[0]["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_message").unwrap();
    assert_eq!((item["createdBy"].as_str(), item["scheduledTaskId"].as_str()), (Some("agent"), Some("scheduled-task:review")));
    assert_eq!(flow.provider_turns(), ["Review the open pull requests."]);
    let history = flow.service.runs("scheduled-task:review").await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!((history[0].trigger.as_str(), history[0].status.as_str(), history[0].thread_id.as_deref()), ("manual", "succeeded", threads[0]["thread"]["id"].as_str()));

    // Webhook tasks: the URL path, the secret flag, rotation, the empty delivery log.
    let webhook = input(json!({"id": "scheduled-task:hook", "title": "Hook", "prompt": "PR {{body.url}}", "schedule": {"type": "webhook", "signature": {"header": "X-Sig", "encoding": "hex", "prefix": "sha256=", "secret": "s3cret"}}}));
    let hook = success(&rpc(&mut operator, 11, "scheduledTasks.upsert", webhook).await).clone();
    assert_eq!(hook["task"]["nextRunAt"], Value::Null);
    assert_eq!(hook["task"]["schedule"], json!({"type": "webhook", "signature": {"header": "x-sig", "encoding": "hex", "prefix": "sha256="}, "maxDeliveryAgeMinutes": null}));
    let path = hook["task"]["webhook"]["path"].as_str().unwrap().to_owned();
    assert!(path.starts_with("/api/hooks/scheduled-task%3Ahook/"), "{path}");
    assert_eq!((hook["task"]["webhook"]["url"].clone(), hook["task"]["webhook"]["hasSecret"].clone()), (Value::Null, json!(true)));
    let rotated = success(&rpc(&mut operator, 12, "scheduledTasks.rotateWebhookToken", json!({"id": "scheduled-task:hook"})).await).clone();
    assert_ne!(rotated["task"]["webhook"]["path"], path);
    assert_eq!(failure(&rpc(&mut operator, 13, "scheduledTasks.rotateWebhookToken", json!({"id": "scheduled-task:review"})).await)["message"], "Only webhook tasks have a URL token.");
    assert_eq!(failure(&rpc(&mut operator, 14, "scheduledTasks.runNow", json!({"id": "scheduled-task:hook"})).await)["message"], "Webhook tasks run when their URL receives a request.");
    assert_eq!(success(&rpc(&mut operator, 15, "scheduledTasks.listWebhookDeliveries", json!({"id": "scheduled-task:hook"})).await), &json!({"deliveries": []}));
    let no_delivery = rpc(&mut operator, 16, "scheduledTasks.getWebhookDelivery", json!({"id": "scheduled-task:hook", "deliveryId": "delivery:none"})).await;
    assert_eq!(failure(&no_delivery), &json!({"_tag": "ScheduledTaskError", "message": "Webhook delivery not found.", "taskId": "scheduled-task:hook"}));
    // A failed manual run is still an answer: the run is recorded as failed.
    let unknown = rpc(&mut operator, 17, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:broken", "threadId": "no-such-thread"}))).await;
    assert_eq!(success(&unknown)["task"]["threadId"], "no-such-thread");
    let failed = success(&rpc(&mut operator, 18, "scheduledTasks.runNow", json!({"id": "scheduled-task:broken"})).await).clone();
    assert_eq!((failed["task"]["lastRunStatus"].as_str(), failed["task"]["runCount"].as_u64()), (Some("failed"), Some(1)));
    assert!(failed["task"]["lastRunError"].as_str().unwrap().contains("no-such-thread"), "{failed}");

    // Delete answers with the id, twice, and the task is gone.
    assert_eq!(success(&rpc(&mut operator, 19, "scheduledTasks.delete", json!({"id": "scheduled-task:review"})).await), &json!({"id": "scheduled-task:review"}));
    assert_eq!(success(&rpc(&mut operator, 20, "scheduledTasks.delete", json!({"id": "scheduled-task:review"})).await), &json!({"id": "scheduled-task:review"}));
    let left = list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:review").is_null() && tasks.len() == 2).await;
    assert_eq!(left.len(), 2);
    let gone = rpc(&mut operator, 21, "scheduledTasks.runNow", json!({"id": "scheduled-task:review"})).await;
    assert_eq!(failure(&gone)["message"], "Schedule task not found.");

    // Leaving ends the subscription.
    send(&mut watcher, json!({"_tag":"Interrupt","requestId":900})).await;
    loop {
        let message = next(&mut watcher).await;
        if message["_tag"] == "Exit" {
            assert_eq!(message["exit"]["cause"][0]["_tag"], "Interrupt");
            break;
        }
    }
    flow.finish().await;
}

#[tokio::test]
async fn a_session_without_orchestration_operate_can_read_tasks_but_not_their_webhook_urls_or_change_them() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), false).await;
    let mut operator = flow.connect(&flow.operator).await;
    let hook = input(json!({"id": "scheduled-task:hook", "schedule": {"type": "webhook"}}));
    assert!(success(&rpc(&mut operator, 1, "scheduledTasks.upsert", hook).await)["task"]["webhook"].is_object());

    let mut reader = flow.connect(&flow.reader).await;
    let listed = success(&rpc(&mut reader, 1, "scheduledTasks.list", json!({})).await).clone();
    assert_eq!(listed["tasks"][0]["id"], "scheduled-task:hook");
    assert!(listed["tasks"][0].get("webhook").is_none(), "{listed}");
    send(&mut reader, json!({"_tag":"Request","id":2,"tag":"scheduledTasks.subscribe","payload":{},"headers":[]})).await;
    let snapshot = list_where(&mut reader, 2, |_| true).await;
    assert!(snapshot[0].get("webhook").is_none());

    let denied = [
        ("scheduledTasks.upsert", input(json!({}))),
        ("scheduledTasks.setEnabled", json!({"id": "scheduled-task:hook", "enabled": false})),
        ("scheduledTasks.delete", json!({"id": "scheduled-task:hook"})),
        ("scheduledTasks.runNow", json!({"id": "scheduled-task:hook"})),
        ("scheduledTasks.rotateWebhookToken", json!({"id": "scheduled-task:hook"})),
        ("scheduledTasks.listWebhookDeliveries", json!({"id": "scheduled-task:hook"})),
        ("scheduledTasks.getWebhookDelivery", json!({"id": "scheduled-task:hook", "deliveryId": "d"})),
    ];
    for (offset, (tag, payload)) in denied.into_iter().enumerate() {
        let exit = rpc(&mut reader, 10 + offset as u64, tag, payload).await;
        let error = failure(&exit);
        assert_eq!(error["_tag"], "EnvironmentAuthorizationError", "{tag}: {exit}");
        assert_eq!((error["operation"].as_str(), error["requiredPermission"].as_str()), (Some(tag), Some("orchestration:operate")), "{tag}");
    }
    // Nothing was changed by the refused calls.
    let after = success(&rpc(&mut operator, 2, "scheduledTasks.list", json!({})).await).clone();
    assert_eq!(after["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(after["tasks"][0]["enabled"], true);

    // A session with neither scope cannot even list or subscribe.
    let mut outsider = flow.connect(&flow.outsider).await;
    let exit = rpc(&mut outsider, 1, "scheduledTasks.list", json!({})).await;
    assert_eq!((failure(&exit)["_tag"].as_str(), failure(&exit)["requiredPermission"].as_str()), (Some("EnvironmentAuthorizationError"), Some("orchestration:read")));
    let exit = rpc(&mut outsider, 2, "scheduledTasks.subscribe", json!({})).await;
    assert_eq!(failure(&exit)["requiredPermission"], "orchestration:read");
    flow.finish().await;
}

#[tokio::test]
async fn a_task_that_comes_due_launches_exactly_one_run_that_reaches_the_provider() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), true).await;
    let mut operator = flow.connect(&flow.operator).await;
    let mut watcher = flow.connect(&flow.operator).await;
    send(&mut watcher, json!({"_tag":"Request","id":900,"tag":"scheduledTasks.subscribe","payload":{},"headers":[]})).await;
    list_where(&mut watcher, 900, |tasks| tasks.is_empty()).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:tick", "prompt": "Scheduled work"}))).await);
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:tick")["nextRunAt"] == iso(T0 + 5 * MINUTE)).await;

    // Moving the clock to just before the due time launches nothing; to the due time, one run.
    flow.clock.set(T0 + 5 * MINUTE - 1);
    flow.clock.set(T0 + 5 * MINUTE);
    let ran = list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:tick")["runCount"] == 1 && task_of(tasks, "scheduled-task:tick")["lastRunStatus"] == "succeeded").await;
    let task = task_of(&ran, "scheduled-task:tick");
    assert_eq!((task["lastRunAt"].clone(), task["nextRunAt"].clone()), (json!(iso(T0 + 5 * MINUTE)), json!(iso(T0 + 10 * MINUTE))));
    flow.threads_where(|threads| threads.len() == 1 && settled(&threads[0])).await;
    assert_eq!(flow.provider_turns(), ["Scheduled work"]);
    assert_eq!(flow.store.projections("thread").unwrap().len(), 1);

    // The next interval produces the second run and only the second: a launch at the in-between time
    // would have moved the task's next run and shown as a different count.
    flow.clock.set(T0 + 10 * MINUTE - 1);
    flow.clock.set(T0 + 10 * MINUTE);
    let ran = list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:tick")["runCount"] == 2 && task_of(tasks, "scheduled-task:tick")["lastRunStatus"] == "succeeded").await;
    assert_eq!(task_of(&ran, "scheduled-task:tick")["nextRunAt"], iso(T0 + 15 * MINUTE));
    flow.threads_where(|threads| threads.len() == 2 && threads.iter().all(settled)).await;
    assert_eq!(flow.provider_turns(), ["Scheduled work", "Scheduled work"]);
    let history = flow.service.runs("scheduled-task:tick").await.unwrap();
    assert_eq!(history.iter().map(|run| (run.trigger.as_str(), run.status.as_str())).collect::<Vec<_>>(), [("scheduled", "succeeded"), ("scheduled", "succeeded")]);
    let linked: std::collections::BTreeSet<_> = history.iter().map(|run| run.thread_id.clone().unwrap()).collect();
    let created: std::collections::BTreeSet<_> = flow.threads().iter().map(|thread| thread["thread"]["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(linked, created, "each run links to the thread it made");

    // A disabled task does not run when its time passes.
    success(&rpc(&mut operator, 2, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:tick", "enabled": false})).await);
    flow.clock.set(T0 + 2 * HOUR);
    success(&rpc(&mut operator, 3, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:tick", "enabled": true})).await);
    flow.clock.set(T0 + 2 * HOUR + 5 * MINUTE);
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:tick")["runCount"] == 3).await;
    assert_eq!(flow.store.projections("thread").unwrap().len(), 3, "disabling skipped the runs while it was off");
    flow.finish().await;
}

#[tokio::test]
async fn a_run_that_comes_due_while_the_previous_one_is_still_going_overlaps_or_queues() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), true).await;
    let mut operator = flow.connect(&flow.operator).await;
    let mut watcher = flow.connect(&flow.operator).await;
    send(&mut watcher, json!({"_tag":"Request","id":900,"tag":"scheduledTasks.subscribe","payload":{},"headers":[]})).await;
    list_where(&mut watcher, 900, |tasks| tasks.is_empty()).await;
    // The fixture holds a turn open while its prompt says [hold].
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:fresh", "prompt": "Watch things [hold]"}))).await);
    flow.clock.set(T0 + 5 * MINUTE);
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:fresh")["runCount"] == 1).await;
    flow.threads_where(|threads| threads.len() == 1 && threads[0]["runs"][0]["status"] == "running").await;
    // A fresh-thread task starts another thread even though the first is still running.
    flow.clock.set(T0 + 10 * MINUTE);
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:fresh")["runCount"] == 2).await;
    let threads = flow.threads_where(|threads| threads.len() == 2 && threads.iter().all(|thread| thread["runs"][0]["status"] == "running")).await;
    assert_ne!(threads[0]["thread"]["id"], threads[1]["thread"]["id"]);

    // A task bound to a thread queues behind the turn that thread is running. The first task stops here,
    // so the clock moving on runs only the bound one.
    success(&rpc(&mut operator, 3, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:fresh", "enabled": false})).await);
    let bound_thread = threads[0]["thread"]["id"].as_str().unwrap().to_owned();
    success(&rpc(&mut operator, 2, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:bound", "prompt": "Follow up", "threadId": bound_thread, "schedule": {"type": "interval", "everyMs": 600_000}}))).await);
    flow.clock.set(T0 + 20 * MINUTE);
    list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:bound")["runCount"] == 1).await;
    let bound = flow.store.projection("thread", &bound_thread).unwrap().unwrap();
    let runs = bound["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!((runs[0]["status"].as_str(), runs[1]["status"].as_str()), (Some("running"), Some("queued")));
    assert_eq!(flow.threads().len(), 2, "a bound task does not make a thread");
    let history = flow.service.runs("scheduled-task:bound").await.unwrap();
    assert_eq!(history[0].thread_id.as_deref(), Some(bound_thread.as_str()));
    flow.finish().await;
}

#[tokio::test]
async fn a_missed_fixed_time_run_is_skipped_after_a_restart_and_one_inside_the_grace_window_runs() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let nine = T0 - 3 * HOUR; // 09:00Z on the same day
    let clock = ManualClock::new(nine - 6 * HOUR); // 03:00Z
    let first = start(&path, clock.clone(), true).await;
    let mut operator = first.connect(&first.operator).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:morning", "prompt": "Good morning", "schedule": {"type": "fixed_time", "timeOfDay": "09:00"}}))).await);
    success(&rpc(&mut operator, 2, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:late", "prompt": "Slightly late", "schedule": {"type": "fixed_time", "timeOfDay": "09:05"}}))).await);
    drop(operator);
    // The server goes away before either comes due.
    first.finish().await;

    // It returns at 09:08: the 09:00 slot is eight minutes late, inside the ten-minute grace, so both run.
    clock.set(nine + 8 * MINUTE);
    let second = start(&path, clock.clone(), false).await;
    second.service.run_due_tasks().await.unwrap();
    second.threads_where(|threads| threads.len() == 2 && threads.iter().all(settled)).await;
    let mut turns = second.provider_turns();
    turns.sort();
    assert_eq!(turns, ["Good morning", "Slightly late"]);
    let mut operator = second.connect(&second.operator).await;
    let listed = success(&rpc(&mut operator, 1, "scheduledTasks.list", json!({})).await).clone();
    assert_eq!(task_of(listed["tasks"].as_array().unwrap(), "scheduled-task:morning")["nextRunAt"], iso(nine + 24 * HOUR));
    drop(operator);
    second.finish().await;

    // Gone for hours: 15:00 is long past both slots, so nothing runs and the next day is booked.
    let missed_dir = tempfile::tempdir().unwrap();
    let missed_path = std::fs::canonicalize(missed_dir.path()).unwrap();
    let clock = ManualClock::new(nine - 6 * HOUR);
    let before = start(&missed_path, clock.clone(), false).await;
    let mut operator = before.connect(&before.operator).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:morning", "schedule": {"type": "fixed_time", "timeOfDay": "09:00"}}))).await);
    drop(operator);
    before.finish().await;
    clock.set(nine + 6 * HOUR);
    let after = start(&missed_path, clock.clone(), false).await;
    after.service.run_due_tasks().await.unwrap();
    assert!(after.threads().is_empty() && after.provider_turns().is_empty(), "a run six hours late must not fire");
    let mut operator = after.connect(&after.operator).await;
    let listed = success(&rpc(&mut operator, 1, "scheduledTasks.list", json!({})).await).clone();
    let task = task_of(listed["tasks"].as_array().unwrap(), "scheduled-task:morning");
    assert_eq!((task["nextRunAt"].clone(), task["lastRunStatus"].clone(), task["runCount"].clone()), (json!(iso(nine + 24 * HOUR)), json!("never"), json!(0)));
    drop(operator);
    after.finish().await;
}

/// A launcher that never finishes: the process "crashes" while it waits.
struct Stalled {
    launched: tokio::sync::mpsc::UnboundedSender<LaunchRequest>,
    /// Performs the real launch first, then stalls, so the launch is committed but unrecorded.
    real: Option<Arc<ExecutionLauncher>>,
}
impl RunLauncher for Stalled {
    fn launch(&self, request: LaunchRequest) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
        let (launched, real) = (self.launched.clone(), self.real.clone());
        Box::pin(async move {
            if let Some(real) = real {
                real.launch(request.clone()).await?;
            }
            let _ = launched.send(request);
            std::future::pending().await
        })
    }
}

/// Claims a due run and "crashes" (abandons the service) while the launch is in flight.
async fn crash_during_launch(path: &std::path::Path, clock: &Arc<ManualClock>, commit_first: bool) -> Flow {
    let flow = start(path, clock.clone(), false).await;
    let real = commit_first.then(|| ExecutionLauncher::new(flow.execution.clone()));
    let (launched, mut requests) = tokio::sync::mpsc::unbounded_channel();
    flow.service.set_launcher(Arc::new(Stalled { launched, real }));
    let mut operator = flow.connect(&flow.operator).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:crash", "prompt": "Crash test"}))).await);
    clock.set(T0 + 5 * MINUTE);
    let service = flow.service.clone();
    let pass = tokio::spawn(async move { service.run_due_tasks().await });
    requests.recv().await.expect("the launch was never attempted");
    pass.abort();
    let _ = pass.await;
    flow
}

#[tokio::test]
async fn a_restart_between_the_decision_to_run_and_the_launch_does_not_launch_it_afterwards() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let clock = ManualClock::new(T0);
    let crashed = crash_during_launch(&path, &clock, false).await;
    // The claim is durable: the task reads as running, with its run recorded.
    let runs = crashed.service.runs("scheduled-task:crash").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].status.as_str(), runs[0].thread_id.clone(), runs[0].trigger.as_str()), ("running", None, "scheduled"));
    assert!(crashed.threads().is_empty());
    // The restart: the interrupted run is failed, not retried, and the task moves to its next time.
    crashed.crash().await;
    clock.set(T0 + 5 * MINUTE + 30_000);
    let restarted = start(&path, clock.clone(), false).await;
    let runs = restarted.service.runs("scheduled-task:crash").await.unwrap();
    assert_eq!((runs[0].status.as_str(), runs[0].error.as_deref(), runs[0].thread_id.clone()), ("failed", Some("Run was interrupted by a server restart."), None));
    let mut operator = restarted.connect(&restarted.operator).await;
    let listed = success(&rpc(&mut operator, 1, "scheduledTasks.list", json!({})).await).clone();
    let task = task_of(listed["tasks"].as_array().unwrap(), "scheduled-task:crash");
    assert_eq!((task["lastRunStatus"].clone(), task["runCount"].clone(), task["lastRunError"].clone()), (json!("failed"), json!(1), json!("Run was interrupted by a server restart.")));
    assert_eq!(task["nextRunAt"], iso(T0 + 5 * MINUTE + 30_000 + 5 * MINUTE));
    // Passing the original due time again launches nothing.
    restarted.service.run_due_tasks().await.unwrap();
    assert!(restarted.threads().is_empty() && restarted.provider_turns().is_empty());
    drop(operator);
    restarted.finish().await;
}

#[tokio::test]
async fn a_restart_after_the_launch_committed_records_the_run_against_its_thread_and_launches_nothing_more() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let clock = ManualClock::new(T0);
    let crashed = crash_during_launch(&path, &clock, true).await;
    // The thread exists, but the run was never recorded as finished.
    let threads = crashed.threads_where(|threads| threads.len() == 1 && settled(&threads[0])).await;
    let thread_id = threads[0]["thread"]["id"].as_str().unwrap().to_owned();
    let runs = crashed.service.runs("scheduled-task:crash").await.unwrap();
    assert_eq!((runs[0].status.as_str(), runs[0].thread_id.clone()), ("running", None));
    crashed.crash().await;

    clock.set(T0 + 5 * MINUTE + 30_000);
    let restarted = start(&path, clock.clone(), false).await;
    let runs = restarted.service.runs("scheduled-task:crash").await.unwrap();
    assert_eq!((runs[0].status.as_str(), runs[0].error.clone(), runs[0].thread_id.as_deref()), ("succeeded", None, Some(thread_id.as_str())));
    let mut operator = restarted.connect(&restarted.operator).await;
    let listed = success(&rpc(&mut operator, 1, "scheduledTasks.list", json!({})).await).clone();
    let task = task_of(listed["tasks"].as_array().unwrap(), "scheduled-task:crash");
    assert_eq!((task["lastRunStatus"].clone(), task["runCount"].clone(), task["lastRunError"].clone()), (json!("succeeded"), json!(1), Value::Null));
    restarted.service.run_due_tasks().await.unwrap();
    assert_eq!(restarted.threads().len(), 1, "the committed launch is not repeated");
    assert_eq!(restarted.provider_turns(), ["Crash test"]);
    drop(operator);
    restarted.finish().await;
}

#[tokio::test]
async fn deleting_a_task_while_its_run_is_launching_does_not_bring_it_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let clock = ManualClock::new(T0);
    let flow = start(&path, clock.clone(), false).await;
    // A launcher the test releases: the run is claimed, then the task is deleted, then the launch ends.
    struct Gate {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: Arc<tokio::sync::Notify>,
    }
    impl RunLauncher for Gate {
        fn launch(&self, _: LaunchRequest) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
            let (entered, release) = (self.entered.clone(), self.release.clone());
            Box::pin(async move {
                let _ = entered.send(());
                release.notified().await;
                Ok("thread".to_owned())
            })
        }
    }
    let (entered, mut inside) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    flow.service.set_launcher(Arc::new(Gate { entered, release: release.clone() }));
    let mut operator = flow.connect(&flow.operator).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:gone"}))).await);
    clock.set(T0 + 5 * MINUTE);
    let service = flow.service.clone();
    let pass = tokio::spawn(async move { service.run_due_tasks().await });
    inside.recv().await.unwrap();
    // While it launches, a second manual run is refused and the task can be deleted.
    let refused = rpc(&mut operator, 2, "scheduledTasks.runNow", json!({"id": "scheduled-task:gone"})).await;
    assert_eq!(failure(&refused)["message"], "Could not run schedule task.");
    assert_eq!(failure(&refused)["cause"]["message"], "Schedule task is already running.");
    success(&rpc(&mut operator, 3, "scheduledTasks.delete", json!({"id": "scheduled-task:gone"})).await);
    release.notify_one();
    pass.await.unwrap().unwrap();
    assert_eq!(success(&rpc(&mut operator, 4, "scheduledTasks.list", json!({})).await), &json!({"tasks": []}));
    flow.finish().await;
}

fn signed(secret: &str, body: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

#[tokio::test]
async fn a_webhook_request_over_http_starts_one_run_and_is_logged_without_its_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), false).await;
    let mut operator = flow.connect(&flow.operator).await;
    let schedule = json!({"type": "webhook", "signature": {"header": "X-Sig", "encoding": "hex", "prefix": "sha256=", "secret": "s3cret"}});
    let created = success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:hook", "title": "Hook", "prompt": "PR: {{body.pull_request.url}} {{query.ref}}", "schedule": schedule}))).await).clone();
    let path = created["task"]["webhook"]["path"].as_str().unwrap().to_owned();
    let url = format!("http://{}{path}", flow.address);
    let client = reqwest::Client::new();
    let body = r#"{"pull_request":{"url":"https://github.com/org/repo/pull/45"}}"#;

    // A valid signed request is answered at once and starts exactly one run.
    let accepted = client
        .post(format!("{url}?ref=main&token=hush"))
        .header("x-sig", signed("s3cret", body))
        .header("authorization", "Bearer abc")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 202);
    assert_eq!(accepted.headers()["x-t3-hook-outcome"], "accepted");
    let delivery_id = accepted.json::<Value>().await.unwrap()["deliveryId"].as_str().unwrap().to_owned();
    assert!(delivery_id.starts_with("delivery:"), "{delivery_id}");
    flow.service.wait_for_webhook_runs().await;
    flow.threads_where(|threads| threads.len() == 1 && settled(&threads[0])).await;
    assert_eq!(flow.provider_turns(), ["PR: https://github.com/org/repo/pull/45 main"]);
    let history = flow.service.runs("scheduled-task:hook").await.unwrap();
    assert_eq!((history[0].trigger.as_str(), history[0].status.as_str(), history[0].delivery_id.as_deref()), ("webhook", "succeeded", Some(delivery_id.as_str())));
    let listed = success(&rpc(&mut operator, 2, "scheduledTasks.list", json!({})).await).clone();
    assert_eq!((listed["tasks"][0]["lastRunStatus"].clone(), listed["tasks"][0]["runCount"].clone(), listed["tasks"][0]["nextRunAt"].clone()), (json!("succeeded"), json!(1), Value::Null));

    // The delivery log keeps the request with its credentials redacted, and the prompt it produced.
    let deliveries = success(&rpc(&mut operator, 3, "scheduledTasks.listWebhookDeliveries", json!({"id": "scheduled-task:hook"})).await).clone();
    assert_eq!((deliveries["deliveries"][0]["id"].clone(), deliveries["deliveries"][0]["outcome"].clone(), deliveries["deliveries"][0]["signatureVerified"].clone()), (json!(delivery_id), json!("accepted"), json!(true)));
    assert_eq!(deliveries["deliveries"][0]["contentType"], "application/json");
    let detail = success(&rpc(&mut operator, 4, "scheduledTasks.getWebhookDelivery", json!({"id": "scheduled-task:hook", "deliveryId": delivery_id})).await).clone();
    let delivery = &detail["delivery"];
    assert_eq!(delivery["query"], "ref=main&token=[redacted]");
    // Only names that look like credentials are redacted: `x-sig` is not one, `authorization` is.
    assert_eq!((delivery["headers"]["authorization"].clone(), delivery["headers"]["x-sig"].clone()), (json!("[redacted]"), json!(signed("s3cret", body))));
    assert_eq!((delivery["method"].clone(), delivery["body"].clone(), delivery["bodyBytes"].clone(), delivery["bodyTruncated"].clone()), (json!("POST"), json!(body), json!(body.len()), json!(false)));
    assert_eq!(delivery["renderedPrompt"], "PR: https://github.com/org/repo/pull/45 main");

    // Every refusal starts nothing.
    let wrong_token = client.post(format!("http://{}/api/hooks/scheduled-task%3Ahook/wrong", flow.address)).body(body).send().await.unwrap();
    assert_eq!((wrong_token.status().as_u16(), wrong_token.headers()["x-t3-hook-outcome"].to_str().unwrap().to_owned()), (404, "not_found".to_owned()));
    assert_eq!(wrong_token.json::<Value>().await.unwrap(), json!({"error": "hook_not_found"}));
    let unsigned = client.post(&url).body(body).send().await.unwrap();
    assert_eq!((unsigned.status().as_u16(), unsigned.json::<Value>().await.unwrap()), (401, json!({"error": "invalid_signature"})));
    let forged = client.post(&url).header("x-sig", signed("other", body)).body(body).send().await.unwrap();
    assert_eq!(forged.status(), 401);
    let huge = client.post(&url).body(vec![b'x'; 1024 * 1024 + 1]).send().await.unwrap();
    assert_eq!((huge.status().as_u16(), huge.json::<Value>().await.unwrap()), (413, json!({"error": "body_too_large"})));
    assert_eq!(client.delete(&url).send().await.unwrap().status(), 404);
    // The signature is checked over the raw bytes, for any of the four methods.
    let put = client.put(&url).header("x-sig", signed("s3cret", "{}")).body("{}").send().await.unwrap();
    assert_eq!(put.status(), 202);
    flow.service.wait_for_webhook_runs().await;
    assert_eq!(flow.threads().len(), 2, "only the two valid requests ran");

    // A disabled hook answers 409 and the log says so; a flood is cut off at 60 a minute.
    success(&rpc(&mut operator, 5, "scheduledTasks.setEnabled", json!({"id": "scheduled-task:hook", "enabled": false})).await);
    let disabled = client.post(&url).header("x-sig", signed("s3cret", "{}")).body("{}").send().await.unwrap();
    assert_eq!((disabled.status().as_u16(), disabled.json::<Value>().await.unwrap()), (409, json!({"error": "hook_disabled"})));
    let mut last = 0;
    for _ in 0..70 {
        last = client.post(&url).header("x-sig", signed("s3cret", "{}")).body("{}").send().await.unwrap().status().as_u16();
        if last == 429 {
            break;
        }
    }
    assert_eq!(last, 429);
    let log = success(&rpc(&mut operator, 6, "scheduledTasks.listWebhookDeliveries", json!({"id": "scheduled-task:hook"})).await).clone();
    let outcomes: Vec<&str> = log["deliveries"].as_array().unwrap().iter().map(|delivery| delivery["outcome"].as_str().unwrap()).collect();
    assert_eq!(outcomes.first(), Some(&"rate_limited"), "{outcomes:?}");
    assert!(outcomes.len() <= 50, "the log keeps the newest 50");
    flow.finish().await;
}

/// A scheduled run goes through the same launch path whichever provider the task names.
async fn run_through(provider: Provider, model: Value, wait: impl Fn(&Value) -> bool) -> Value {
    let directory = tempfile::tempdir().unwrap();
    let flow = start_with(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), true, provider).await;
    let mut operator = flow.connect(&flow.operator).await;
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:agent", "prompt": "Summarise the repository", "modelSelection": model}))).await);
    flow.clock.set(T0 + 5 * MINUTE);
    let mut watcher = flow.connect(&flow.operator).await;
    send(&mut watcher, json!({"_tag":"Request","id":900,"tag":"scheduledTasks.subscribe","payload":{},"headers":[]})).await;
    let tasks = list_where(&mut watcher, 900, |tasks| task_of(tasks, "scheduled-task:agent")["runCount"] == 1).await;
    assert_eq!(task_of(&tasks, "scheduled-task:agent")["lastRunStatus"], "succeeded", "{tasks:?}");
    let threads = flow.threads_where(|threads| threads.len() == 1 && wait(&threads[0])).await;
    let thread = threads[0].clone();
    assert_eq!(thread["messages"][0]["text"], "Summarise the repository");
    flow.finish().await;
    thread
}

#[tokio::test]
async fn a_scheduled_run_reaches_a_claude_agent() {
    let thread = run_through(Provider::Claude, json!({"instanceId": "claudeAgent", "model": "claude-sonnet-4-6"}), settled).await;
    assert_eq!(thread["runs"][0]["providerInstanceId"], "claudeAgent");
    assert_eq!(thread["runs"][0]["status"], "completed");
}

#[tokio::test]
async fn a_scheduled_run_reaches_an_acp_registry_agent() {
    // The ACP fixture asks for approval of every prompt, which is the proof it received the prompt.
    let thread = run_through(Provider::Acp, json!({"instanceId": "local-agent", "model": "default"}), |thread| {
        thread["runtimeRequests"].as_array().is_some_and(|requests| requests.iter().any(|request| request["status"] == "pending"))
    })
    .await;
    assert_eq!(thread["runs"][0]["providerInstanceId"], "local-agent");
}

#[tokio::test]
async fn a_task_holds_at_most_twenty_webhook_deliveries_and_runs_them_in_arrival_order() {
    use t3_server::{scheduled_tasks::{WebhookTriggerRequest, WebhookTriggerResult}, scheduled_tasks_webhook::WebhookRequest};
    struct Gate {
        order: Arc<std::sync::Mutex<Vec<String>>>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl RunLauncher for Gate {
        fn launch(&self, request: LaunchRequest) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
            let (order, release) = (self.order.clone(), self.release.clone());
            Box::pin(async move {
                order.lock().unwrap().push(request.prompt);
                release.acquire().await.unwrap().forget();
                Ok("thread".to_owned())
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), false).await;
    let (order, release) = (Arc::new(std::sync::Mutex::new(Vec::new())), Arc::new(tokio::sync::Semaphore::new(0)));
    flow.service.set_launcher(Arc::new(Gate { order: order.clone(), release: release.clone() }));
    let mut operator = flow.connect(&flow.operator).await;
    let created = success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:hook", "prompt": "Delivery {{body.n}}", "schedule": {"type": "webhook"}}))).await).clone();
    let token = created["task"]["webhook"]["path"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let trigger = |n: usize| WebhookTriggerRequest {
        hook_id: "scheduled-task:hook".into(),
        token: token.clone(),
        request: WebhookRequest { method: "POST".into(), path: "/api/hooks/scheduled-task:hook".into(), query: String::new(), headers: Default::default(), body_text: format!("{{\"n\":{n}}}") },
        body: format!("{{\"n\":{n}}}").into_bytes(),
        relay_delivery_id: None,
        received_at: None,
    };
    // The first delivery is held inside the launcher; nineteen more wait their turn; the 21st is refused.
    let mut answers = Vec::new();
    for n in 1..=21 {
        answers.push(flow.service.trigger_webhook(trigger(n)).await.unwrap());
    }
    assert!(answers[..20].iter().all(|answer| matches!(answer, WebhookTriggerResult::Accepted { outcome: "accepted", .. })), "{answers:?}");
    assert_eq!(answers[20], WebhookTriggerResult::RateLimited { outcome: "queue_full" });
    release.add_permits(20);
    flow.service.wait_for_webhook_runs().await;
    let expected: Vec<String> = (1..=20).map(|n| format!("Delivery {n}")).collect();
    assert_eq!(*order.lock().unwrap(), expected, "deliveries launch one at a time, in arrival order");
    // The refused request was not logged, and the twenty others were.
    let log = success(&rpc(&mut operator, 2, "scheduledTasks.listWebhookDeliveries", json!({"id": "scheduled-task:hook"})).await).clone();
    assert_eq!(log["deliveries"].as_array().unwrap().len(), 20);
    flow.finish().await;
}

#[tokio::test]
async fn a_run_bound_to_a_thread_queues_a_message_that_names_its_task() {
    let directory = tempfile::tempdir().unwrap();
    let flow = start(&std::fs::canonicalize(directory.path()).unwrap(), ManualClock::new(T0), false).await;
    let mut operator = flow.connect(&flow.operator).await;
    let launched = t3_server::launch::ThreadLaunchService::new(flow.store.clone())
        .launch(json!({"commandId":"bound-launch","projectId":"project","title":"Bound","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap();
    let thread = launched["threadId"].as_str().unwrap().to_owned();
    success(&rpc(&mut operator, 1, "scheduledTasks.upsert", input(json!({"id": "scheduled-task:bound", "threadId": thread, "createdBy": "agent", "creationSource": "mcp"}))).await);
    success(&rpc(&mut operator, 2, "scheduledTasks.runNow", json!({"id": "scheduled-task:bound"})).await);
    let threads = flow.threads_where(|threads| threads.len() == 1 && settled(&threads[0])).await;
    let message = &threads[0]["messages"][0];
    assert_eq!((message["createdBy"].as_str(), message["creationSource"].as_str(), message["scheduledTaskId"].as_str()), (Some("agent"), Some("mcp"), Some("scheduled-task:bound")));
    // The thread itself was made by the user; only the scheduled message is the agent's.
    assert_eq!(threads[0]["thread"]["createdBy"], "user");
    flow.finish().await;
}

/// A secret the user entered for an agent (`request_secret`) signs a webhook once: saving the task
/// consumes the ref, a second save with it fails, a ref entered for another project is refused,
/// and a replay of the first save keeps the secret that save stored.
#[tokio::test]
async fn a_webhook_signing_secret_can_come_from_a_one_use_secret_ref() {
    use t3_server::server_secret_store::ServerSecretStore;
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let flow = start(&base, ManualClock::new(T0), false).await;
    let mut operator = flow.connect(&flow.operator).await;
    let secrets = ServerSecretStore::open(base.join("secrets")).unwrap();
    let keep = |hex: &str, project: &str, saved_at: i64| {
        secrets.create(&format!("secret-request-{hex}"), json!({"projectId":project,"value":"from-the-user","savedAt":saved_at}).to_string().as_bytes()).unwrap();
        format!("secret-ref:{hex}")
    };
    let now = Utc::now().timestamp_millis();
    let (good, foreign, stale) = (keep(&"a".repeat(32), "project", now), keep(&"b".repeat(32), "elsewhere", now), keep(&"c".repeat(32), "project", now - 25 * HOUR));
    let signed = |id: &str, reference: &str, command: Option<&str>| {
        let mut task = input(json!({"id": id, "schedule": {"type": "webhook", "signature": {"header": "X-Sig", "encoding": "hex", "prefix": "sha256=", "secretRef": reference}}}));
        if let Some(command) = command {
            task["commandId"] = json!(command);
        }
        task
    };
    let saved = success(&rpc(&mut operator, 1, "scheduledTasks.upsert", signed("scheduled-task:hook", &good, Some("save-1")).clone()).await).clone();
    assert_eq!(saved["task"]["webhook"]["hasSecret"], true);
    assert!(secrets.get(&format!("secret-request-{}", "a".repeat(32))).unwrap().is_none(), "the ref is used up by the save");
    // The ref works once: a different save with it is refused, a replay of the same save is not.
    let again = rpc(&mut operator, 2, "scheduledTasks.upsert", signed("scheduled-task:second", &good, None)).await;
    assert!(failure(&again)["message"].as_str().unwrap().starts_with("That secretRef was already used or does not exist."), "{again}");
    let replay = rpc(&mut operator, 3, "scheduledTasks.upsert", signed("scheduled-task:hook", &good, Some("save-1"))).await;
    assert_eq!(success(&replay)["task"]["webhook"]["hasSecret"], true);
    // A ref entered for another project, an expired one and a malformed one are each refused.
    let wrong = rpc(&mut operator, 4, "scheduledTasks.upsert", signed("scheduled-task:third", &foreign, None)).await;
    assert!(failure(&wrong)["message"].as_str().unwrap().starts_with("That secretRef was already used or does not exist."));
    assert!(secrets.get(&format!("secret-request-{}", "b".repeat(32))).unwrap().is_some(), "another project's value is left alone");
    let expired = rpc(&mut operator, 5, "scheduledTasks.upsert", signed("scheduled-task:fourth", &stale, None)).await;
    assert!(failure(&expired)["message"].as_str().unwrap().starts_with("That secretRef expired."));
    assert!(secrets.get(&format!("secret-request-{}", "c".repeat(32))).unwrap().is_none(), "an expired value is deleted when it is found");
    let malformed = rpc(&mut operator, 6, "scheduledTasks.upsert", signed("scheduled-task:fifth", "secret-ref:nope", None)).await;
    assert_eq!(failure(&malformed)["message"], "That secretRef is not valid.");
    flow.finish().await;
}
