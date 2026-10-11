#![cfg(unix)]
#![allow(dead_code)]
//! Launching a thread into a git worktree, end to end: the worktree is created after the launch
//! commits, the provider runs in it, a setup script runs in the thread's terminal with its output
//! reaching `subscribeWorktreeSetup`, and failures and cancellation clean up as the original does.
//! A real server on a loopback socket, a temporary git repository, the recording Codex fixture and
//! real PTY shells; nothing outside the temporary directory is read or written.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
    terminal_manager::{TerminalManager, TerminalManagerOptions},
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Fixture {
    _directory: tempfile::TempDir,
    base: std::path::PathBuf,
    workspace: std::path::PathBuf,
    store: Store,
    execution: ExecutionService,
    terminals: TerminalManager,
    address: std::net::SocketAddr,
    operator: String,
    reader: String,
    record: std::path::PathBuf,
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

/// `scripts` are the project's scripts; one with `runOnWorktreeCreate` is the setup script.
async fn fixture(scripts: Value) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-q", "-m", "initial"]);
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace,"scripts":scripts}), Utc::now())
        .unwrap();
    let record = base.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":base.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let mut options = TerminalManagerOptions::host(base.join("terminal-logs"), &t3_contracts::ServerSettings::default());
    options.shell = Some("/bin/sh".into());
    options.kill_grace = std::time::Duration::ZERO;
    let terminals = TerminalManager::new(options).await.unwrap();
    execution.workspace().set_completion_shell("sh");
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_workspace".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"workspace-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: Some(providers),
        execution: Some(execution.clone()),
        workspace: None,
        terminals: Some(terminals.clone()),
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
    Fixture { _directory: directory, base, workspace, store, execution, terminals, address, operator, reader, record, stop: Some(stop), server: Some(server) }
}

impl Fixture {
    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    async fn finish(mut self) {
        self.terminals.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
    fn worktrees(&self) -> Vec<std::path::PathBuf> {
        let directory = self.base.join("worktrees").join("workspace");
        let mut found: Vec<_> = std::fs::read_dir(directory).map(|entries| entries.map(|entry| entry.unwrap().path()).collect()).unwrap_or_default();
        found.sort();
        found
    }
    fn provider_requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == method)
            .collect()
    }
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket
        .send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into()))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}
fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}

/// Reads a stream until a chunk value satisfies `wanted`, acknowledging every chunk.
async fn stream_until(socket: &mut Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
            if message["_tag"] != "Chunk" {
                continue;
            }
            socket.send(Message::Text(json!({"_tag":"Ack","requestId":id}).to_string().into())).await.unwrap();
            for value in message["values"].as_array().unwrap() {
                if wanted(value) {
                    return value.clone();
                }
            }
        }
    })
    .await
    .expect("the stream never showed the wanted state")
}

async fn subscribe(fixture: &Fixture, thread: &str) -> Socket {
    let mut socket = fixture.connect(&fixture.reader).await;
    socket
        .send(Message::Text(json!({"_tag":"Request","id":50,"tag":"subscribeWorktreeSetup","payload":{"threadId":thread},"headers":[]}).to_string().into()))
        .await
        .unwrap();
    // The first value is the state when the subscription opened: nothing yet.
    stream_until(&mut socket, 50, |value| value.is_null()).await;
    socket
}

/// Waits on committed events until the thread satisfies `done`.
async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, thread: &str, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(HANG_BOUND, async {
        loop {
            if let Some(view) = store.projection("thread", thread).unwrap() {
                last = json!({"thread":view["thread"],"runs":view["runs"],"items":view["turnItems"]});
                if done(&view) {
                    return view;
                }
            }
            events.recv().await.unwrap();
        }
    })
    .await;
    outcome.unwrap_or_else(|_| panic!("the thread never reached the wanted state: {last}"))
}
fn run_status(view: &Value, status: &str) -> bool {
    view["runs"].as_array().is_some_and(|runs| runs.len() == 1 && runs[0]["status"] == status)
}

