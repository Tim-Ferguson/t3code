#![cfg(unix)]
//! Background work and queued runs over the real WebSocket transport. Ports the meaningful cases of
//! the original's `BackgroundWorkStop.integration.test.ts` (Stop on a settled run reaches work the
//! turn left running; a plain interrupt does too; Stop holds the queue) and the queued-run commands
//! `queued-run.cancel` and `queued-run.edit`. The ACP side replays the recorded `grok` session the
//! original's integration test uses; the Codex side keeps a turn active with the fixture's hold.
//! Every wait is on a persisted event; nothing here sleeps or polls.
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
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    _directory: tempfile::TempDir,
    address: std::net::SocketAddr,
    store: Store,
    execution: ExecutionService,
    thread: String,
    operator: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

/// `acp` replays the recorded session in `scenario` of `fixtures/acp-replay.json`; otherwise the
/// thread runs on the scripted Codex fixture.
async fn harness(acp: Option<&str>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":cwd}), Utc::now()).unwrap();
    let codex = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let agent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-replay-agent.py");
    let mut acp_args = json!([]);
    if let Some(scenario) = acp {
        let rows: Vec<Value> = serde_json::from_str(include_str!("fixtures/acp-replay.json")).unwrap();
        let row = rows.iter().find(|row| row["name"] == scenario).unwrap();
        let recorded = format!("/tmp/claude-replay-{scenario}");
        let transcript = json!({"scenario":scenario,"entries":serde_json::from_str::<Value>(&row["transcript"].to_string().replace(&recorded, &cwd.to_string_lossy())).unwrap()});
        std::fs::write(cwd.join("transcript.json"), transcript.to_string()).unwrap();
        acp_args = json!([agent, cwd.join("transcript.json"), cwd.join("replay-status.json"), cwd]);
    }
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":acp.is_none(),"config":{"binaryPath":codex,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":cwd.join("codex-requests.jsonl")}]},
        "acpRegistry":{"driver":"acpRegistry","enabled":acp.is_some(),"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":acp_args}}}}))
    .unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let instance = if acp.is_some() { "acpRegistry" } else { "codex" };
    let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":instance,"model":if acp.is_some(){"default"}else{"fixture-model"}},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_background".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"background-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    Harness { _directory: directory, address, store, execution, thread, operator, stop: Some(stop), server: Some(server) }
}

impl Harness {
    async fn connect(&self) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {}", self.operator).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    /// Waits on committed events until `check` finds something in the thread projection.
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a persisted event: {}", self.view()["runs"]))
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
    loop {
        let value: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}
async fn command(socket: &mut Socket, id: u64, command: Value) -> Value {
    rpc(socket, id, "orchestration.dispatchCommand", command).await
}
fn accepted(exit: &Value) {
    assert_eq!(exit["_tag"], "Success", "{exit}");
}
fn refused(exit: &Value) -> String {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    exit["cause"][0]["error"]["message"].as_str().or_else(|| exit["cause"][0]["message"].as_str()).unwrap_or_default().to_owned()
}
fn message(thread: &str, id: &str, text: &str, mode: Value) -> Value {
    json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":mode})
}
fn pending(view: &Value) -> Vec<Value> {
    t3_server::thread::shell(view)["pendingBackgroundTasks"].as_array().unwrap().clone()
}
const SETTLED: [&str; 4] = ["completed", "failed", "interrupted", "cancelled"];

/// A prompt that failed with a tool still running leaves that tool as pending background work;
/// Stop on the failed run ends it, holds nothing back that was not queued, and a second Stop has
/// nothing left to reach.
#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_the_work_a_failed_turn_left_running() {
    let harness = harness(Some("stop_background_work_after_failed_turn")).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "start", "Start the dev server in the background with `bun run dev` and keep it running.", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| view["runs"][0]["status"].as_str().filter(|status| SETTLED.contains(status)).map(|_| view.clone())).await;
    assert_eq!(view["runs"][0]["status"], "failed");
    let work = pending(&view);
    assert_eq!(work.len(), 1, "{view}");
    assert_eq!((work[0]["kind"].as_str(), work[0]["description"].as_str()), (Some("command"), Some("Ran command")));

    let run = view["runs"][0]["id"].clone();
    accepted(&command(&mut socket, 2, json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run,"holdQueue":true})).await);
    let view = harness.until(|view| (pending(view).is_empty() && view["turnItems"].as_array().unwrap().iter().all(|item| item["type"] != "command_execution" || item["status"] != "running")).then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!(item["status"], "interrupted");
    assert!(item["completedAt"].is_string());
    assert_eq!(view["runs"][0]["status"], "failed", "Stop settles the work, not the settled run");
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "run_interrupt_request"));

    // Nothing is left to reach, so the same Stop on another command is refused.
    let again = command(&mut socket, 3, json!({"type":"run.interrupt","commandId":"stop-again","threadId":harness.thread,"runId":run})).await;
    assert_eq!(refused(&again), "Run is no longer active.");
    harness.finish().await;
}

