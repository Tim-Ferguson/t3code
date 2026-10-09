#![cfg(unix)]
#![allow(dead_code)]
//! Turn checkpoints end to end: a real turn over the WebSocket transport in a temporary git
//! repository, the persisted events and projection, the hidden refs in the repository, and the
//! two diff RPCs with their error envelopes and permissions.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::{Store, StoredEvent},
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
    directory: tempfile::TempDir,
    address: std::net::SocketAddr,
    store: Store,
    execution: ExecutionService,
    thread_id: String,
    operator: String,
    outsider: String,
    reader: String,
    record: std::path::PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness(git: bool) -> Harness {
    harness_for("codex", git).await
}

/// `codex` runs the recording Codex fixture; `acp` runs the registry-agent fixture.
async fn harness_for(driver: &str, git: bool) -> Harness {
    harness_with(driver, git, false).await
}

/// `worktree` launches the thread in its own git worktree of the project, the only workspace
/// whose files a checkpoint rollback may restore.
async fn harness_with(driver: &str, git: bool, worktree: bool) -> Harness {
    harness_env(driver, git, worktree, &[]).await
}

/// `environment` is extra provider-process environment, such as the fixture's legacy-history switch.
async fn harness_env(driver: &str, git: bool, worktree: bool, environment: &[(&str, &str)]) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    if git {
        run_git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
        run_git(&workspace, &["add", "."]);
        run_git(&workspace, &["commit", "-q", "-m", "initial"]);
        if worktree {
            run_git(&workspace, &["worktree", "add", "-q", "-b", "feature", cwd.join("worktree").to_str().unwrap()]);
        }
    }
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let record = cwd.join("codex-requests.jsonl");
    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let acp =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
    let mut provider_environment = vec![json!({"name":"FIXTURE_RPC_RECORD","value":record})];
    provider_environment.extend(environment.iter().map(|(name, value)| json!({"name":name,"value":value})));
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":driver=="codex","config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":provider_environment},
        "local-agent":{"driver":"acpRegistry","enabled":driver=="acp","config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[acp,"2","normal"]}}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread_id = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":if driver=="acp"{"local-agent"}else{"codex"},"model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":if worktree{json!({"type":"existing_worktree","worktreePath":cwd.join("worktree")})}else{json!({"type":"root"})}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(
        store.clone(),
        [7; 32],
        "t3_session_attachments".into(),
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
    ]);
    let outsider = issue(vec![AuthEnvironmentScope::TerminalRead]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"attachments-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    Harness {
        directory,
        address,
        store,
        execution,
        thread_id,
        operator,
        outsider,
        reader,
        record,
        stop: Some(stop),
        server: Some(server),
    }
}

impl Harness {
    fn attachments(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.directory.path())
            .unwrap()
            .join("attachments")
    }
    fn listing(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.attachments())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .filter(|name| name.starts_with(prefix))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    fn url(&self, relative: &str) -> String {
        format!("http://{}{relative}", self.address)
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                .to_string()
                .into(),
        ))
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
fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    &exit["cause"][0]
}


fn run_git(cwd: &std::path::Path, args: &[&str]) -> String {
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

/// Waits on committed events until every run is terminal and the given number exist.
async fn settled(
    store: &Store,
    events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>,
    thread: &str,
    runs: usize,
) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let view = store.projection("thread", thread).unwrap().unwrap();
            last = view["runs"].clone();
            if view["runs"].as_array().unwrap().len() >= runs
                && view["runs"].as_array().unwrap().iter().all(|run| {
                    matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "rolled_back"))
                })
            {
                return view;
            }
            events.recv().await.unwrap();
        }
    })
    .await;
    outcome.unwrap_or_else(|_| panic!("the provider turn did not settle: {last}"))
}

fn dispatch(thread: &str, id: &str) -> Value {
    json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":format!("message:{id}"),"text":"Do the work","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})
}

/// One turn: dispatch, then wait for the checkpoint effect to complete the run.
async fn turn(harness: &Harness, socket: &mut Socket, id: u64, runs: usize) -> Value {
    let mut events = harness.store.subscribe();
    let exit = rpc(socket, id, "orchestration.dispatchCommand", dispatch(&harness.thread_id, &format!("send-{id}"))).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    settled(&harness.store, &mut events, &harness.thread_id, runs).await
}