fn launch_input(strategy: Value, message: bool) -> Value {
    let mut input = json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":strategy});
    if message {
        input["initialMessage"] = json!({"text":"Do the work","attachments":[]});
    }
    input
}

async fn launch(_fixture: &Fixture, socket: &mut Socket, strategy: Value, message: bool) -> Value {
    let exit = rpc(socket, 1, "orchestration.launchThread", launch_input(strategy, message)).await;
    success(&exit).clone()
}

fn setup_script(command: &str, wait: bool) -> Value {
    json!([{"id":"setup","name":"Install","command":command,"icon":"configure","runOnWorktreeCreate":true,"async":!wait}])
}

#[tokio::test]
async fn a_worktree_launch_creates_the_checkout_after_the_commit_and_the_provider_runs_in_it() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut watcher = subscribe(&fixture, "thread").await;
    let mut events = fixture.store.subscribe();
    let launched = launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    assert_eq!(launched["threadId"], "thread");
    // The launch answers with the run waiting for its workspace: no worktree has been recorded yet.
    assert!(launched["projection"]["runs"][0]["status"] == "preparing" || launched["projection"]["runs"][0]["status"] == "starting" || launched["projection"]["runs"][0]["status"] == "running" || launched["projection"]["runs"][0]["status"] == "completed");
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    let thread = &view["thread"];
    let path = thread["worktreePath"].as_str().expect("the thread records its worktree");
    let worktrees = fixture.worktrees();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(std::path::Path::new(path), worktrees[0]);
    // `<worktrees>/<repo name>/<branch with slashes flattened>`.
    let branch = thread["branch"].as_str().unwrap();
    assert!(regex_like_temporary(branch), "{branch}");
    assert_eq!(worktrees[0].file_name().unwrap().to_string_lossy(), branch.replace('/', "-"));
    assert!(worktrees[0].join("README.md").exists());
    assert!(git(&fixture.workspace, &["worktree", "list", "--porcelain"]).contains(path));
    assert_eq!(git(&worktrees[0], &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), branch);
    // The provider ran in the worktree, and the run's checkpoint scope is that directory.
    let starts = fixture.provider_requests("thread/start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["params"]["cwd"], path);
    assert_eq!(view["checkpointScopes"][0]["cwd"], path);
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "command_execution" && item["title"] == "Workspace ready" && item["output"] == "Workspace preparation completed."));
    // The card the client watched ran its stages in order and ended done.
    let done = stream_until(&mut watcher, 50, |value| value["phase"] == "done").await;
    assert_eq!(done["worktreePath"], path);
    assert_eq!(done["branch"], branch);
    assert_eq!(done["baseRef"], "main");
    let stages: Vec<(&str, &str)> = done["stages"].as_array().unwrap().iter().map(|stage| (stage["id"].as_str().unwrap(), stage["status"].as_str().unwrap())).collect();
    assert_eq!(stages, [("fetch", "skipped"), ("checkout", "done"), ("setup-script", "skipped"), ("agent", "done")]);
    assert_eq!(done["error"], Value::Null);
    fixture.finish().await;
}