/// A run still queued behind the active one can be cancelled without ever starting, and cancelling
/// a run that is not queued is refused with a recorded rejection.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_run_is_cancelled_without_starting() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[hold] first", json!({"type":"start_immediately"}))).await);
    harness.until(|view| (view["runs"][0]["status"] == "running").then_some(())).await;
    accepted(&command(&mut socket, 2, message(&harness.thread, "second", "second", json!({"type":"queue_after_active"}))).await);
    let queued = harness.until(|view| view["runs"].get(1).filter(|run| run["status"] == "queued").cloned()).await;

    accepted(&command(&mut socket, 3, json!({"type":"queued-run.cancel","commandId":"cancel","threadId":harness.thread,"runId":queued["id"]})).await);
    let view = harness.view();
    let run = &view["runs"][1];
    assert_eq!((run["status"].as_str(), run["queuePosition"].clone(), run["startedAt"].clone()), (Some("cancelled"), Value::Null, Value::Null));
    assert!(run["completedAt"].is_string());
    let attempt = view["attempts"].as_array().unwrap().iter().find(|attempt| attempt["id"] == run["activeAttemptId"]).unwrap();
    let node = view["nodes"].as_array().unwrap().iter().find(|node| node["id"] == run["rootNodeId"]).unwrap();
    assert_eq!((attempt["status"].as_str(), node["status"].as_str()), (Some("cancelled"), Some("cancelled")));
    assert_eq!(view["runs"][0]["status"], "running", "the active run is untouched");
    let shell = t3_server::thread::shell(&view);
    assert_eq!((shell["latestRunId"].clone(), shell["activeRunId"].clone()), (run["id"].clone(), view["runs"][0]["id"].clone()));

    let again = command(&mut socket, 4, json!({"type":"queued-run.cancel","commandId":"cancel-twice","threadId":harness.thread,"runId":queued["id"]})).await;
    assert!(refused(&again).contains("is not queued"), "{again}");
    let active = view["runs"][0]["id"].clone();
    accepted(&command(&mut socket, 5, json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":active})).await);
    harness.until(|view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    harness.finish().await;
}

/// Editing a queued run replaces what it will send, in the message and in its transcript item;
/// an empty edit, an edit of a run that is not queued, and a repeat of the same command id are
/// refused or replayed like the original's.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_message_is_edited_in_place() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[hold] first", json!({"type":"start_immediately"}))).await);
    harness.until(|view| (view["runs"][0]["status"] == "running").then_some(())).await;
    accepted(&command(&mut socket, 2, message(&harness.thread, "second", "second", json!({"type":"queue_after_active"}))).await);
    let queued = harness.until(|view| view["runs"].get(1).filter(|run| run["status"] == "queued").cloned()).await;

    let empty = command(&mut socket, 3, json!({"type":"queued-run.edit","commandId":"edit-empty","threadId":harness.thread,"runId":queued["id"],"text":"   "})).await;
    assert!(refused(&empty).contains("cannot be edited to an empty message"), "{empty}");
    let active = command(&mut socket, 4, json!({"type":"queued-run.edit","commandId":"edit-active","threadId":harness.thread,"runId":harness.view()["runs"][0]["id"],"text":"nope"})).await;
    assert!(refused(&active).contains("is not queued"), "{active}");

    let edit = json!({"type":"queued-run.edit","commandId":"edit","threadId":harness.thread,"runId":queued["id"],"text":"second, edited","context":null});
    accepted(&command(&mut socket, 5, edit.clone()).await);
    let view = harness.view();
    let message = view["messages"].as_array().unwrap().iter().find(|message| message["id"] == queued["userMessageId"]).unwrap();
    assert_eq!(message["text"], "second, edited");
    // A queued message has no transcript item yet; the run writes it from the edited message.
    assert!(view["turnItems"].as_array().unwrap().iter().all(|item| item["messageId"] != queued["userMessageId"]));
    assert_eq!(view["runs"][1]["status"], "queued");
    let sequence = view["updatedAt"].clone();
    accepted(&command(&mut socket, 6, edit).await);
    assert_eq!(harness.view()["updatedAt"], sequence, "a repeated command id changes nothing");

    // The edited message is what the provider receives once the active run ends.
    let active = harness.view()["runs"][0]["id"].clone();
    accepted(&command(&mut socket, 7, json!({"type":"run.interrupt","commandId":"stop-first","threadId":harness.thread,"runId":active})).await);
    let view = harness.until(|view| view["runs"].get(1).filter(|run| SETTLED.contains(&run["status"].as_str().unwrap_or_default())).map(|_| view.clone())).await;
    let assistant = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "assistant_message" && item["runId"] == queued["id"]).unwrap_or_else(|| panic!("{}", view["turnItems"]));
    assert!(assistant["text"].as_str().unwrap().contains("second, edited"), "{assistant}");
    let sent = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_message" && item["messageId"] == queued["userMessageId"]).unwrap();
    assert_eq!(sent["text"], "second, edited");
    harness.finish().await;
}

