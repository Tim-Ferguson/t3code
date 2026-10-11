#![cfg(unix)]
//! Thread forks, merge-backs and context handoffs end to end through the real WebSocket transport,
//! against the scripted Codex app-server (`fixtures/codex-provider.py`) in a temporary git
//! repository. The recorded sessions of the original are replayed by `codex_replay.rs`; these cover
//! what no recording does: independent continuation of two forks, a fork into a new worktree,
//! refusals, a portable fork of an interrupted run, a source that was archived or deleted, and
//! checkpoints on a fork. Tests wait on committed events, never on sleeps.
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
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    directory: tempfile::TempDir,
    address: std::net::SocketAddr,
    store: Store,
    execution: ExecutionService,
    record: std::path::PathBuf,
    /// What the second Codex instance, `codex-b`, was asked.
    record_b: std::path::PathBuf,
    /// The prompts the registry agent `local-agent` was given.
    acp_prompts: std::path::PathBuf,
    operator: String,
    reader: String,
    socket: Option<Socket>,
    next: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

fn git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// The fake `claude`, which answers every prompt with "Working on: " and the prompt it was given.
fn claude_shim(cwd: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-provider.py");
    let binary = cwd.join("claude");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

async fn fixture(environment: &[(&str, &str)]) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-q", "-m", "initial"]);
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now())
        .unwrap();
    let record = cwd.join("codex-requests.jsonl");
    let record_b = cwd.join("codex-b-requests.jsonl");
    let acp_prompts = cwd.join("acp-prompts.log");
    let acp = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let mut provider_environment = vec![json!({"name":"FIXTURE_RPC_RECORD","value":record})];
    provider_environment.extend(environment.iter().map(|(name, value)| json!({"name":name,"value":value})));
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":true,"config":{"binaryPath":claude_shim(&cwd),"homePath":cwd.join("claude-home")},"environment":[{"name":"CLAUDE_FIXTURE_SCENARIO","value":"turn"},{"name":"CLAUDE_FIXTURE_LOG","value":cwd.join("claude-launches.jsonl")}]},
        "claude-b":{"driver":"claudeAgent","enabled":true,"config":{"binaryPath":claude_shim(&cwd),"homePath":cwd.join("claude-home")},"environment":[{"name":"CLAUDE_FIXTURE_SCENARIO","value":"turn"},{"name":"CLAUDE_FIXTURE_LOG","value":cwd.join("claude-b-launches.jsonl")}]},
        "claude-c":{"driver":"claudeAgent","enabled":true,"config":{"binaryPath":claude_shim(&cwd),"homePath":cwd.join("claude-c-home")},"environment":[{"name":"CLAUDE_FIXTURE_SCENARIO","value":"turn"},{"name":"CLAUDE_FIXTURE_LOG","value":cwd.join("claude-c-launches.jsonl")}]},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":provider_environment},
        "local-agent":{"driver":"acpRegistry","enabled":true,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[acp,"2","normal",acp_prompts]}},
        "codex-b":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-b-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record_b}]},
        "codex-same-home":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":cwd.join("codex-same-home-requests.jsonl")}]}}}))
    .unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    ThreadLaunchService::with_providers(store.clone(), providers.clone())
        .launch(json!({"commandId":"launch","threadId":"source","projectId":"project","title":"Source","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_fork".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"fork-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    let mut fixture = Fixture { directory, address, store, execution, record, record_b, acp_prompts, operator, reader, socket: None, next: 0, stop: Some(stop), server: Some(server) };
    fixture.socket = Some(fixture.connect(&fixture.operator.clone()).await);
    fixture
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}

impl Fixture {
    async fn connect(&self, token: &str) -> Socket {
        connect(self.address, token).await
    }
    /// One command over the operator's socket; the exit as the client sees it.
    async fn exit(&mut self, mut command: Value) -> Value {
        // A client names who made a fork or a merge-back and where from.
        if matches!(command["type"].as_str(), Some("thread.fork" | "thread.merge_back")) {
            command["createdBy"] = json!("user");
            command["creationSource"] = json!("web");
        }
        self.next += 1;
        let id = self.next;
        rpc(self.socket.as_mut().unwrap(), id, "orchestration.dispatchCommand", command).await
    }
    async fn command(&mut self, command: Value) {
        let exit = self.exit(command).await;
        assert_eq!(exit["_tag"], "Success", "{exit}");
    }
    /// The admission error text of a refused command.
    async fn refusal(&mut self, command: Value) -> String {
        let exit = self.exit(command).await;
        assert_eq!(exit["_tag"], "Failure", "{exit}");
        let error = &exit["cause"][0]["error"];
        assert_eq!(error["_tag"], "OrchestrationV2DispatchCommandError", "{exit}");
        error["message"].as_str().unwrap().to_owned()
    }
    fn view(&self, thread: &str) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", thread).unwrap().unwrap_or_else(|| panic!("no thread {thread}")))
    }
    async fn until(&self, thread: &str, mut check: impl FnMut(&Value) -> bool) -> Value {
        let mut events = self.store.subscribe();
        let mut last = Value::Null;
        let waited = tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(view) = self.store.projection("thread", thread).unwrap() {
                    last = json!({"thread":view["thread"],"runs":view["runs"]});
                    if check(&view) {
                        return view;
                    }
                }
                let _ = events.recv().await;
            }
        })
        .await;
        waited.unwrap_or_else(|_| panic!("{thread} never reached the wanted state: {last}"))
    }
    /// Sends one message and waits for the run it starts to end.
    async fn say(&mut self, thread: &str, text: &str) -> Value {
        self.say_with(thread, text, json!({"type":"start_immediately"})).await
    }
    async fn say_with(&mut self, thread: &str, text: &str, mode: Value) -> Value {
        let wanted = self.view(thread)["runs"].as_array().unwrap().len() + 1;
        let id = format!("say-{}", self.next + 1);
        self.command(json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":mode})).await;
        self.settled(thread, wanted).await
    }
    async fn settled(&self, thread: &str, runs: usize) -> Value {
        self.until(thread, |view| {
            let rows = view["runs"].as_array().unwrap();
            rows.len() >= runs && rows.iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back")))
        })
        .await
    }
    fn requests(&self, method: &str) -> Vec<Value> {
        self.requests_in(&self.record, method)
    }
    fn requests_in(&self, record: &std::path::Path, method: &str) -> Vec<Value> {
        std::fs::read_to_string(record)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == method)
            .map(|record| record["params"].clone())
            .collect()
    }
    async fn fork(&mut self, id: &str, source: &str, target: &str, point: Value) {
        self.command(json!({"type":"thread.fork","commandId":id,"sourceThreadId":source,"targetThreadId":target,"sourcePoint":point,"title":format!("{target} title")})).await;
    }
    async fn finish(mut self) {
        self.socket = None;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

fn native_thread(view: &Value) -> String {
    view["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned()
}
fn native_turn(view: &Value, run: usize) -> String {
    let attempt = view["runs"][run]["activeAttemptId"].clone();
    view["providerTurns"].as_array().unwrap().iter().find(|turn| turn["runAttemptId"] == attempt).unwrap()["nativeTurnRef"]["nativeId"].as_str().unwrap().to_owned()
}
fn run_point(view: &Value, run: usize) -> Value {
    json!({"type":"run","runId":view["runs"][run]["id"]})
}
/// What a client shows of a thread: visibility and the text of the messages.
fn shown(view: &Value) -> Vec<String> {
    view["visibleTurnItems"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| {
            let item = &row["item"];
            match item["type"].as_str().unwrap() {
                "user_message" | "assistant_message" => Some(format!("{}:{}", row["visibility"].as_str().unwrap(), item["text"].as_str().unwrap())),
                "fork" | "handoff" => Some(format!("{}:{}", row["visibility"].as_str().unwrap(), item["type"].as_str().unwrap())),
                _ => None,
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn forks_from_the_latest_and_a_prior_turn_continue_independently() {
    let mut fixture = fixture(&[]).await;
    let first = fixture.say("source", "alpha").await;
    let both = fixture.say("source", "beta").await;
    assert_eq!(first["runs"].as_array().unwrap().len(), 1);
    let source_native = native_thread(&both);
    // The newest completed run that has a checkpoint is the latest stable point.
    assert!(!both["runs"][1]["checkpointId"].is_null());

    fixture.fork("fork-latest", "source", "latest", json!({"type":"latest_stable"})).await;
    fixture.fork("fork-prior", "source", "prior", json!({"type":"run","runId":both["runs"][0]["id"]})).await;
    // The same command answers with its first receipt.
    fixture.fork("fork-prior", "source", "prior", json!({"type":"run","runId":both["runs"][0]["id"]})).await;

    // Lineage, the pending transfer on both threads and what each shows before any message.
    for (name, run) in [("latest", 1), ("prior", 0)] {
        let view = fixture.view(name);
        assert_eq!(view["thread"]["lineage"], json!({"parentThreadId":"source","relationshipToParent":"fork","rootThreadId":"source"}), "{name}");
        assert_eq!(view["thread"]["forkedFrom"], json!({"type":"run","threadId":"source","runId":both["runs"][run]["id"]}), "{name}");
        assert_eq!(view["thread"]["title"], format!("{name} title"));
        assert!(view["thread"]["activeProviderThreadId"].is_null());
        assert_eq!(view["contextTransfers"][0]["status"], "pending");
        assert_eq!(view["contextTransfers"][0]["type"], "fork");
        assert!(view["runs"].as_array().unwrap().is_empty());
    }
    assert_eq!(shown(&fixture.view("prior")), ["inherited:alpha", "inherited:Fixture response: alpha", "synthetic:fork"]);
    assert_eq!(shown(&fixture.view("latest")), ["inherited:alpha", "inherited:Fixture response: alpha", "inherited:beta", "inherited:Fixture response: beta", "synthetic:fork"]);
    let source = fixture.view("source");
    assert_eq!(source["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "fork").count(), 2, "the source lists both forks");
    let shell = t3_server::thread::ThreadService::new(fixture.store.clone()).shell_snapshot().unwrap();
    let listed = shell["threads"].as_array().unwrap().iter().find(|thread| thread["id"] == "prior").unwrap();
    assert_eq!(listed["lineage"]["relationshipToParent"], "fork");
    assert_eq!(listed["forkedFrom"]["type"], "run");

    // Both forks and the source go on independently.
    let latest = fixture.say("latest", "gamma").await;
    let prior = fixture.say("prior", "delta").await;
    let source = fixture.say("source", "epsilon").await;
    let forks = fixture.requests("thread/fork");
    assert_eq!(forks.len(), 2);
    let by_cut = |turn: String| forks.iter().find(|params| params["lastTurnId"] == turn.as_str()).unwrap_or_else(|| panic!("no fork cut at {turn}: {forks:?}"));
    assert_eq!(by_cut(native_turn(&both, 1))["threadId"], source_native.as_str());
    assert_eq!(by_cut(native_turn(&both, 0))["threadId"], source_native.as_str());
    let (latest_native, prior_native) = (native_thread(&latest), native_thread(&prior));
    assert!(latest_native != source_native && prior_native != source_native && latest_native != prior_native);
    for view in [&latest, &prior] {
        assert_eq!(view["contextTransfers"][0]["status"], "consumed");
        assert_eq!(view["contextTransfers"][0]["resolution"]["strategy"], "native_fork");
        assert_eq!(view["providerThreads"][0]["forkedFrom"]["providerThreadId"], both["providerThreads"][0]["id"]);
    }
    let turn_starts = fixture.requests("turn/start");
    let thread_of = |text: &str| turn_starts.iter().find(|params| params["input"][0]["text"].as_str().unwrap().contains(text)).unwrap()["threadId"].clone();
    assert_eq!(thread_of("gamma"), latest_native.as_str());
    assert_eq!(thread_of("delta"), prior_native.as_str());
    assert_eq!(thread_of("epsilon"), source_native.as_str());
    assert_eq!(source["runs"].as_array().unwrap().len(), 3);
    // Each thread keeps its own history: the fork shows the source through its fork point only.
    assert_eq!(shown(&prior), ["inherited:alpha", "inherited:Fixture response: alpha", "synthetic:fork", "local:delta", "local:Fixture response: delta"]);
    assert!(!shown(&latest).iter().any(|row| row.contains("delta") || row.contains("epsilon")));
    assert!(!shown(&source).iter().any(|row| row.contains("gamma") || row.contains("delta") || row.contains("fork")));
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_into_a_new_worktree_runs_there_and_leaves_the_source_where_it_was() {
    let mut fixture = fixture(&[]).await;
    let source = fixture.say("source", "alpha").await;
    fixture.fork("fork", "source", "tree", json!({"type":"run","runId":source["runs"][0]["id"]})).await;
    // The fork starts where its source was; the first message may ask for a workspace of its own.
    assert_eq!(fixture.view("tree")["thread"]["worktreePath"], source["thread"]["worktreePath"]);
    let view = fixture.say_with("tree", "beta", json!({"type":"defer_start","workspaceStrategy":{"type":"worktree","baseRef":"main"}})).await;
    let path = view["thread"]["worktreePath"].as_str().expect("the fork has a worktree");
    assert!(std::path::Path::new(path).join("README.md").exists());
    assert!(source["thread"]["worktreePath"].is_null());
    assert!(fixture.view("source")["thread"]["worktreePath"].is_null(), "the source keeps its workspace");
    let forks = fixture.requests("thread/fork");
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["cwd"], path, "the native fork runs in the new worktree");
    assert_eq!(view["runs"][0]["status"], "completed");
    assert_eq!(view["contextTransfers"][0]["status"], "consumed");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_is_refused_for_the_reasons_the_original_refuses() {
    let mut fixture = fixture(&[]).await;
    let done = fixture.say("source", "alpha").await;
    let finished = done["runs"][0]["id"].clone();
    let fork = |id: &str, source: &str, target: &str, point: Value| json!({"type":"thread.fork","commandId":id,"sourceThreadId":source,"targetThreadId":target,"sourcePoint":point});

    assert_eq!(fixture.refusal(fork("no-source", "missing", "t1", json!({"type":"latest_stable"}))).await, "Thread missing does not exist.");
    assert_eq!(fixture.refusal(fork("no-run", "source", "t2", json!({"type":"run","runId":"run:nope"}))).await, "No stable source run was found for fork source run.");
    assert_eq!(fixture.refusal(fork("no-checkpoint", "source", "t3", json!({"type":"checkpoint","checkpointId":"nope"}))).await, "No stable source run was found for fork source checkpoint.");
    assert_eq!(fixture.refusal(fork("exists", "source", "source", json!({"type":"run","runId":finished}))).await, "Thread source already exists.");

    // A run that is still going cannot be forked, and neither can one that was rolled back.
    fixture.command(json!({"type":"message.dispatch","commandId":"hold","threadId":"source","messageId":"hold","text":"work [hold]","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let running = fixture.until("source", |view| view["runs"][1]["status"] == "running").await;
    let run = running["runs"][1]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        fixture.refusal(fork("running", "source", "t4", json!({"type":"run","runId":run}))).await,
        format!("Fork source run {run} is running; in-progress and rolled-back runs cannot be forked.")
    );
    // Only a fork can be merged back, into the thread it came from.
    fixture.fork("ok", "source", "child", json!({"type":"run","runId":finished})).await;
    assert_eq!(
        fixture.refusal(json!({"type":"thread.merge_back","commandId":"wrong-way","sourceThreadId":"source","targetThreadId":"child","sourcePoint":{"type":"run","runId":finished}})).await,
        "Thread source is not a fork of child."
    );
    assert_eq!(
        fixture.refusal(json!({"type":"thread.merge_back","commandId":"no-fork-run","sourceThreadId":"child","targetThreadId":"source","sourcePoint":{"type":"latest_stable"}})).await,
        "No stable source run was found for merge-back source latest_stable."
    );
    // A connection without orchestration:operate may not fork at all.
    let mut reader = fixture.connect(&fixture.reader.clone()).await;
    let denied = rpc(&mut reader, 1, "orchestration.dispatchCommand", fork("reader", "source", "t5", json!({"type":"run","runId":finished}))).await;
    assert_eq!(denied["cause"][0]["error"]["_tag"], "EnvironmentAuthorizationError", "{denied}");
    assert_eq!(denied["cause"][0]["error"]["requiredPermission"], "orchestration:operate");
    for refused in ["t1", "t2", "t3", "t4", "t5"] {
        assert!(fixture.store.projection("thread", refused).unwrap().is_none(), "{refused} was created");
    }
    assert!(fixture.store.receipt("reader").unwrap().is_none());
    fixture.command(json!({"type":"run.interrupt","commandId":"release","threadId":"source","runId":run})).await;
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn merging_a_fork_back_gives_its_work_to_the_source_on_its_next_message() {
    let mut fixture = fixture(&[]).await;
    let source = fixture.say("source", "alpha").await;
    let point = json!({"type":"run","runId":source["runs"][0]["id"]});
    fixture.fork("fork", "source", "child", point.clone()).await;
    let child = fixture.say("child", "remember the marker").await;
    let child_run = json!({"type":"run","runId":child["runs"][0]["id"]});
    fixture.command(json!({"type":"thread.merge_back","commandId":"merge","sourceThreadId":"child","targetThreadId":"source","sourcePoint":child_run})).await;
    // Pending on the thread it will join, and listed by the fork too.
    let waiting = fixture.view("source");
    let merge = waiting["contextTransfers"].as_array().unwrap().iter().find(|transfer| transfer["type"] == "merge_back").unwrap().clone();
    assert_eq!((merge["status"].as_str(), merge["sourceThreadId"].as_str(), merge["targetThreadId"].as_str()), (Some("pending"), Some("child"), Some("source")));
    assert_eq!(merge["basePoint"]["runId"], source["runs"][0]["id"], "the fork's base is the point it was taken at");
    assert!(fixture.view("child")["contextTransfers"].as_array().unwrap().iter().any(|transfer| transfer["id"] == merge["id"]));
    assert!(fixture.requests("thread/inject_items").is_empty(), "nothing reaches the provider before the next message");

    let after = fixture.say("source", "continue").await;
    let transfer = after["contextTransfers"].as_array().unwrap().iter().find(|transfer| transfer["id"] == merge["id"]).unwrap();
    assert_eq!(transfer["status"], "consumed");
    assert_eq!(transfer["resolution"]["strategy"], "fork_delta_context");
    let handoff = &after["contextHandoffs"][0];
    assert_eq!(handoff["strategy"], "fork_delta_summary");
    assert_eq!(handoff["delivery"]["status"], "injected");
    assert_eq!(after["runs"][1]["contextHandoffId"], handoff["id"]);
    let card = after["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "handoff").unwrap();
    assert_eq!((card["title"].as_str(), card["contextHandoffId"].clone()), (Some("Merge-back context"), handoff["id"].clone()));
    assert!(card["summary"].as_str().unwrap().contains("- User: remember the marker"));
    // The source's native thread took the fork's work as history, and the user's message untouched.
    let injected = fixture.requests("thread/inject_items");
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0]["threadId"], native_thread(&after).as_str());
    let items = injected[0]["items"].as_array().unwrap();
    assert!(items[0]["content"][0]["text"].as_str().unwrap().contains("Provider context handoff. Thread: child."));
    assert!(items.iter().any(|item| item["content"][0]["text"].as_str().unwrap().contains("remember the marker")));
    let sent = fixture.requests("turn/start");
    assert_eq!(sent.last().unwrap()["input"][0]["text"], "continue");
    // The merge-back is delivered once; later messages carry nothing.
    fixture.say("source", "again").await;
    assert_eq!(fixture.requests("thread/inject_items").len(), 1);
    // A fork can be merged back again, from a later point, and supersedes one still waiting.
    let more = fixture.say("child", "second marker").await;
    for id in ["merge-1", "merge-2"] {
        fixture.command(json!({"type":"thread.merge_back","commandId":id,"sourceThreadId":"child","targetThreadId":"source","sourcePoint":{"type":"run","runId":more["runs"][1]["id"]}})).await;
    }
    let pending: Vec<Value> = fixture.view("source")["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "merge_back").cloned().collect();
    assert_eq!(pending.iter().map(|transfer| transfer["status"].as_str().unwrap()).collect::<Vec<_>>(), ["consumed", "superseded", "pending"]);
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_cannot_take_history_gets_it_with_the_message() {
    let mut fixture = fixture(&[("FIXTURE_NO_INJECT", "1")]).await;
    let source = fixture.say("source", "alpha").await;
    fixture.fork("fork", "source", "child", json!({"type":"run","runId":source["runs"][0]["id"]})).await;
    let child = fixture.say("child", "remember the marker").await;
    fixture.command(json!({"type":"thread.merge_back","commandId":"merge","sourceThreadId":"child","targetThreadId":"source","sourcePoint":{"type":"run","runId":child["runs"][0]["id"]}})).await;
    let after = fixture.say("source", "continue").await;
    assert_eq!(after["contextHandoffs"][0]["delivery"]["status"], "inline");
    let sent = fixture.requests("turn/start");
    let text = sent.last().unwrap()["input"][0]["text"].as_str().unwrap();
    assert!(text.contains("remember the marker"), "{text}");
    assert!(text.ends_with("User message:\ncontinue"), "{text}");
    assert_eq!(after["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user" && message["runId"] == after["runs"][1]["id"]).unwrap()["text"], "continue", "the stored message is the user's own");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_run_is_forked_as_a_transcript_not_a_native_fork() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    fixture.command(json!({"type":"message.dispatch","commandId":"hold","threadId":"source","messageId":"hold","text":"unfinished [hold]","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let running = fixture.until("source", |view| view["runs"][1]["status"] == "running").await;
    let run = running["runs"][1]["id"].clone();
    fixture.command(json!({"type":"run.interrupt","commandId":"stop","threadId":"source","runId":run})).await;
    fixture.settled("source", 2).await;
    assert_eq!(fixture.view("source")["runs"][1]["status"], "interrupted");

    fixture.fork("fork", "source", "child", json!({"type":"run","runId":run})).await;
    let child = fixture.say("child", "go on").await;
    // The provider is not asked to fork: the conversation up to the fork point is its context.
    assert!(fixture.requests("thread/fork").is_empty());
    let transfer = &child["contextTransfers"][0];
    assert_eq!((transfer["status"].as_str(), transfer["resolution"]["strategy"].as_str()), (Some("consumed"), Some("portable_context")));
    let handoff = &child["contextHandoffs"][0];
    assert_eq!(handoff["strategy"], "full_thread_summary");
    assert_eq!(child["providerThreads"][0]["handoffIds"], json!([handoff["id"]]));
    assert!(child["providerThreads"][0]["forkedFrom"].is_null());
    assert_eq!(child["runs"][0]["contextHandoffId"], handoff["id"]);
    let card = child["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "handoff").unwrap();
    assert_eq!(card["title"], "Fork context");
    let injected = fixture.requests("thread/inject_items");
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0]["threadId"], native_thread(&child).as_str());
    let text = injected[0]["items"].to_string();
    assert!(text.contains("alpha") && text.contains("unfinished [hold]"), "{text}");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archiving_or_deleting_a_source_leaves_its_forks_working() {
    let mut fixture = fixture(&[]).await;
    let source = fixture.say("source", "alpha").await;
    let point = json!({"type":"run","runId":source["runs"][0]["id"]});
    fixture.fork("fork-a", "source", "kept", point.clone()).await;
    fixture.fork("fork-b", "source", "orphaned", point.clone()).await;

    // Archiving the source changes nothing about a fork that has not started.
    fixture.command(json!({"type":"thread.archive","commandId":"archive","threadId":"source"})).await;
    let kept = fixture.say("kept", "beta").await;
    assert_eq!(kept["contextTransfers"][0]["resolution"]["strategy"], "native_fork");
    assert_eq!(shown(&kept)[..3], ["inherited:alpha", "inherited:Fixture response: alpha", "synthetic:fork"]);
    fixture.command(json!({"type":"thread.unarchive","commandId":"unarchive","threadId":"source"})).await;

    // Deleting it: the fork keeps what it inherited, and can still be continued.
    fixture.command(json!({"type":"thread.delete","commandId":"delete","threadId":"source"})).await;
    assert!(!fixture.view("source")["thread"]["deletedAt"].is_null());
    let orphan = fixture.say("orphaned", "gamma").await;
    assert_eq!(orphan["thread"]["lineage"]["parentThreadId"], "source");
    assert_eq!(shown(&orphan), ["inherited:alpha", "inherited:Fixture response: alpha", "synthetic:fork", "local:gamma", "local:Fixture response: gamma"]);
    assert_eq!(orphan["contextTransfers"][0]["status"], "consumed");
    // A deleted thread is not a fork source, and its forks cannot be merged back into it.
    let refusal = fixture.refusal(json!({"type":"thread.fork","commandId":"late","sourceThreadId":"source","targetThreadId":"late","sourcePoint":point.clone()})).await;
    assert_eq!(refusal, "Thread source does not exist.");
    let refusal = fixture.refusal(json!({"type":"thread.merge_back","commandId":"back","sourceThreadId":"kept","targetThreadId":"source","sourcePoint":{"type":"run","runId":kept["runs"][0]["id"]}})).await;
    assert_eq!(refusal, "Thread source does not exist.");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_has_checkpoints_of_its_own_and_rolls_back_without_touching_its_source() {
    let mut fixture = fixture(&[]).await;
    let source = fixture.say("source", "alpha").await;
    fixture.fork("fork", "source", "child", json!({"type":"run","runId":source["runs"][0]["id"]})).await;
    let first = fixture.say("child", "beta").await;
    let second = fixture.say("child", "gamma").await;
    // Checkpoints are kept per thread: the fork's runs have their own, none of them the source's.
    let checkpoints: Vec<&Value> = second["checkpoints"].as_array().unwrap().iter().filter(|checkpoint| checkpoint["status"] == "ready" && !checkpoint["runId"].is_null()).collect();
    assert_eq!(checkpoints.len(), 2);
    assert!(checkpoints.iter().all(|checkpoint| checkpoint["threadId"] == "child"));
    assert!(source["checkpoints"].as_array().unwrap().iter().all(|checkpoint| !checkpoints.iter().any(|own| own["id"] == checkpoint["id"])));
    // A fork of the fork cuts at its checkpoint-bearing run.
    fixture.fork("fork-2", "child", "grandchild", json!({"type":"latest_stable"})).await;
    assert_eq!(fixture.view("grandchild")["thread"]["forkedFrom"]["runId"], second["runs"][1]["id"]);
    assert_eq!(fixture.view("grandchild")["thread"]["lineage"]["rootThreadId"], "source");
    assert_eq!(fixture.view("grandchild")["thread"]["lineage"]["parentThreadId"], "child");

    // Rolling the fork back to its first checkpoint rewinds the fork's native thread alone.
    let scope = first["checkpointScopes"][0]["id"].clone();
    let target = first["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["runId"] == first["runs"][0]["id"]).unwrap()["id"].clone();
    fixture.command(json!({"type":"checkpoint.rollback","commandId":"rollback","threadId":"child","scopeId":scope,"checkpointId":target,"restoreFiles":false})).await;
    let rolled = fixture.until("child", |view| view["runs"][1]["status"] == "rolled_back").await;
    assert_eq!(rolled["runs"][0]["status"], "completed");
    let reverts = fixture.requests("thread/revert");
    assert_eq!(reverts.len(), 1);
    assert_eq!(reverts[0]["threadId"], native_thread(&rolled).as_str());
    assert_ne!(reverts[0]["threadId"], native_thread(&fixture.view("source")).as_str());
    let after = fixture.view("source");
    assert!(after["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed"));
    assert_eq!(shown(&rolled)[..3], ["inherited:alpha", "inherited:Fixture response: alpha", "synthetic:fork"]);
    assert!(!shown(&rolled).iter().any(|row| row.contains("gamma")), "a rolled-back run is not part of the conversation");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_another_provider_hands_the_conversation_over_and_back() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    fixture.say("source", "beta").await;
    let other = json!({"instanceId":"codex-b","model":"fixture-model"});
    let switched = fixture.say_with("source", "gamma", json!({"type":"start_immediately"})).await;
    assert_eq!(switched["runs"].as_array().unwrap().len(), 3);
    // (the plain `say` stays on the first provider; the switch follows)
    let id = format!("switch-{}", fixture.next + 1);
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"delta","attachments":[],"modelSelection":other,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 4).await;
    assert_eq!(view["thread"]["providerInstanceId"], "codex-b");
    assert_eq!(view["thread"]["modelSelection"], other);
    let kinds: Vec<String> = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.command_id.as_deref() == Some(id.as_str())).map(|event| event.event.event_type).collect();
    assert_eq!(kinds[0], "thread.provider-switched");
    // The new provider has a provider thread of its own and took the whole conversation as a handoff.
    let threads = view["providerThreads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    let new = threads.iter().find(|thread| thread["providerInstanceId"] == "codex-b").unwrap();
    assert_eq!(new["handoffIds"].as_array().unwrap().len(), 1);
    let handoff = &view["contextHandoffs"][0];
    assert_eq!((handoff["strategy"].as_str(), handoff["status"].as_str(), handoff["delivery"]["status"].as_str()), (Some("full_thread_summary"), Some("ready"), Some("injected")));
    assert_eq!(handoff["coveredRunOrdinals"], json!({"from":1,"to":3}));
    assert_eq!(view["runs"][3]["contextHandoffId"], handoff["id"]);
    let transfer = view["contextTransfers"].as_array().unwrap().iter().find(|transfer| transfer["type"] == "provider_handoff").unwrap();
    assert_eq!((transfer["status"].as_str(), transfer["resolution"]["strategy"].as_str(), transfer["targetProviderInstanceId"].as_str(), transfer["sourceProviderInstanceId"].as_str()), (Some("consumed"), Some("portable_context"), Some("codex-b"), Some("codex")));
    assert!(transfer["basePoint"].is_null());
    let card = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "handoff").unwrap();
    assert_eq!((card["title"].as_str(), card["toProviderInstanceId"].as_str(), card["fromProviderInstanceIds"].clone()), (Some("Provider handoff"), Some("codex-b"), json!(["codex"])));
    let injected = fixture.requests_in(&fixture.record_b.clone(), "thread/inject_items");
    assert_eq!(injected.len(), 1);
    let text = injected[0]["items"].to_string();
    assert!(text.contains("alpha") && text.contains("Fixture response: gamma") && text.contains("beta"), "{text}");
    assert_eq!(injected[0]["threadId"], new["nativeThreadRef"]["nativeId"]);
    let started = fixture.requests_in(&fixture.record_b.clone(), "turn/start");
    assert_eq!((started.len(), started[0]["input"][0]["text"].as_str()), (1, Some("delta")));
    // A handoff does not release the previous provider's session; only continuing a native thread does.
    let detached = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.event_type == "provider-session.detached").count();
    assert_eq!(detached, 0);

    // Back to the first provider: it continues its own provider thread and is told only what it missed.
    let back = format!("back-{}", fixture.next + 1);
    fixture.command(json!({"type":"message.dispatch","commandId":back,"threadId":"source","messageId":back,"text":"epsilon","attachments":[],"modelSelection":{"instanceId":"codex","model":"fixture-model"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 5).await;
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 2, "no third provider thread");
    let delta = view["contextHandoffs"].as_array().unwrap().iter().find(|handoff| handoff["targetRunId"] == view["runs"][4]["id"]).unwrap();
    assert_eq!(delta["strategy"], "delta_since_target_last_seen");
    assert_eq!(delta["coveredRunOrdinals"], json!({"from":4,"to":4}));
    let injected = fixture.requests("thread/inject_items");
    assert_eq!(injected.len(), 1, "the first provider took the missed run");
    let text = injected[0]["items"].to_string();
    assert!(text.contains("delta") && !text.contains("alpha"), "{text}");
    fixture.finish().await;
}

fn launches(fixture: &Fixture, file: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(fixture.directory.path().join(file)).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter_map(|entry| entry["launch"].as_array().map(|args| args.iter().map(|arg| arg.as_str().unwrap().to_owned()).collect())).filter(|args: &Vec<String>| !args.contains(&"--no-session-persistence".to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_that_share_a_claude_home_continue_one_native_conversation_without_a_handoff() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    let send = |id: &str, text: &str, instance: &str| json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":text,"attachments":[],"modelSelection":{"instanceId":instance,"model":"claude-sonnet-4-6"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}});
    // Codex to Claude is a different provider: a handoff.
    fixture.command(send("to-claude", "beta", "claudeAgent")).await;
    let view = fixture.settled("source", 2).await;
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 1);
    let claude_thread = view["providerThreads"].as_array().unwrap().iter().find(|thread| thread["providerInstanceId"] == "claudeAgent").unwrap().clone();
    let native = claude_thread["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();

    // The same Claude home under another account: the native conversation goes on, nobody is briefed.
    fixture.command(send("to-b", "gamma", "claude-b")).await;
    let view = fixture.settled("source", 3).await;
    assert_eq!(view["thread"]["providerInstanceId"], "claude-b");
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 1, "no second handoff");
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 2, "no provider thread of its own");
    assert_eq!(view["runs"][2]["providerThreadId"], claude_thread["id"]);
    assert!(view["runs"][2]["contextHandoffId"].is_null());
    assert!(view["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "provider_handoff").count() == 1);
    let b = launches(&fixture, "claude-b-launches.jsonl");
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(b[0].contains(&format!("--resume={native}")), "{:?}", b[0]);
    // Only the session the conversation left was released, and it says why.
    let detached: Vec<Value> = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.event_type == "provider-session.detached" && event.event.command_id.as_deref() == Some("to-b")).map(|event| event.event.payload).collect();
    assert_eq!(detached.len(), 1, "{detached:?}");
    assert_eq!(detached[0]["reason"], "Provider account changed; continuing the native thread.");

    // Another Claude home is another history: that is a handoff to a provider thread of its own.
    fixture.command(send("to-c", "delta", "claude-c")).await;
    let view = fixture.settled("source", 4).await;
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 2);
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 3);
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn selecting_another_provider_then_sending_hands_over_and_a_running_turn_is_not_steered_elsewhere() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    let other = json!({"instanceId":"codex-b","model":"fixture-model"});
    fixture.command(json!({"type":"message.dispatch","commandId":"hold","threadId":"source","messageId":"hold","text":"busy [hold]","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let running = fixture.until("source", |view| view["runs"][1]["status"] == "running").await;
    // A running turn cannot be steered or restarted onto another provider.
    let refusal = fixture.refusal(json!({"type":"message.dispatch","commandId":"busy","threadId":"source","messageId":"busy","text":"x","attachments":[],"modelSelection":other,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"restart_active","targetRunId":running["runs"][1]["id"]}})).await;
    assert!(refusal.contains("cannot steer or restart a run that is still active"), "{refusal}");
    fixture.command(json!({"type":"run.interrupt","commandId":"release","threadId":"source","runId":running["runs"][1]["id"]})).await;
    fixture.settled("source", 2).await;

    // Choosing the provider changes the thread and releases the old session; the next message hands over.
    fixture.command(json!({"type":"thread.model-selection.set","commandId":"select","threadId":"source","modelSelection":other})).await;
    let chosen = fixture.view("source");
    assert_eq!((chosen["thread"]["providerInstanceId"].as_str(), chosen["thread"]["modelSelection"].clone()), (Some("codex-b"), other.clone()));
    let id = format!("after-{}", fixture.next + 1);
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"gamma","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 3).await;
    let handoff = &view["contextHandoffs"][0];
    assert_eq!((handoff["strategy"].as_str(), handoff["delivery"]["status"].as_str()), (Some("full_thread_summary"), Some("injected")));
    // The interrupted run is part of the conversation the new provider is given.
    let text = fixture.requests_in(&fixture.record_b.clone(), "thread/inject_items")[0]["items"].to_string();
    assert!(text.contains("alpha") && text.contains("busy [hold]"), "{text}");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_provider_switch_command_chooses_the_provider_and_the_next_message_hands_over() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    let other = json!({"instanceId":"codex-b","model":"fixture-model"});
    fixture.command(json!({"type":"provider.switch","commandId":"switch","threadId":"source","modelSelection":other})).await;
    let chosen = fixture.view("source");
    assert_eq!((chosen["thread"]["providerInstanceId"].as_str(), chosen["thread"]["modelSelection"].clone()), (Some("codex-b"), other.clone()));
    let kinds: Vec<String> = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.command_id.as_deref() == Some("switch")).map(|event| event.event.event_type).collect();
    assert_eq!(kinds[0], "thread.provider-switched");
    // Naming the provider it already has is still a switch, not a model change.
    fixture.command(json!({"type":"provider.switch","commandId":"again","threadId":"source","modelSelection":other})).await;
    let kinds: Vec<String> = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.command_id.as_deref() == Some("again")).map(|event| event.event.event_type).collect();
    assert_eq!(kinds[0], "thread.provider-switched");
    fixture.say("source", "gamma").await;
    let view = fixture.view("source");
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 1);
    assert_eq!(view["runs"][1]["providerInstanceId"], "codex-b");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_moves_from_codex_to_claude_and_back() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    let claude = json!({"instanceId":"claudeAgent","model":"claude-sonnet-4-6"});
    let id = "to-claude";
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"beta","attachments":[],"modelSelection":claude,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 2).await;
    assert_eq!(view["runs"][1]["status"], "completed");
    let handoff = &view["contextHandoffs"][0];
    // A native thread that did not exist yet cannot be named in a delivery record, so there is none.
    assert_eq!((handoff["strategy"].as_str(), handoff["delivery"].is_null()), (Some("full_thread_summary"), true));
    // Claude takes no injection: the conversation went ahead of the message, which stays the user's own.
    let said = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "assistant_message" && item["runId"] == view["runs"][1]["id"]).map(|item| item["text"].as_str().unwrap().to_owned()).collect::<Vec<_>>().join("\n");
    assert!(said.starts_with("Working on: Context handoff (full_thread_summary):"), "{said}");
    assert!(said.contains("Fixture response: alpha") && said.contains("\n\nUser message:\nbeta"), "{said}");
    assert_eq!(view["messages"].as_array().unwrap().iter().find(|message| message["runId"] == view["runs"][1]["id"] && message["role"] == "user").unwrap()["text"], "beta");
    assert_eq!(view["providerThreads"].as_array().unwrap().iter().filter(|thread| thread["providerInstanceId"] == "claudeAgent").count(), 1);
    assert!(fixture.requests("thread/inject_items").is_empty());

    // Claude's work comes back to Codex as what Codex missed.
    let id = "to-codex";
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"gamma","attachments":[],"modelSelection":{"instanceId":"codex","model":"fixture-model"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 3).await;
    let delta = view["contextHandoffs"].as_array().unwrap().iter().find(|handoff| handoff["targetRunId"] == view["runs"][2]["id"]).unwrap();
    assert_eq!((delta["strategy"].as_str(), delta["coveredRunOrdinals"].clone(), delta["delivery"]["status"].as_str()), (Some("delta_since_target_last_seen"), json!({"from":2,"to":2}), Some("injected")));
    let text = fixture.requests("thread/inject_items")[0]["items"].to_string();
    assert!(text.contains("beta") && text.contains("Working on:") && text.contains("Covered app runs: 2-2."), "{text}");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_moves_to_a_registry_agent_in_its_prompt() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    let agent = json!({"instanceId":"local-agent","model":"fixture-model"});
    let id = "to-agent";
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"beta","attachments":[],"modelSelection":agent,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 2).await;
    let handoff = &view["contextHandoffs"][0];
    assert_eq!(handoff["strategy"], "full_thread_summary");
    let prompts: Vec<String> = std::fs::read_to_string(&fixture.acp_prompts).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].starts_with("Context handoff (full_thread_summary):"), "{}", prompts[0]);
    assert!(prompts[0].contains("Fixture response: alpha") && prompts[0].ends_with("\n\nUser message:\nbeta"), "{}", prompts[0]);
    assert_eq!(view["thread"]["providerInstanceId"], "local-agent");
    fixture.finish().await;
}

/// The commands the server's own services issue are public commands in the original, which runs them
/// for a client like any other. Each reaches the same service code over the socket.
#[tokio::test(flavor = "multi_thread")]
async fn the_commands_services_issue_run_for_a_client_too() {
    let mut fixture = fixture(&[]).await;
    let view = fixture.say("source", "alpha").await;
    let session = view["providerSessions"][0]["id"].clone();
    assert_ne!(view["providerSessions"][0]["status"], "stopped");

    // A provider session of the thread is released; one it does not own is refused with the original's text.
    let refusal = fixture.refusal(json!({"type":"provider-session.detach","commandId":"detach-other","threadId":"source","providerSessionId":"session-other"})).await;
    assert_eq!(refusal, "Provider session session-other does not belong to thread source.");
    fixture.command(json!({"type":"provider-session.detach","commandId":"detach","threadId":"source","providerSessionId":session,"reason":"client-requested"})).await;
    fixture.until("source", |view| view["providerSessions"].as_array().unwrap().is_empty()).await;

    // A pull request link receives what the sync read, and only the link it names.
    fixture.command(json!({"type":"thread.pull-request.link","commandId":"link","threadId":"source","host":"github.com","repository":"o/r","number":7,"url":"https://github.com/o/r/pull/7","source":"manual"})).await;
    let snapshot = json!({"state":"open","title":"A change","headBranch":"feature","baseBranch":"main","isDraft":false,"updatedAt":"2026-02-01T00:00:00.000Z","syncedAt":"2026-02-01T00:00:00.000Z"});
    fixture.command(json!({"type":"thread.pull-request-link.sync","commandId":"link-sync","threadId":"source","host":"github.com","repository":"o/r","number":7,"snapshot":snapshot,"stack":null})).await;
    let thread = fixture.view("source")["thread"].clone();
    assert_eq!(thread["pullRequests"][0]["snapshot"]["title"], "A change");

    // A discovery read before the thread changed again is refused.
    let expected = json!({"workspaceRoot":std::fs::canonicalize(fixture.directory.path()).unwrap().join("workspace"),"branch":thread["branch"],"worktreePath":thread["worktreePath"],"linkedPullRequest":null,"branchPullRequest":null});
    let discovery = |id: &str, sequence: u64| json!({"type":"thread.pull-request.sync","commandId":id,"threadId":"source","projectId":"project","snapshotSequence":sequence,"expected":expected,"branchPullRequest":null});
    let refusal = fixture.refusal(discovery("discover-stale", 0)).await;
    assert_eq!(refusal, "Thread source changed before pull request discovery.");

    // A title regeneration that is not the current request changes nothing; the current one lands its title.
    fixture.command(json!({"type":"thread.title.regeneration.complete","commandId":"title-other","threadId":"source","requestId":"not-current","title":"Ignored"})).await;
    assert_eq!(fixture.view("source")["thread"]["title"], "Source");

    // The delegated-task commands refuse as the service does, not as the decoder did.
    let refusal = fixture.refusal(json!({"type":"delegated_task.completion-delivery.dispose","commandId":"dispose","parentThreadId":"source","taskId":"node:none"})).await;
    assert_eq!(refusal, "Delegated task node:none is not an app-owned task of thread source.");
    let refusal = fixture.refusal(json!({"type":"delegated_task.request","commandId":"delegate","parentThreadId":"source","parentRunId":"run:none","parentNodeId":"node:none","task":"Do it","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","createdBy":"agent","creationSource":"mcp"})).await;
    assert_eq!(refusal, "Parent run run:none is not active.");
    let refusal = fixture.refusal(json!({"type":"thread.created.record","commandId":"record","parentThreadId":"source","parentRunId":"run:none","parentNodeId":"node:none","targetThreadId":"source","targetRunId":null})).await;
    assert_eq!(refusal, "Parent node node:none is not the root of run run:none.");
    fixture.finish().await;
}

/// A message to another provider while a run is going waits behind it on that provider's own thread, and
/// the handoff is made when it starts. Nothing is released until then.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_another_provider_waits_behind_the_active_run_and_hands_over_when_it_starts() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    fixture.command(json!({"type":"message.dispatch","commandId":"hold","threadId":"source","messageId":"hold","text":"busy [hold]","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let running = fixture.until("source", |view| view["runs"][1]["status"] == "running").await;
    let other = json!({"instanceId":"codex-b","model":"fixture-model"});
    fixture.command(json!({"type":"message.dispatch","commandId":"to-b","threadId":"source","messageId":"to-b","text":"gamma","attachments":[],"modelSelection":other,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let queued = fixture.view("source");
    let run = &queued["runs"][2];
    assert_eq!((run["status"].as_str(), run["providerInstanceId"].as_str()), (Some("queued"), Some("codex-b")));
    let waiting = queued["providerThreads"].as_array().unwrap().iter().find(|thread| thread["id"] == run["providerThreadId"]).unwrap();
    assert_eq!((waiting["providerInstanceId"].as_str(), waiting["nativeThreadRef"].is_null()), (Some("codex-b"), true));
    assert_eq!(queued["thread"]["providerInstanceId"], "codex", "the thread stays on its provider until the queued run starts");
    assert!(queued["providerSessions"].as_array().unwrap().iter().all(|session| session["status"] != "stopped"));

    fixture.command(json!({"type":"run.interrupt","commandId":"release","threadId":"source","runId":running["runs"][1]["id"]})).await;
    let view = fixture.settled("source", 3).await;
    assert_eq!(view["runs"][2]["status"], "completed");
    assert_eq!(view["thread"]["providerInstanceId"], "codex-b");
    let handoff = &view["contextHandoffs"][0];
    assert_eq!((handoff["strategy"].as_str(), handoff["delivery"]["status"].as_str(), view["runs"][2]["contextHandoffId"].clone()), (Some("full_thread_summary"), Some("injected"), handoff["id"].clone()));
    let text = fixture.requests_in(&fixture.record_b.clone(), "thread/inject_items")[0]["items"].to_string();
    assert!(text.contains("alpha") && text.contains("busy [hold]"), "{text}");
    assert_eq!(view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "handoff" && item["runId"] == view["runs"][2]["id"]).count(), 1);
    fixture.finish().await;
}

/// Choosing another provider is allowed while a run is going; it releases the sessions the switch names.
#[tokio::test(flavor = "multi_thread")]
async fn choosing_another_provider_during_a_run_releases_the_old_providers_session() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    fixture.command(json!({"type":"message.dispatch","commandId":"hold","threadId":"source","messageId":"hold","text":"busy [hold]","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    fixture.until("source", |view| view["runs"][1]["status"] == "running").await;
    let other = json!({"instanceId":"codex-b","model":"fixture-model"});
    fixture.command(json!({"type":"thread.model-selection.set","commandId":"select-busy","threadId":"source","modelSelection":other})).await;
    let kinds: Vec<(String, Value)> = fixture.store.events(0, None, Some("thread"), Some("source"), 10_000).unwrap().into_iter().filter(|event| event.event.command_id.as_deref() == Some("select-busy")).map(|event| (event.event.event_type, event.event.payload)).collect();
    assert_eq!(kinds[0].0, "thread.provider-switched");
    let detached: Vec<&Value> = kinds.iter().filter(|(kind, _)| kind == "provider-session.detached").map(|(_, payload)| payload).collect();
    assert_eq!(detached.len(), 1, "{kinds:?}");
    assert_eq!(detached[0]["reason"], "Provider or model selection changed.");
    let view = fixture.until("source", |view| view["runs"][1]["status"] != "running").await;
    assert_eq!(view["thread"]["providerInstanceId"], "codex-b");
    fixture.finish().await;
}

/// A fork is named by its creator. Its first message carries a `titleSeed` like every first message to a
/// thread with no messages of its own (a fork's inherited history is not a message of the thread): the
/// original sets the title to that seed, so the web client sends the thread's current title as the seed
/// when it writes to an existing thread, and a client that sends the message's own words renames the fork.
#[tokio::test(flavor = "multi_thread")]
async fn a_forks_first_message_takes_its_title_from_the_seed_the_client_sends() {
    let mut fixture = fixture(&[]).await;
    fixture.say("source", "alpha").await;
    fixture.fork("fork-kept", "source", "kept", json!({"type":"latest_stable"})).await;
    fixture.fork("fork-renamed", "source", "renamed", json!({"type":"latest_stable"})).await;
    assert_eq!(fixture.view("kept")["thread"]["title"], "kept title");
    for (thread, seed) in [("kept", "kept title"), ("renamed", "beta")] {
        let id = format!("first-{thread}");
        fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":"beta","titleSeed":seed,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        fixture.settled(thread, 1).await;
    }
    assert_eq!(fixture.view("kept")["thread"]["title"], "kept title");
    assert_eq!(fixture.view("renamed")["thread"]["title"], "beta");
    fixture.finish().await;
}

/// Two Codex instances that share a Codex home share its sessions: a thread moving between them goes on
/// in one native conversation, resumed by the second instance, and nobody is briefed.
#[tokio::test(flavor = "multi_thread")]
async fn codex_instances_that_share_a_home_continue_one_native_conversation() {
    let mut fixture = fixture(&[]).await;
    let first = fixture.say("source", "alpha").await;
    let native = native_thread(&first);
    let id = "to-same-home";
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"beta","attachments":[],"modelSelection":{"instanceId":"codex-same-home","model":"fixture-model"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 2).await;
    assert_eq!(view["thread"]["providerInstanceId"], "codex-same-home");
    assert!(view["contextHandoffs"].as_array().unwrap().is_empty(), "no handoff");
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 1, "one native conversation");
    let resumed = fixture.requests_in(&std::fs::canonicalize(fixture.directory.path()).unwrap().join("codex-same-home-requests.jsonl"), "thread/resume");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0]["threadId"], native.as_str());
    // A home of its own is another history: a handoff, as before.
    let id = "to-other-home";
    fixture.command(json!({"type":"message.dispatch","commandId":id,"threadId":"source","messageId":id,"text":"gamma","attachments":[],"modelSelection":{"instanceId":"codex-b","model":"fixture-model"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let view = fixture.settled("source", 3).await;
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 1);
    fixture.finish().await;
}