fn regex_like_temporary(branch: &str) -> bool {
    branch.len() == 11 && branch.starts_with("t3/") && branch[3..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[tokio::test]
async fn a_launch_that_cannot_check_out_fails_the_run_and_leaves_nothing_behind() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut watcher = subscribe(&fixture, "thread").await;
    let mut events = fixture.store.subscribe();
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"no-such-branch"}), true).await;
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "failed")).await;
    // The thread and its message stay visible; only the workspace failed.
    assert_eq!(view["messages"][0]["text"], "Do the work");
    assert_eq!(view["thread"]["worktreePath"], Value::Null);
    let items = view["turnItems"].as_array().unwrap();
    let preparation = items.iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!((preparation["status"].as_str(), preparation["title"].as_str()), (Some("failed"), Some("Workspace preparation failed")));
    let error = items.iter().find(|item| item["type"] == "error").unwrap();
    assert_eq!(error["failure"]["code"], "workspace_preparation_failed");
    assert_eq!(error["failure"]["class"], "validation_error");
    assert!(error["failure"]["message"].as_str().unwrap().starts_with("Workspace preparation failed during provision worktree:"), "{error}");
    assert!(fixture.worktrees().is_empty());
    assert_eq!(git(&fixture.workspace, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    assert!(fixture.provider_requests("thread/start").is_empty());
    let failed = stream_until(&mut watcher, 50, |value| value["phase"] == "failed").await;
    assert!(failed["error"].as_str().unwrap().contains("provision worktree"));
    fixture.finish().await;
}

#[tokio::test]
async fn a_setup_script_runs_in_the_worktree_and_its_output_reaches_the_client_before_the_agent_starts() {
    let fixture = fixture(setup_script("echo installing-dependencies\nprintf ready > setup-ran.txt\nprintf '%s|%s' \"$T3CODE_PROJECT_ROOT\" \"$T3CODE_WORKTREE_PATH\" > env.txt", true)).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut watcher = subscribe(&fixture, "thread").await;
    // Drained as it arrives: a terminal subscription that falls behind is dropped.
    let mut terminal_events = fixture.terminals.events();
    let closed = tokio::spawn(async move {
        while let Ok(Some(event)) = terminal_events.recv().await {
            if event["type"] == "closed" && event["terminalId"] == "setup-setup" {
                return true;
            }
        }
        false
    });
    let mut events = fixture.store.subscribe();
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    let path = view["thread"]["worktreePath"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(std::path::Path::new(path).join("setup-ran.txt")).unwrap(), "ready");
    // The script sees the project it belongs to and the checkout it runs in.
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(path).join("env.txt")).unwrap(),
        format!("{}|{path}", fixture.workspace.display())
    );
    let done = stream_until(&mut watcher, 50, |value| value["phase"] == "done").await;
    let script = &done["stages"].as_array().unwrap().iter().find(|stage| stage["id"] == "setup-script").unwrap().clone();
    assert_eq!(script["status"], "done");
    assert_eq!(script["detail"], "exited with 0");
    assert!(script["tail"].as_array().unwrap().iter().any(|line| line == "installing-dependencies"), "{script}");
    assert_eq!(done["setupScript"]["name"], "Install");
    assert_eq!(done["setupScript"]["terminalId"], "setup-setup");
    // A clean run leaves only an idle shell, which is closed before the agent starts.
    assert!(closed.await.unwrap(), "the terminal events ended before the shell closed");
    fixture.finish().await;
}

#[tokio::test]
async fn a_failing_setup_script_fails_the_run_keeps_the_recorded_worktree_and_a_retry_reuses_it() {
    let fixture = fixture(setup_script("test -f ready.txt || { echo not-ready; exit 3; }", true)).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut events = fixture.store.subscribe();
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "failed")).await;
    let items = view["turnItems"].as_array().unwrap();
    let error = items.iter().find(|item| item["type"] == "error").unwrap();
    assert!(error["failure"]["message"].as_str().unwrap().contains("Setup script exited with 3."), "{error}");
    // The thread recorded the worktree before the script ran, so it stays for the retry.
    let path = view["thread"]["worktreePath"].as_str().expect("a recorded worktree is kept").to_owned();
    assert!(std::path::Path::new(&path).exists());
    assert_eq!(fixture.worktrees().len(), 1);
    assert!(fixture.provider_requests("thread/start").is_empty());
    let run_id = view["runs"][0]["id"].clone();
    std::fs::write(std::path::Path::new(&path).join("ready.txt"), "yes").unwrap();
    let mut events = fixture.store.subscribe();
    let retry = rpc(&mut socket, 2, "orchestration.dispatchCommand", json!({"type":"prepared-run.retry","commandId":"retry","threadId":"thread","runId":run_id})).await;
    assert!(success(&retry)["sequence"].is_number(), "{retry}");
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    // The same run, the same worktree; the old failure no longer offers a retry.
    assert_eq!(view["runs"][0]["id"], run_id);
    assert_eq!(view["thread"]["worktreePath"], path);
    assert_eq!(fixture.worktrees().len(), 1);
    let items = view["turnItems"].as_array().unwrap();
    assert_eq!(items.iter().find(|item| item["type"] == "error").unwrap()["status"], "cancelled");
    assert_eq!(fixture.provider_requests("thread/start")[0]["params"]["cwd"], path);
    // A retry with nothing to retry is refused.
    let again = rpc(&mut socket, 3, "orchestration.dispatchCommand", json!({"type":"prepared-run.retry","commandId":"retry-again","threadId":"thread","runId":run_id})).await;
    assert_eq!(again["_tag"], "Failure", "{again}");
    fixture.finish().await;
}