/// Queued messages are reordered ahead of one another or to the end; a refusal names why; a stopped
/// run can hold the queue, and resuming it starts the next message.
#[tokio::test(flavor = "multi_thread")]
async fn queued_messages_are_reordered_and_a_held_queue_is_resumed() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[hold] first", json!({"type":"start_immediately"}))).await);
    harness.until(|view| (view["runs"][0]["status"] == "running").then_some(())).await;
    for (index, name) in ["second", "third", "fourth"].into_iter().enumerate() {
        accepted(&command(&mut socket, 2 + index as u64, message(&harness.thread, name, name, json!({"type":"queue_after_active"}))).await);
        harness.until(|view| view["runs"].get(index + 1).filter(|run| run["status"] == "queued").map(|_| ())).await;
    }
    let order = |view: &Value| {
        let mut queued: Vec<&Value> = view["runs"].as_array().unwrap().iter().filter(|run| run["status"] == "queued").collect();
        queued.sort_by_key(|run| run["queuePosition"].as_u64());
        queued.iter().map(|run| view["messages"].as_array().unwrap().iter().find(|message| message["id"] == run["userMessageId"]).unwrap()["text"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(order(&harness.view()), ["second", "third", "fourth"]);
    let runs = harness.view()["runs"].clone();
    let (second, fourth) = (runs[1]["id"].clone(), runs[3]["id"].clone());

    accepted(&command(&mut socket, 10, json!({"type":"queued-run.reorder","commandId":"ahead","threadId":harness.thread,"runId":fourth,"beforeRunId":second})).await);
    assert_eq!(order(&harness.view()), ["fourth", "second", "third"]);
    accepted(&command(&mut socket, 11, json!({"type":"queued-run.reorder","commandId":"to-end","threadId":harness.thread,"runId":fourth,"beforeRunId":null})).await);
    assert_eq!(order(&harness.view()), ["second", "third", "fourth"]);
    let active = runs[0]["id"].clone();
    let not_queued = command(&mut socket, 12, json!({"type":"queued-run.reorder","commandId":"active","threadId":harness.thread,"runId":active,"beforeRunId":second})).await;
    assert!(refused(&not_queued).contains("is not queued"), "{not_queued}");
    let no_target = command(&mut socket, 13, json!({"type":"queued-run.reorder","commandId":"no-target","threadId":harness.thread,"runId":fourth,"beforeRunId":active})).await;
    assert!(refused(&no_target).contains("is not queued"), "{no_target}");
    assert_eq!(order(&harness.view()), ["second", "third", "fourth"], "a refused reorder moves nothing");

    // Stop holds the queue; resume releases it and the next message runs.
    accepted(&command(&mut socket, 14, json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":active,"holdQueue":true})).await);
    let view = harness.until(|view| (view["runs"][0]["status"] == "interrupted").then(|| view.clone())).await;
    assert!(view["runs"].as_array().unwrap().iter().filter(|run| run["status"] == "queued").all(|run| run["queueHeld"] == true));
    accepted(&command(&mut socket, 15, json!({"type":"queue.resume","commandId":"resume","threadId":harness.thread})).await);
    let view = harness.until(|view| view["runs"].as_array().unwrap().iter().all(|run| SETTLED.contains(&run["status"].as_str().unwrap_or_default())).then(|| view.clone())).await;
    assert_eq!(view["runs"].as_array().unwrap().iter().map(|run| run["status"].as_str().unwrap()).collect::<Vec<_>>(), ["interrupted", "completed", "completed", "completed"]);
    harness.finish().await;
}

fn provider_requests(harness: &Harness, method: &str) -> usize {
    std::fs::read_to_string(harness._directory.path().join("codex-requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).unwrap()["method"] == method)
        .count()
}

/// A command still running when its turn completes is background work: the run completes, the
/// command stays running and is listed as pending, and Codex's later report of its end updates the
/// same item without a run being active.
#[tokio::test(flavor = "multi_thread")]
async fn a_command_a_completed_turn_left_running_reports_when_it_ends() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[background] start the server", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| (view["runs"][0]["status"] == "completed").then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!((item["status"].as_str(), item["input"].as_str()), (Some("running"), Some("bun run dev")));
    let work = pending(&view);
    assert_eq!((work.len(), work[0]["kind"].as_str()), (1, Some("command")), "{view}");

    // The next turn's start makes the fixture report the earlier command's end.
    accepted(&command(&mut socket, 2, message(&harness.thread, "second", "[finish-background] go on", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| (view["runs"][1]["status"] == "completed" && view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "command_execution" && item["status"] == "completed")).then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["input"] == "bun run dev").unwrap();
    assert_eq!((item["exitCode"].as_i64(), item["output"].as_str()), (Some(0), Some("ready\ndone\n")));
    assert!(pending(&view).is_empty());
    harness.finish().await;
}

/// A tool its input marks as persistent outlives the turn that started it: the run completes, the
/// tool stays running without being listed as pending work, and its end updates the same item.
#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_tool_a_completed_turn_left_running_reports_when_it_ends() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[persistent-tool] watch", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| (view["runs"][0]["status"] == "completed").then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "dynamic_tool").unwrap();
    assert_eq!((item["status"].as_str(), item["toolName"].as_str()), (Some("running"), Some("watch")));
    assert!(pending(&view).is_empty(), "a persistent tool is not pending work: {:?}", pending(&view));

    accepted(&command(&mut socket, 2, message(&harness.thread, "second", "[finish-background] go on", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| (view["runs"][1]["status"] == "completed" && view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "dynamic_tool" && item["status"] == "completed")).then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "dynamic_tool").unwrap();
    assert_eq!(item["output"], json!([{"type":"inputText","text":"watching"}]));
    harness.finish().await;
}

/// Stop on a completed run reaches the command it left running: Codex is told to terminate the
/// terminal, the item ends as interrupted, and the work is no longer pending.
#[tokio::test(flavor = "multi_thread")]
async fn stop_terminates_the_terminal_of_a_command_a_completed_turn_left_running() {
    let harness = harness(None).await;
    let mut socket = harness.connect().await;
    accepted(&command(&mut socket, 1, message(&harness.thread, "first", "[background] start the server", json!({"type":"start_immediately"}))).await);
    let view = harness.until(|view| (view["runs"][0]["status"] == "completed" && !pending(view).is_empty()).then(|| view.clone())).await;
    let run = view["runs"][0]["id"].clone();
    accepted(&command(&mut socket, 2, json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run,"holdQueue":true})).await);
    let view = harness.until(|view| pending(view).is_empty().then(|| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!(item["status"], "interrupted");
    assert_eq!(provider_requests(&harness, "thread/backgroundTerminals/terminate"), 1);
    assert_eq!(view["runs"][0]["status"], "completed");
    harness.finish().await;
}