/// The registry-agent fixture asks for approval on every prompt; accept it so the turn ends.
async fn approved_turn(harness: &Harness, socket: &mut Socket, id: u64, runs: usize) -> Value {
    let mut events = harness.store.subscribe();
    let exit = rpc(socket, id, "orchestration.dispatchCommand", dispatch(&harness.thread_id, &format!("send-{id}"))).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    let request = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let view = harness.store.projection("thread", &harness.thread_id).unwrap().unwrap();
            if let Some(request) = view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending") {
                return request.clone();
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect("the agent never asked for approval");
    let answer = json!({"type":"runtime-request.respond","commandId":format!("approve-{id}"),"threadId":harness.thread_id,"requestId":request["id"],"decision":"accept"});
    assert!(success(&rpc(socket, id + 1000, "orchestration.dispatchCommand", answer).await)["sequence"].is_number());
    settled(&harness.store, &mut events, &harness.thread_id, runs).await
}

fn checkpoint<'a>(view: &'a Value, ordinal: u64) -> &'a Value {
    view["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal)
        .unwrap_or_else(|| panic!("no checkpoint {ordinal}: {}", view["checkpoints"]))
}

fn refs(workspace: &std::path::Path) -> Vec<String> {
    run_git(workspace, &["for-each-ref", "--format=%(refname)", "refs/t3/orchestration-v2/checkpoints"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn capture_events(harness: &Harness, run_id: &str) -> Vec<StoredEvent> {
    harness
        .store
        .events(0, None, Some("thread"), Some(&harness.thread_id), 10_000)
        .unwrap()
        .into_iter()
        .filter(|event| event.event.command_id.as_deref() == Some(&format!("command:effect:checkpoint.capture:{run_id}")))
        .collect()
}

async fn diff(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    rpc(socket, id, tag, payload).await
}

#[tokio::test]
async fn codex_turns_record_checkpoints_and_serve_diffs() {
    let harness = harness(true).await;
    let workspace = std::fs::canonicalize(harness.directory.path()).unwrap().join("workspace");
    let mut socket = harness.connect(&harness.operator).await;

    // Turn 1 changes nothing: the baseline and the first checkpoint are the same tree.
    let view = turn(&harness, &mut socket, 1, 1).await;
    let run = &view["runs"][0];
    assert_eq!(run["status"], "completed");
    assert!(run["completedAt"].is_string());
    let scope = &view["checkpointScopes"][0];
    assert_eq!(scope["kind"], "root_run");
    assert_eq!(scope["cwd"], json!(workspace));
    assert_eq!(view["nodes"][0]["checkpointScopeId"], scope["id"]);
    assert_eq!(view["nodes"][0]["status"], "completed");
    let first = checkpoint(&view, 1);
    assert_eq!(run["checkpointId"], first["id"]);
    assert_eq!((first["status"].as_str(), first["appRunOrdinal"].as_u64(), first["files"].clone()), (Some("ready"), Some(1), json!([])));
    assert_eq!(first["parentCheckpointId"], checkpoint(&view, 0)["id"]);
    assert_eq!(checkpoint(&view, 0)["status"], "ready");
    assert!(checkpoint(&view, 0)["appRunOrdinal"].is_null());
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "checkpoint").unwrap();
    assert_eq!(item["checkpointId"], first["id"]);
    assert_eq!(item["ordinal"], 199);
    // The capture is one command: the baseline checkpoint, the turn's, then the run settles.
    let types: Vec<_> = capture_events(&harness, run["id"].as_str().unwrap()).iter().map(|event| event.event.event_type.clone()).collect();
    assert_eq!(types, ["checkpoint.captured", "checkpoint.captured", "turn-item.updated", "run.updated", "node.updated"]);
    let first_refs = refs(&workspace);
    assert_eq!(first_refs.len(), 2, "{first_refs:?}");
    for checkpoint in view["checkpoints"].as_array().unwrap() {
        run_git(&workspace, &["rev-parse", "--verify", checkpoint["ref"].as_str().unwrap()]);
    }

    // Turn 2 follows a user edit: a new file and a modified tracked file.
    std::fs::write(workspace.join("hello.txt"), "world\n").unwrap();
    std::fs::write(workspace.join("README.md"), "hello\nmore\n").unwrap();
    let view = turn(&harness, &mut socket, 2, 2).await;
    let second = checkpoint(&view, 2);
    assert_eq!(second["status"], "ready");
    assert_eq!(second["parentCheckpointId"], first["id"]);
    let files: Vec<_> = second["files"].as_array().unwrap().iter().map(|file| (file["path"].as_str().unwrap(), file["additions"].as_u64().unwrap(), file["deletions"].as_u64().unwrap())).collect();
    assert_eq!(files, [("hello.txt", 1, 0), ("README.md", 1, 0)]);
    assert_eq!(view["runs"][1]["checkpointId"], second["id"]);
    assert_eq!(view["checkpointScopes"].as_array().unwrap().len(), 1, "one root scope per thread");
    assert_eq!(refs(&workspace).len(), 3);
    // The snapshot never touches the user's index or branch.
    assert_eq!(run_git(&workspace, &["show", &format!("{}:hello.txt", second["ref"].as_str().unwrap())]), "world\n");
    let status = run_git(&workspace, &["status", "--porcelain"]);
    assert!(status.contains(" M README.md") && status.contains("?? hello.txt"), "{status}");

    // Turn 3.
    std::fs::write(workspace.join("hello.txt"), "world\nagain\n").unwrap();
    let view = turn(&harness, &mut socket, 3, 3).await;
    assert_eq!(checkpoint(&view, 3)["files"].as_array().unwrap().len(), 1);

    // Diff RPCs.
    let turn_diff = |from: u64, to: u64| json!({"threadId":harness.thread_id,"fromTurnCount":from,"toTurnCount":to});
    let value = |exit: Value| success(&exit).clone();
    let second_turn = value(diff(&mut socket, 10, "orchestration.getTurnDiff", turn_diff(1, 2)).await);
    assert_eq!((second_turn["fromTurnCount"].clone(), second_turn["toTurnCount"].clone()), (json!(1), json!(2)));
    let patch = second_turn["diff"].as_str().unwrap();
    assert!(patch.contains("diff --git a/hello.txt b/hello.txt") && patch.contains("+world") && patch.contains("+more"), "{patch}");
    assert!(!patch.contains("+again"));
    let third_turn = value(diff(&mut socket, 11, "orchestration.getTurnDiff", turn_diff(2, 3)).await);
    assert!(third_turn["diff"].as_str().unwrap().contains("+again") && !third_turn["diff"].as_str().unwrap().contains("README"));
    // Turn count 0 is the baseline before the first turn.
    let whole = value(diff(&mut socket, 12, "orchestration.getFullThreadDiff", json!({"threadId":harness.thread_id,"toTurnCount":3})).await);
    assert_eq!(whole["fromTurnCount"], 0);
    let whole_patch = whole["diff"].as_str().unwrap();
    assert!(whole_patch.contains("README.md") && whole_patch.contains("+world") && whole_patch.contains("+again"), "{whole_patch}");
    let same = value(diff(&mut socket, 13, "orchestration.getTurnDiff", turn_diff(2, 2)).await);
    assert_eq!(same["diff"], "");
    let empty = value(diff(&mut socket, 14, "orchestration.getFullThreadDiff", json!({"threadId":harness.thread_id,"toTurnCount":0})).await);
    assert_eq!(empty["diff"], "");
    // ignoreWhitespace defaults to true and can be turned off.
    std::fs::write(workspace.join("hello.txt"), "world\nagain\n").unwrap();

    // Errors keep the original envelopes.
    let beyond = diff(&mut socket, 15, "orchestration.getTurnDiff", turn_diff(0, 9)).await;
    let error = failure(&beyond)["error"].clone();
    assert_eq!(error["_tag"], "OrchestrationGetTurnDiffError");
    assert_eq!(error["message"], "Failed to load turn diff");
    assert_eq!(error["cause"]["name"], "CheckpointTurnRangeUnavailableError");
    assert!(error["cause"]["message"].as_str().unwrap().ends_with("requested 9, current 3."));
    let unknown = diff(&mut socket, 16, "orchestration.getFullThreadDiff", json!({"threadId":"thread:missing","toTurnCount":1})).await;
    let error = failure(&unknown)["error"].clone();
    assert_eq!(error["_tag"], "OrchestrationGetFullThreadDiffError");
    assert_eq!(error["cause"]["name"], "CheckpointThreadNotFoundError");
    // An inverted range fails decoding, as in the source schema.
    let inverted = diff(&mut socket, 17, "orchestration.getTurnDiff", turn_diff(3, 1)).await;
    assert_eq!(inverted["_tag"], "Failure");
    assert_ne!(failure(&inverted)["error"]["_tag"], "OrchestrationGetTurnDiffError");

    // Diff reads need orchestration:read.
    let mut outsider = harness.connect(&harness.outsider).await;
    for (id, tag, payload) in [(20, "orchestration.getTurnDiff", turn_diff(1, 2)), (21, "orchestration.getFullThreadDiff", json!({"threadId":harness.thread_id,"toTurnCount":1}))] {
        let denied = rpc(&mut outsider, id, tag, payload).await;
        assert_eq!(failure(&denied)["error"]["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&denied)["error"]["requiredPermission"], "orchestration:read");
    }
    harness.finish().await;
}

#[tokio::test]
async fn a_project_outside_git_completes_turns_with_a_missing_checkpoint() {
    let harness = harness(false).await;
    let mut socket = harness.connect(&harness.operator).await;
    let view = turn(&harness, &mut socket, 1, 1).await;
    assert_eq!(view["runs"][0]["status"], "completed");
    assert_eq!(view["nodes"][0]["status"], "completed");
    let first = checkpoint(&view, 1);
    assert_eq!((first["status"].as_str(), first["files"].clone()), (Some("missing"), json!([])));
    assert_eq!(checkpoint(&view, 0)["status"], "missing");
    assert_eq!(view["runs"][0]["checkpointId"], first["id"]);
    // There is nothing to diff, and the error says so rather than failing the connection.
    let exit = rpc(&mut socket, 2, "orchestration.getTurnDiff", json!({"threadId":harness.thread_id,"fromTurnCount":0,"toTurnCount":1})).await;
    assert_eq!(failure(&exit)["error"]["cause"]["name"], "CheckpointTurnRangeUnavailableError");
    harness.finish().await;
}

#[tokio::test]
async fn acp_turns_record_checkpoints_too() {
    let harness = harness_for("acp", true).await;
    let workspace = std::fs::canonicalize(harness.directory.path()).unwrap().join("workspace");
    let mut socket = harness.connect(&harness.operator).await;
    let view = approved_turn(&harness, &mut socket, 1, 1).await;
    assert_eq!(view["runs"][0]["status"], "completed");
    assert_eq!(checkpoint(&view, 1)["status"], "ready");
    assert_eq!(view["runs"][0]["checkpointId"], checkpoint(&view, 1)["id"]);
    assert_eq!(refs(&workspace).len(), 2);
    std::fs::write(workspace.join("agent.txt"), "from the agent\n").unwrap();
    let view = approved_turn(&harness, &mut socket, 2, 2).await;
    assert_eq!(checkpoint(&view, 2)["files"][0]["path"], "agent.txt");
    let exit = rpc(&mut socket, 4, "orchestration.getTurnDiff", json!({"threadId":harness.thread_id,"fromTurnCount":1,"toTurnCount":2})).await;
    assert!(success(&exit)["diff"].as_str().unwrap().contains("+from the agent"));
    harness.finish().await;
}

// ---- rollback ----------------------------------------------------------------------------

fn rollback(harness: &Harness, id: &str, scope: &Value, checkpoint: &Value, restore_files: Option<bool>) -> Value {
    let mut command = json!({"type":"checkpoint.rollback","commandId":id,"threadId":harness.thread_id,"scopeId":scope,"checkpointId":checkpoint});
    if let Some(restore) = restore_files {
        command["restoreFiles"] = json!(restore);
    }
    command
}

fn view(harness: &Harness) -> Value {
    harness.store.projection("thread", &harness.thread_id).unwrap().unwrap()
}

/// The rollback effect has committed its result: the command that applies it has a receipt.
async fn rolled_back(harness: &Harness, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, command: &str) -> Value {
    let provider_thread = view(harness)["thread"]["activeProviderThreadId"].as_str().unwrap().to_owned();
    let effect_id = |checkpoint: &str| format!("effect:{command}:provider-thread.rollback:{provider_thread}:{checkpoint}");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let projection = view(harness);
            let applied = projection["checkpoints"].as_array().unwrap().iter().any(|checkpoint| {
                harness.store.receipt(&format!("command:effect:{}:applied", effect_id(checkpoint["id"].as_str().unwrap()))).unwrap().is_some()
            });
            if applied || projection["thread"]["rollbackFailure"].is_object() {
                return projection;
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect("the rollback never finished")
}

async fn dispatch_error(socket: &mut Socket, id: u64, command: Value) -> String {
    let exit = rpc(socket, id, "orchestration.dispatchCommand", command).await;
    // Admission failures reach the client as the original's dispatch error, with the bare
    // reason as the message.
    let error = &failure(&exit)["error"];
    assert_eq!(error["_tag"], "OrchestrationV2DispatchCommandError", "{exit}");
    assert_eq!(error["detail"], error["message"], "{exit}");
    assert!(!error["message"].as_str().unwrap().starts_with("Invalid command"), "{exit}");
    error["message"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn rolling_back_to_the_latest_checkpoint_restores_the_worktree_without_touching_the_provider() {
    let harness = harness_with("codex", true, true).await;
    let worktree = std::fs::canonicalize(harness.directory.path()).unwrap().join("worktree");
    let mut socket = harness.connect(&harness.operator).await;
    turn(&harness, &mut socket, 1, 1).await;
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    let after_turns = turn(&harness, &mut socket, 2, 2).await;
    let (head, status, branch) = (run_git(&worktree, &["rev-parse", "HEAD"]), run_git(&worktree, &["status", "--porcelain"]), run_git(&worktree, &["branch", "--show-current"]));
    assert_eq!(status, "?? a.txt\n");
    assert_eq!(refs(&worktree).len(), 3);

    // The user edits after the last turn; rolling back to that turn's checkpoint undoes it.
    std::fs::write(worktree.join("a.txt"), "edited\n").unwrap();
    std::fs::write(worktree.join("junk.txt"), "junk\n").unwrap();
    std::fs::write(worktree.join("README.md"), "changed\n").unwrap();
    let (scope, latest) = (after_turns["checkpointScopes"][0]["id"].clone(), checkpoint(&after_turns, 2)["id"].clone());
    let mut events = harness.store.subscribe();
    let exit = rpc(&mut socket, 3, "orchestration.dispatchCommand", rollback(&harness, "rollback-latest", &scope, &latest, None)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    let projection = rolled_back(&harness, &mut events, "rollback-latest").await;

    assert_eq!(std::fs::read_to_string(worktree.join("a.txt")).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(worktree.join("README.md")).unwrap(), "hello\n");
    assert!(!worktree.join("junk.txt").exists());
    // The user's repository is as it was: same commit and branch, nothing staged.
    assert_eq!(run_git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(run_git(&worktree, &["branch", "--show-current"]), branch);
    assert_eq!(run_git(&worktree, &["status", "--porcelain"]), status);
    // Nothing follows the target: no checkpoint goes stale, no run is rolled back, no ref is deleted.
    assert!(projection["checkpoints"].as_array().unwrap().iter().all(|checkpoint| checkpoint["status"] == "ready"));
    assert!(projection["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed"));
    assert_eq!(refs(&worktree).len(), 3);
    assert_eq!(projection["thread"]["rollbackRequestId"], "rollback-latest");
    assert!(projection["thread"]["rollbackFailure"].is_null());
    assert_eq!(projection["providerThreads"][0]["lastRunOrdinal"], 2);
    let requested: Vec<_> = harness.store.events(0, None, Some("thread"), Some(&harness.thread_id), 10_000).unwrap().into_iter()
        .filter(|event| event.event.command_id.as_deref() == Some("rollback-latest")).map(|event| event.event.event_type).collect();
    assert_eq!(requested, ["thread.metadata-updated", "checkpoint.rollback-requested"]);
    harness.finish().await;
}

#[tokio::test]
async fn rollback_refusals_name_their_reason_and_change_nothing() {
    // Not isolated: a thread in the project's own checkout may share it with other work.
    let harness = harness_with("codex", true, false).await;
    let mut socket = harness.connect(&harness.operator).await;
    let view_1 = turn(&harness, &mut socket, 1, 1).await;
    let (scope, first) = (view_1["checkpointScopes"][0]["id"].clone(), checkpoint(&view_1, 1)["id"].clone());
    let before = harness.store.latest_sequence().unwrap();
    let shared = dispatch_error(&mut socket, 2, rollback(&harness, "shared", &scope, &first, None)).await;
    assert!(shared.contains("File restore requires an isolated worktree."), "{shared}");
    assert_eq!(harness.store.latest_sequence().unwrap(), before, "a refusal writes no events");
    assert!(view(&harness)["thread"]["rollbackRequestId"].is_null());

    // Wrong scope, unknown checkpoint.
    let wrong = dispatch_error(&mut socket, 3, rollback(&harness, "wrong-scope", &json!("scope:other"), &first, Some(false))).await;
    assert!(wrong.contains(&format!("belongs to scope {}, not scope:other.", scope.as_str().unwrap())), "{wrong}");
    let unknown = dispatch_error(&mut socket, 4, rollback(&harness, "unknown", &scope, &json!("checkpoint:nope"), Some(false))).await;
    assert!(unknown.contains("Checkpoint checkpoint:nope was not found."), "{unknown}");

    // A conversation-only rewind to a ready checkpoint is allowed anywhere.
    let allowed = rpc(&mut socket, 5, "orchestration.dispatchCommand", rollback(&harness, "conversation-only", &scope, &first, Some(false))).await;
    assert!(success(&allowed)["sequence"].is_number(), "{allowed}");

    // Permissions: rolling back needs orchestration:operate.
    let mut reader = harness.connect(&harness.reader).await;
    let denied = rpc(&mut reader, 6, "orchestration.dispatchCommand", rollback(&harness, "denied", &scope, &first, Some(false))).await;
    assert_eq!(failure(&denied)["error"]["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(failure(&denied)["error"]["requiredPermission"], "orchestration:operate");
    assert!(harness.store.receipt("denied").unwrap().is_none());
    harness.finish().await;

    // Another thread working in the same worktree makes a restore unsafe.
    let harness = harness_with("codex", true, true).await;
    let worktree = std::fs::canonicalize(harness.directory.path()).unwrap().join("worktree");
    let mut socket = harness.connect(&harness.operator).await;
    let view_1 = turn(&harness, &mut socket, 1, 1).await;
    let (scope, first) = (view_1["checkpointScopes"][0]["id"].clone(), checkpoint(&view_1, 1)["id"].clone());
    ThreadLaunchService::new(harness.store.clone())
        .launch(json!({"commandId":"launch-neighbour","projectId":"project","title":"Neighbour","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"existing_worktree","worktreePath":worktree}}), Utc::now())
        .unwrap();
    let neighbour = dispatch_error(&mut socket, 2, rollback(&harness, "neighbour", &scope, &first, None)).await;
    assert!(neighbour.contains("File restore requires an isolated worktree."), "{neighbour}");
    harness.finish().await;

    // A checkpoint that was never captured cannot be restored.
    let harness = harness_with("codex", false, false).await;
    let mut socket = harness.connect(&harness.operator).await;
    let view_1 = turn(&harness, &mut socket, 1, 1).await;
    let (scope, missing) = (view_1["checkpointScopes"][0]["id"].clone(), checkpoint(&view_1, 1)["id"].clone());
    let refused = dispatch_error(&mut socket, 2, rollback(&harness, "missing", &scope, &missing, Some(false))).await;
    assert!(refused.contains("is missing and cannot be restored."), "{refused}");
    harness.finish().await;
}

fn recorded(harness: &Harness) -> Vec<Value> {
    std::fs::read_to_string(&harness.record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn rolling_back_past_a_turn_rewinds_codex_restores_files_and_marks_later_work() {
    let harness = harness_with("codex", true, true).await;
    let worktree = std::fs::canonicalize(harness.directory.path()).unwrap().join("worktree");
    let mut socket = harness.connect(&harness.operator).await;
    let first = turn(&harness, &mut socket, 1, 1).await;
    let (head, status) = (run_git(&worktree, &["rev-parse", "HEAD"]), run_git(&worktree, &["status", "--porcelain"]));
    std::fs::write(worktree.join("b.txt"), "two\n").unwrap();
    let second = turn(&harness, &mut socket, 2, 2).await;
    std::fs::write(worktree.join("README.md"), "edited after turn two\n").unwrap();
    let (scope, target, stale) = (first["checkpointScopes"][0]["id"].clone(), checkpoint(&first, 1).clone(), checkpoint(&second, 2).clone());
    assert_eq!(refs(&worktree).len(), 3);

    let mut events = harness.store.subscribe();
    let exit = rpc(&mut socket, 3, "orchestration.dispatchCommand", rollback(&harness, "rewind", &scope, &target["id"], None)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    let projection = rolled_back(&harness, &mut events, "rewind").await;

    // Files are the first turn's again; the user's repository is untouched.
    assert_eq!(std::fs::read_to_string(worktree.join("README.md")).unwrap(), "hello\n");
    assert!(!worktree.join("b.txt").exists());
    assert_eq!((run_git(&worktree, &["rev-parse", "HEAD"]), run_git(&worktree, &["status", "--porcelain"])), (head, status));
    // The later checkpoint is stale and its ref is gone; the baseline and target stay.
    let by_ordinal = |ordinal: u64| checkpoint(&projection, ordinal);
    assert_eq!((by_ordinal(0)["status"].as_str(), by_ordinal(1)["status"].as_str(), by_ordinal(2)["status"].as_str()), (Some("ready"), Some("ready"), Some("stale")));
    assert_eq!(by_ordinal(2)["id"], stale["id"]);
    assert_eq!(by_ordinal(2)["files"], stale["files"], "a stale checkpoint keeps its record");
    let remaining = refs(&worktree);
    assert_eq!(remaining.len(), 2, "{remaining:?}");
    assert!(!remaining.contains(&stale["ref"].as_str().unwrap().to_owned()));
    // Run 2 is audit history now; run 1 is untouched.
    let (run_1, run_2) = (&projection["runs"][0], &projection["runs"][1]);
    assert_eq!((run_1["status"].as_str(), run_2["status"].as_str()), (Some("completed"), Some("rolled_back")));
    assert!(run_2["completedAt"].is_string());
    let node = projection["nodes"].as_array().unwrap().iter().find(|node| node["id"] == run_2["rootNodeId"]).unwrap();
    assert_eq!(node["status"], "rolled_back");
    let provider_thread = &projection["providerThreads"][0];
    assert_eq!((provider_thread["lastRunOrdinal"].clone(), provider_thread["status"].clone()), (json!(1), json!("idle")));
    let first_turn = projection["providerTurns"].as_array().unwrap().iter().find(|turn| turn["ordinal"] == 1).unwrap();
    assert_eq!(provider_thread["nativeConversationHeadRef"], first_turn["nativeTurnRef"]);
    assert!(projection["thread"]["rollbackFailure"].is_null());
    // Codex was asked to drop exactly the second turn.
    let second_native = projection["providerTurns"].as_array().unwrap().iter().find(|turn| turn["ordinal"] == 2).unwrap()["nativeTurnRef"]["nativeId"].clone();
    let calls = recorded(&harness);
    let methods: Vec<_> = calls.iter().map(|call| call["method"].as_str().unwrap()).skip_while(|method| *method != "thread/read").collect();
    assert_eq!(methods, ["thread/read", "thread/turns/list", "thread/revert"]);
    let list = calls.iter().find(|call| call["method"] == "thread/turns/list").unwrap();
    assert_eq!((list["params"]["limit"].clone(), list["params"]["sortDirection"].clone()), (json!(1), json!("desc")));
    assert_eq!(calls.iter().find(|call| call["method"] == "thread/revert").unwrap()["params"]["beforeTurnId"], second_native);

    // The conversation continues from the target: a new run takes the next ordinal and its
    // baseline is re-captured at the rolled-back state.
    std::fs::write(worktree.join("c.txt"), "three\n").unwrap();
    let third = turn(&harness, &mut socket, 4, 3).await;
    assert_eq!(third["runs"][2]["ordinal"], 3);
    let files: Vec<_> = checkpoint(&third, 3)["files"].as_array().unwrap().iter().map(|file| file["path"].as_str().unwrap().to_owned()).collect();
    // The edit predates the new run, so its re-captured baseline already contains it; a diff
    // from the rollback target still shows it.
    assert!(files.is_empty(), "{files:?}");
    let diff = success(&rpc(&mut socket, 5, "orchestration.getTurnDiff", json!({"threadId":harness.thread_id,"fromTurnCount":1,"toTurnCount":3})).await).clone();
    assert!(diff["diff"].as_str().unwrap().contains("+three") && !diff["diff"].as_str().unwrap().contains("b.txt"));
    let gone = rpc(&mut socket, 6, "orchestration.getTurnDiff", json!({"threadId":harness.thread_id,"fromTurnCount":1,"toTurnCount":2})).await;
    assert_eq!(failure(&gone)["error"]["cause"]["name"], "CheckpointRefUnavailableError");
    harness.finish().await;
}

#[tokio::test]
async fn a_rollback_that_fails_every_retry_is_recorded_on_the_thread_and_changes_nothing() {
    // Codex 0.156 cannot revert legacy history.
    let harness = harness_env("codex", true, true, &[("FIXTURE_LEGACY_HISTORY", "1")]).await;
    let worktree = std::fs::canonicalize(harness.directory.path()).unwrap().join("worktree");
    let mut socket = harness.connect(&harness.operator).await;
    let first = turn(&harness, &mut socket, 1, 1).await;
    std::fs::write(worktree.join("b.txt"), "two\n").unwrap();
    turn(&harness, &mut socket, 2, 2).await;
    let (scope, target) = (first["checkpointScopes"][0]["id"].clone(), checkpoint(&first, 1)["id"].clone());
    let mut events = harness.store.subscribe();
    let exit = rpc(&mut socket, 3, "orchestration.dispatchCommand", rollback(&harness, "doomed", &scope, &target, None)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    assert!(view(&harness)["thread"]["rollbackFailure"].is_null(), "a pending rollback has not failed yet");
    let projection = rolled_back(&harness, &mut events, "doomed").await;
    assert_eq!(projection["thread"]["rollbackFailure"], json!({"requestId":"doomed","message":"The provider could not roll back this conversation. Try again; if it keeps failing, check the provider and server logs."}));
    // Nothing was restored, deleted or marked.
    assert!(worktree.join("b.txt").exists());
    assert_eq!(refs(&worktree).len(), 3);
    assert!(projection["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed"));
    assert!(projection["checkpoints"].as_array().unwrap().iter().all(|checkpoint| checkpoint["status"] == "ready"));
    // Five attempts, each reading the history mode first.
    assert_eq!(recorded(&harness).iter().filter(|call| call["method"] == "thread/read").count(), 5);
    // A newer rollback clears the failure when it is accepted.
    let again = rpc(&mut socket, 4, "orchestration.dispatchCommand", rollback(&harness, "again", &scope, &target, Some(false))).await;
    assert!(success(&again)["sequence"].is_number());
    let cleared = view(&harness);
    assert_eq!((cleared["thread"]["rollbackRequestId"].clone(), cleared["thread"]["rollbackFailure"].clone()), (json!("again"), Value::Null));
    harness.finish().await;
}

#[tokio::test]
async fn acp_rollback_starts_a_fresh_native_session() {
    let harness = harness_with("acp", true, true).await;
    let worktree = std::fs::canonicalize(harness.directory.path()).unwrap().join("worktree");
    let mut socket = harness.connect(&harness.operator).await;
    let first = approved_turn(&harness, &mut socket, 1, 1).await;
    std::fs::write(worktree.join("b.txt"), "two\n").unwrap();
    approved_turn(&harness, &mut socket, 2, 2).await;
    let (scope, target) = (first["checkpointScopes"][0]["id"].clone(), checkpoint(&first, 1)["id"].clone());
    let mut events = harness.store.subscribe();
    let exit = rpc(&mut socket, 3, "orchestration.dispatchCommand", rollback(&harness, "acp-rewind", &scope, &target, None)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    let projection = rolled_back(&harness, &mut events, "acp-rewind").await;
    assert!(!worktree.join("b.txt").exists());
    assert_eq!(projection["runs"][1]["status"], "rolled_back");
    assert_eq!(checkpoint(&projection, 2)["status"], "stale");
    // The old native session is retired; the thread holds no native binding until the next turn.
    let provider_thread = &projection["providerThreads"][0];
    assert!(provider_thread["nativeThreadRef"].is_null());
    assert_eq!(provider_thread["nativeMetadata"]["itemIdentityVersion"], 2);
    assert_eq!(provider_thread["lastRunOrdinal"], 1);
    let third = approved_turn(&harness, &mut socket, 4, 3).await;
    assert_eq!(third["runs"][2]["status"], "completed");
    assert!(third["providerThreads"][0]["nativeThreadRef"]["nativeId"].is_string(), "the next turn opened a new session");
    harness.finish().await;
}