#[tokio::test]
async fn cancelling_a_running_setup_removes_the_worktree_and_fails_the_run() {
    let fixture = fixture(setup_script("echo started; sleep 120", true)).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut watcher = subscribe(&fixture, "thread").await;
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    // The script is running in its terminal once the card names it and shows its first line.
    let running = stream_until(&mut watcher, 50, |value| {
        value["setupScript"]["terminalId"] == "setup-setup" && value["stages"].as_array().is_some_and(|stages| stages.iter().any(|stage| stage["id"] == "setup-script" && stage["tail"].as_array().is_some_and(|tail| tail.iter().any(|line| line == "started"))))
    })
    .await;
    let path = running["worktreePath"].as_str().unwrap().to_owned();
    assert!(std::path::Path::new(&path).exists());
    // Observing the setup is allowed with read access; stopping it is not.
    let mut reader = fixture.connect(&fixture.reader).await;
    let refused = rpc(&mut reader, 9, "worktreeSetup.cancel", json!({"threadId":"thread"})).await;
    assert_eq!(refused["_tag"], "Failure", "{refused}");
    assert!(fixture.worktrees().len() == 1);
    let cancelled = rpc(&mut socket, 2, "worktreeSetup.cancel", json!({"threadId":"thread"})).await;
    assert_eq!(success(&cancelled)["cancelled"], true);
    // The call returned after the setup unwound: everything is already rolled back.
    let view = fixture.store.projection("thread", "thread").unwrap().unwrap();
    assert!(run_status(&view, "failed"), "{}", view["runs"]);
    assert_eq!(view["thread"]["worktreePath"], Value::Null);
    assert_eq!(view["thread"]["branch"], Value::Null);
    assert!(!std::path::Path::new(&path).exists());
    assert!(fixture.worktrees().is_empty());
    let error = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").unwrap().clone();
    assert!(error["failure"]["message"].as_str().unwrap().contains("Worktree setup cancelled."), "{error}");
    let cancelled = stream_until(&mut watcher, 50, |value| value["phase"] == "cancelled").await;
    assert_eq!(cancelled["error"], Value::Null);
    // Nothing is left to cancel.
    assert_eq!(success(&rpc(&mut socket, 3, "worktreeSetup.cancel", json!({"threadId":"thread"})).await)["cancelled"], false);
    assert!(fixture.provider_requests("thread/start").is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn a_root_launch_with_a_setup_script_starts_it_without_waiting_for_it() {
    // The arithmetic is only in the script's output, never in the echo of the typed command.
    let fixture = fixture(setup_script("echo $((6*7))", false)).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut terminal_events = fixture.terminals.events();
    let printed = tokio::spawn(async move {
        let mut output = String::new();
        while let Ok(Some(event)) = terminal_events.recv().await {
            if event["type"] == "output" && event["terminalId"] == "setup-setup" {
                output.push_str(event["data"].as_str().unwrap_or(""));
                if output.lines().any(|line| line == "42") {
                    return true;
                }
            }
        }
        false
    });
    let mut events = fixture.store.subscribe();
    launch(&fixture, &mut socket, json!({"type":"root"}), true).await;
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    assert_eq!(view["thread"]["worktreePath"], Value::Null);
    assert!(fixture.worktrees().is_empty());
    // The provider ran in the project root, and the script ran in a terminal there.
    assert!(printed.await.unwrap(), "the terminal events ended before the script ran");
    assert_eq!(fixture.provider_requests("thread/start")[0]["params"]["cwd"], fixture.workspace.to_string_lossy().as_ref());
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["title"] == "Workspace ready"));
    // No card is shown for a launch that creates no worktree.
    let _watcher = subscribe(&fixture, "thread").await;
    fixture.finish().await;
}

#[tokio::test]
async fn a_worktree_launch_without_a_message_prepares_the_workspace_and_waits() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut events = fixture.store.subscribe();
    let launched = launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main","branch":"feature/work"}), false).await;
    assert_eq!(launched["projection"]["thread"]["branch"], "feature/work");
    let view = until(&fixture.store, &mut events, "thread", |view| view["thread"]["worktreePath"].is_string()).await;
    assert_eq!(view["thread"]["branch"], "feature/work");
    assert_eq!(view["runs"], json!([]));
    let worktrees = fixture.worktrees();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].file_name().unwrap(), "feature-work");
    assert_eq!(git(&worktrees[0], &["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "feature/work");
    fixture.finish().await;
}

// ---- deleting threads and projects ----------------------------------------------------------

fn refs(workspace: &std::path::Path) -> Vec<String> {
    git(workspace, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/t3/orchestration-v2/checkpoints"]).lines().map(str::to_owned).collect()
}

async fn command(socket: &mut Socket, id: u64, command: Value) -> Value {
    rpc(socket, id, "orchestration.dispatchCommand", command).await
}

fn delete_command(thread: &str, id: &str) -> Value {
    json!({"type":"thread.delete","commandId":id,"threadId":thread})
}

#[tokio::test]
async fn deleting_a_thread_with_history_cleans_up_what_it_owned_and_leaves_what_the_original_leaves() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut events = fixture.store.subscribe();
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    let view = until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    let worktree = std::path::PathBuf::from(view["thread"]["worktreePath"].as_str().unwrap());
    // A second turn carries an uploaded file, which the store keeps under the thread's name.
    let issued = success(&rpc(&mut socket, 2, "attachments.createUploadUrl", json!({"type":"file","name":"notes.pdf","mimeType":"application/pdf","sizeBytes":16})).await).clone();
    let posted = reqwest::Client::new().post(format!("http://{}{}", fixture.address, issued["relativeUrl"].as_str().unwrap())).body(b"%PDF-1.4 fixture".to_vec()).send().await.unwrap();
    assert_eq!(posted.status(), 204);
    let attachment = json!({"type":"file","id":issued["attachmentId"],"name":"notes.pdf","mimeType":"application/pdf","sizeBytes":16});
    let second = json!({"type":"message.dispatch","commandId":"second","threadId":"thread","messageId":"message:second","text":"Read this","attachments":[attachment],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}});
    assert!(success(&command(&mut socket, 3, second).await)["sequence"].is_number());
    let view = until(&fixture.store, &mut events, "thread", |view| view["runs"].as_array().is_some_and(|runs| runs.len() == 2 && runs.iter().all(|run| run["status"] == "completed"))).await;
    let stored = view["messages"].as_array().unwrap().iter().find(|message| message["id"] == "message:second").unwrap()["attachments"][0]["id"].as_str().unwrap().to_owned();
    let file = fixture.base.join("attachments").join(format!("{stored}.pdf"));
    assert!(file.exists());
    // A terminal the thread opened, and the checkpoints its turns captured.
    fixture.terminals.open(serde_json::from_value(json!({"threadId":"thread","terminalId":"shell","cwd":worktree})).unwrap()).await.unwrap();
    let mut terminal_events = fixture.terminals.events();
    let closed = tokio::spawn(async move {
        while let Ok(Some(event)) = terminal_events.recv().await {
            if event["type"] == "closed" && event["terminalId"] == "shell" {
                return true;
            }
        }
        false
    });
    let checkpoints = refs(&fixture.workspace);
    assert!(!checkpoints.is_empty());

    // Reading is not enough to delete.
    let mut reader = fixture.connect(&fixture.reader).await;
    assert_eq!(command(&mut reader, 1, delete_command("thread", "reader-delete")).await["_tag"], "Failure");
    assert!(fixture.store.projection("thread", "thread").unwrap().unwrap()["thread"]["deletedAt"].is_null());

    let mut events = fixture.store.subscribe();
    assert!(success(&command(&mut socket, 4, delete_command("thread", "delete")).await)["sequence"].is_number());
    fixture.execution.drained("thread").await;
    let after = fixture.store.projection("thread", "thread").unwrap().unwrap();
    assert!(after["thread"]["deletedAt"].is_string());
    // The history stays: runs, messages and the worktree record are not rewritten.
    assert_eq!(after["runs"], view["runs"]);
    assert_eq!(after["messages"].as_array().unwrap().len(), view["messages"].as_array().unwrap().len());
    assert_eq!(after["thread"]["worktreePath"], worktree.to_string_lossy().as_ref());
    // Removed: the attachment file and the terminal with its history.
    assert!(!file.exists());
    assert!(closed.await.unwrap(), "the thread's terminal was never closed");
    // Left alone, as the original leaves them: the worktree, its branch and the checkpoint refs.
    assert!(worktree.join("README.md").exists());
    assert!(git(&fixture.workspace, &["worktree", "list", "--porcelain"]).contains(worktree.to_str().unwrap()));
    assert_eq!(refs(&fixture.workspace), checkpoints);
    // Every effect of the deletion settled.
    let effects: Vec<Value> = ["terminal.cleanup", "attachment.cleanup"].iter().map(|kind| serde_json::to_value(fixture.store.effect(&format!("effect:delete:{kind}")).unwrap().unwrap()).unwrap()).collect();
    assert!(effects.iter().all(|effect| effect["status"] == "succeeded"), "{effects:?}");
    // The shell no longer lists it, and nothing more can be said to it.
    let shell = success(&rpc(&mut socket, 5, "orchestration.getArchivedShellSnapshot", json!({})).await).clone();
    assert!(shell["threads"].as_array().unwrap().iter().all(|thread| thread["id"] != "thread"));
    let refused = command(&mut socket, 6, json!({"type":"message.dispatch","commandId":"late","threadId":"thread","messageId":"message:late","text":"hello","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    assert_eq!(refused["_tag"], "Failure");
    // Deleting again under another id is accepted and changes nothing; the same id replays.
    let before = fixture.store.projection("thread", "thread").unwrap().unwrap()["thread"]["deletedAt"].clone();
    assert!(success(&command(&mut socket, 7, delete_command("thread", "delete-again")).await)["sequence"].is_number());
    assert_eq!(fixture.store.projection("thread", "thread").unwrap().unwrap()["thread"]["deletedAt"], before);
    let latest = fixture.store.latest_sequence().unwrap();
    assert!(success(&command(&mut socket, 8, delete_command("thread", "delete")).await)["sequence"].is_number());
    assert_eq!(fixture.store.latest_sequence().unwrap(), latest);
    drop(events);
    fixture.finish().await;
}

#[tokio::test]
async fn deleting_a_thread_during_a_turn_cancels_the_run_and_releases_the_provider() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut events = fixture.store.subscribe();
    let mut input = launch_input(json!({"type":"root"}), true);
    input["runtimeMode"] = json!("approval-required");
    // The fixture holds a turn that asks for approval until someone answers.
    input["initialMessage"]["text"] = json!("Run it [approval]");
    success(&rpc(&mut socket, 1, "orchestration.launchThread", input).await);
    // The turn is running and waiting for the user to approve a command.
    let view = until(&fixture.store, &mut events, "thread", |view| view["runtimeRequests"].as_array().is_some_and(|requests| requests.iter().any(|request| request["status"] == "pending"))).await;
    assert!(view["providerSessions"].as_array().is_some_and(|sessions| !sessions.is_empty()));
    let run = view["runs"][0]["id"].clone();
    assert!(success(&command(&mut socket, 2, delete_command("thread", "delete")).await)["sequence"].is_number());
    fixture.execution.drained("thread").await;
    let after = fixture.store.projection("thread", "thread").unwrap().unwrap();
    assert_eq!(after["runs"][0]["id"], run);
    assert_eq!(after["runs"][0]["status"], "cancelled");
    assert_eq!(after["attempts"][0]["status"], "cancelled");
    assert_eq!(after["nodes"][0]["status"], "cancelled");
    let request = after["runtimeRequests"].as_array().unwrap().iter().find(|request| request["id"] == view["runtimeRequests"][0]["id"]).unwrap();
    assert_eq!(request["status"], "cancelled");
    assert_eq!(request["responseCapability"], json!({"type":"not_resumable","reason":"The thread was deleted."}));
    // The provider session was detached, and its detach effect ran to the end.
    assert_eq!(after["providerSessions"], json!([]));
    let detach = fixture.store.effect(&format!("effect:delete:provider-session.detach:{}", view["providerSessions"][0]["id"].as_str().unwrap())).unwrap().unwrap();
    assert_eq!(detach.status, "succeeded");
    // The run never completes afterwards: the provider stays quiet about it.
    assert_eq!(fixture.store.projection("thread", "thread").unwrap().unwrap()["runs"][0]["status"], "cancelled");
    fixture.finish().await;
}

#[tokio::test]
async fn deleting_a_project_deletes_its_threads_and_refuses_without_force() {
    let fixture = fixture(json!([])).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut events = fixture.store.subscribe();
    // A thread with a finished turn, an archived thread, and a thread of another project.
    launch(&fixture, &mut socket, json!({"type":"root"}), true).await;
    until(&fixture.store, &mut events, "thread", |view| run_status(view, "completed")).await;
    let mut archived = launch_input(json!({"type":"root"}), false);
    archived["commandId"] = json!("launch-archived");
    archived["threadId"] = json!("archived");
    success(&rpc(&mut socket, 2, "orchestration.launchThread", archived).await);
    assert!(success(&command(&mut socket, 3, json!({"type":"thread.archive","commandId":"archive","threadId":"archived"})).await)["sequence"].is_number());
    let other = fixture.base.join("other");
    std::fs::create_dir(&other).unwrap();
    success(&rpc(&mut socket, 4, "projects.mutate", json!({"type":"project.create","commandId":"other-project","projectId":"other","title":"Other","workspaceRoot":other})).await);
    let mut kept = launch_input(json!({"type":"root"}), false);
    kept["commandId"] = json!("launch-kept");
    kept["threadId"] = json!("kept");
    kept["projectId"] = json!("other");
    success(&rpc(&mut socket, 5, "orchestration.launchThread", kept).await);

    // Without force the project and everything in it stays, and the message is the original's.
    let refused = rpc(&mut socket, 6, "projects.mutate", json!({"type":"project.delete","commandId":"delete-project","projectId":"project"})).await;
    assert_eq!(refused["_tag"], "Failure", "{refused}");
    assert_eq!(refused["cause"][0]["error"]["message"], "Project project is not empty.");
    assert!(fixture.store.projection("project", "project").unwrap().unwrap()["deletedAt"].is_null());
    assert!(["thread", "archived"].iter().all(|id| fixture.store.projection("thread", id).unwrap().unwrap()["thread"]["deletedAt"].is_null()));
    // Reading is not enough to force it either.
    let mut reader = fixture.connect(&fixture.reader).await;
    let forbidden = rpc(&mut reader, 1, "projects.mutate", json!({"type":"project.delete","commandId":"reader-delete","projectId":"project","force":true})).await;
    assert_eq!(forbidden["_tag"], "Failure");
    assert!(fixture.store.projection("project", "project").unwrap().unwrap()["deletedAt"].is_null());

    let deleted = success(&rpc(&mut socket, 7, "projects.mutate", json!({"type":"project.delete","commandId":"delete-project","projectId":"project","force":true})).await).clone();
    assert!(deleted["deletedAt"].is_string(), "{deleted}");
    for id in ["thread", "archived"] {
        let thread = fixture.store.projection("thread", id).unwrap().unwrap();
        assert!(thread["thread"]["deletedAt"].is_string(), "{id}");
        fixture.execution.drained(id).await;
    }
    assert!(fixture.store.projection("project", "project").unwrap().unwrap()["deletedAt"].is_string());
    // The other project is untouched, and the finished turn's history is still there.
    assert!(fixture.store.projection("project", "other").unwrap().unwrap()["deletedAt"].is_null());
    assert!(fixture.store.projection("thread", "kept").unwrap().unwrap()["thread"]["deletedAt"].is_null());
    assert_eq!(fixture.store.projection("thread", "thread").unwrap().unwrap()["runs"][0]["status"], "completed");
    // The cascade is recorded under the project command's ids, and the same command replays.
    let thread_deletions: Vec<_> = fixture.store.events(0, None, Some("thread"), Some("thread"), 1000).unwrap().into_iter().filter(|event| event.event.event_type == "thread.deleted").collect();
    assert_eq!(thread_deletions.len(), 1);
    assert_eq!(thread_deletions[0].event.command_id.as_deref(), Some("delete-project:delete-thread:thread"));
    let latest = fixture.store.latest_sequence().unwrap();
    success(&rpc(&mut socket, 8, "projects.mutate", json!({"type":"project.delete","commandId":"delete-project","projectId":"project","force":true})).await);
    assert_eq!(fixture.store.latest_sequence().unwrap(), latest);
    // A project with no threads left deletes without a cascade.
    success(&rpc(&mut socket, 9, "projects.mutate", json!({"type":"project.delete","commandId":"delete-other-later","projectId":"other","force":true})).await);
    fixture.finish().await;
}

#[tokio::test]
async fn deleting_a_thread_while_its_setup_runs_stops_the_wait_and_keeps_the_worktree() {
    let fixture = fixture(setup_script("echo started; sleep 120", true)).await;
    let mut socket = fixture.connect(&fixture.operator).await;
    let mut watcher = subscribe(&fixture, "thread").await;
    let mut terminal_events = fixture.terminals.events();
    let closed = tokio::spawn(async move {
        while let Ok(Some(event)) = terminal_events.recv().await {
            if event["type"] == "closed" && event["terminalId"] == "setup-setup" {
                return true;
            }
        }
        false
    });
    launch(&fixture, &mut socket, json!({"type":"worktree","baseRef":"main"}), true).await;
    let running = stream_until(&mut watcher, 50, |value| {
        value["stages"].as_array().is_some_and(|stages| stages.iter().any(|stage| stage["id"] == "setup-script" && stage["tail"].as_array().is_some_and(|tail| tail.iter().any(|line| line == "started"))))
    })
    .await;
    let path = running["worktreePath"].as_str().unwrap().to_owned();
    assert!(success(&command(&mut socket, 2, delete_command("thread", "delete")).await)["sequence"].is_number());
    // The preparation gives up waiting, so the cleanup queued behind it can close the shell.
    fixture.execution.drained("thread").await;
    assert!(closed.await.unwrap());
    stream_until(&mut watcher, 50, |value| value["phase"] == "cancelled").await;
    let view = fixture.store.projection("thread", "thread").unwrap().unwrap();
    assert!(run_status(&view, "cancelled"), "{}", view["runs"]);
    assert!(view["thread"]["deletedAt"].is_string());
    // A deleted thread keeps its worktree, and no agent was started in it.
    assert!(std::path::Path::new(&path).exists());
    assert_eq!(view["thread"]["worktreePath"], path.as_str());
    assert!(fixture.provider_requests("thread/start").is_empty());
    fixture.finish().await;
}
