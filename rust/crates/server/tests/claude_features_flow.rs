#![cfg(unix)]
//! The Claude adapter's features beyond the turn path, end to end through the real WebSocket
//! transport against the fake `claude` (`fixtures/claude-provider.py`): conversation rollback,
//! usage limits and banked resets, the usage summary, skills, and importing Claude history. Every
//! provider is a fake at an absolute temp path or disabled, every directory a temp directory, and
//! the only network is loopback. Tests wait on persisted events, never on sleeps.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
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

// --- loopback HTTP --------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: String,
}
/// A loopback origin answering from `respond(method, target, body) -> (status, body)`.
struct Origin {
    address: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}
impl Origin {
    async fn start(respond: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let (recorded, respond) = (seen.clone(), Arc::new(respond));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let (recorded, respond) = (recorded.clone(), respond.clone());
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, length) = loop {
                        let count = stream.read(&mut chunk).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..count]);
                        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                            let length = head.lines().find_map(|line| line.strip_prefix("content-length:")).and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while buffer.len() < head_end + length {
                        let count = stream.read(&mut chunk).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..count]);
                    }
                    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                    let mut lines = head.lines();
                    let mut request = lines.next().unwrap_or("").split(' ');
                    let (method, target) = (request.next().unwrap_or("").to_owned(), request.next().unwrap_or("").to_owned());
                    let headers = lines.filter_map(|line| line.split_once(':')).map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned())).collect();
                    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();
                    let seen = Seen { method, target, headers, body };
                    let (status, payload) = respond(&seen);
                    recorded.lock().unwrap().push(seen);
                    let response = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}", payload.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { address, seen }
    }
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

// --- the fixture ----------------------------------------------------------------------------

struct Options {
    scenario: &'static str,
    worktree: bool,
    environment: Vec<(String, String)>,
    /// Written before the providers are discovered: `(path relative to the root, contents)`.
    files: Vec<(String, String)>,
}
impl Options {
    fn new(scenario: &'static str) -> Self {
        Self { scenario, worktree: false, environment: vec![], files: vec![] }
    }
    fn env(mut self, name: &str, value: impl Into<String>) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }
    fn file(mut self, relative: &str, contents: impl Into<String>) -> Self {
        self.files.push((relative.into(), contents.into()));
        self
    }
}

struct Fixture {
    root: PathBuf,
    /// The project's root, where the thread runs.
    workspace: PathBuf,
    claude_home: PathBuf,
    codex_home: PathBuf,
    worktree: bool,
    log: PathBuf,
    address: std::net::SocketAddr,
    store: Store,
    providers: ProviderRegistry,
    execution: ExecutionService,
    thread: String,
    operator: String,
    reader: String,
    outsider: String,
    next: usize,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

fn git(cwd: &Path, args: &[&str]) {
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
}
fn shim(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-provider.py");
    let binary = directory.join("claude");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

impl Fixture {
    async fn start(options: Options) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        for (relative, contents) in &options.files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents.replace("$ROOT", root.to_str().unwrap())).unwrap();
        }
        let (claude_home, codex_home) = (root.join("claude-home"), root.join("codex-home"));
        std::fs::create_dir_all(&claude_home).unwrap();
        let log = root.join("fixture.log");
        let (workspace, project_root, strategy) = if options.worktree {
            let repository = root.join("repository");
            std::fs::create_dir_all(&repository).unwrap();
            git(&repository, &["init", "-q"]);
            std::fs::write(repository.join("README.md"), "hello\n").unwrap();
            git(&repository, &["add", "."]);
            git(&repository, &["commit", "-q", "-m", "initial"]);
            git(&repository, &["worktree", "add", "-q", "-b", "feature", root.join("worktree").to_str().unwrap()]);
            (root.join("worktree"), repository, json!({"type":"existing_worktree","worktreePath":root.join("worktree")}))
        } else {
            let workspace = root.join("work");
            std::fs::create_dir_all(&workspace).unwrap();
            (workspace.clone(), workspace, json!({"type":"root"}))
        };
        let mut environment = vec![json!({"name":"CLAUDE_FIXTURE_SCENARIO","value":options.scenario}), json!({"name":"CLAUDE_FIXTURE_LOG","value":log})];
        environment.extend(options.environment.iter().map(|(name, value)| json!({"name":name,"value":value})));
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false,"config":{"homePath":codex_home}},
            "claudeAgent":{"driver":"claudeAgent","config":{"binaryPath":shim(&root),"homePath":claude_home},"environment":environment}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &workspace).await.unwrap();
        let state_dir = root.join("t3home").join("userdata");
        std::fs::create_dir_all(&state_dir).unwrap();
        let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":project_root}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"Claude","modelSelection":{"instanceId":"claudeAgent","model":"claude-sonnet-4-6"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":strategy}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let auth = AuthService::new(store.clone(), [7; 32], "t3_session_claude".into(), "loopback-browser".into()).unwrap();
        let issue = |scopes: Vec<AuthEnvironmentScope>| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
        let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::ProvidersManage, AuthEnvironmentScope::DiagnosticsRead]);
        let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
        let outsider = issue(vec![AuthEnvironmentScope::TerminalRead]);
        let state = ApiState {
            store: store.clone(),
            auth,
            environment: json!({"environmentId":"claude-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
        Self { root, workspace, claude_home, codex_home, worktree: options.worktree, log, address, store, providers, execution, thread, operator, reader, outsider, next: 0, stop: Some(stop), server: Some(server), _directory: directory }
    }

    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .expect("timed out waiting for a persisted event")
    }
    /// Sends a message over the socket and waits for run `ordinal` to end.
    async fn turn(&mut self, socket: &mut Socket, text: &str, ordinal: usize) -> Value {
        self.next += 1;
        let id = format!("message-{}", self.next);
        let exit = rpc(socket, 1000 + self.next as u64, "orchestration.dispatchCommand", json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        assert!(success(&exit)["sequence"].is_number(), "{exit}");
        let run = self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await;
        // The checkpoint capture settles through the same persisted state.
        if !self.worktree {
            return run;
        }
        self.until(|view| view["checkpoints"].as_array().unwrap().iter().any(|checkpoint| checkpoint["appRunOrdinal"] == ordinal && checkpoint["status"] == "ready").then_some(())).await;
        run
    }
    /// Like `turn`, for a thread whose workspace takes no checkpoints.
    async fn turn_without_checkpoint(&mut self, socket: &mut Socket, text: &str, ordinal: usize) -> Value {
        self.next += 1;
        let id = format!("message-{}", self.next);
        let exit = rpc(socket, 1000 + self.next as u64, "orchestration.dispatchCommand", json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        assert!(success(&exit)["sequence"].is_number(), "{exit}");
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await
    }
    fn launches(&self) -> Vec<Vec<String>> {
        self.entries().iter().filter_map(|entry| entry["launch"].as_array()).map(|args| args.iter().map(|arg| arg.as_str().unwrap().to_owned()).collect()).filter(|args: &Vec<String>| !args.contains(&"--no-session-persistence".to_string())).collect()
    }
    fn entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    fn snapshot(&self) -> Value {
        self.providers.snapshots().into_iter().find(|row| row["instanceId"] == "claudeAgent").unwrap()
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
    &exit["cause"][0]["error"]
}
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).cloned()
}
fn arg_value(args: &[String], prefix: &str) -> Option<String> {
    args.iter().find_map(|arg| arg.strip_prefix(prefix)).map(str::to_owned)
}

// --- rollback -------------------------------------------------------------------------------

fn rollback(fixture: &Fixture, id: &str, ordinal: u64) -> Value {
    let view = fixture.view();
    let scope = view["checkpointScopes"][0]["id"].clone();
    let target = view["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal).unwrap()["id"].clone();
    json!({"type":"checkpoint.rollback","commandId":id,"threadId":fixture.thread,"scopeId":scope,"checkpointId":target,"restoreFiles":false})
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_claude_conversation_resumes_at_the_assistant_message_of_the_target_turn() {
    let mut fixture = Fixture::start(Options { worktree: true, ..Options::new("turn") }).await;
    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    fixture.turn(&mut operator, "first", 1).await;
    fixture.turn(&mut operator, "second", 2).await;
    let view = fixture.view();
    let native = view["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    let first_cursor = view["providerTurns"][0]["nativeTurnRef"]["nativeId"].as_str().unwrap().to_owned();
    assert!(!first_cursor.starts_with("turn:"), "the turn recorded its last assistant message: {first_cursor}");

    // A reader cannot rewind; an operator can.
    let mut reader = fixture.connect(&fixture.reader.clone()).await;
    let denied = rpc(&mut reader, 1, "orchestration.dispatchCommand", rollback(&fixture, "denied", 1)).await;
    assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
    assert!(fixture.view()["thread"]["rollbackRequestId"].is_null());
    let exit = rpc(&mut operator, 2, "orchestration.dispatchCommand", rollback(&fixture, "rewind", 1)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    fixture.until(|view| (view["thread"]["rollbackFailure"].is_object() || view["runs"][1]["status"] == "rolled_back").then_some(())).await;
    let view = fixture.view();
    assert!(view["thread"]["rollbackFailure"].is_null(), "{}", view["thread"]["rollbackFailure"]);
    assert_eq!((view["runs"][0]["status"].as_str(), view["runs"][1]["status"].as_str()), (Some("completed"), Some("rolled_back")));
    let thread = &view["providerThreads"][0];
    assert_eq!(thread["nativeConversationHeadRef"], json!({"driver":"claudeAgent","nativeId":first_cursor,"strength":"weak"}));
    assert_eq!((thread["status"].as_str(), thread["lastRunOrdinal"].clone(), thread["nativeThreadRef"]["nativeId"].as_str()), (Some("idle"), json!(1), Some(native.as_str())));

    // The next turn reopens the same native session at that message, and finishing it clears the head.
    fixture.turn(&mut operator, "third", 3).await;
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "the rewind closed the process: {launches:?}");
    let reopened = &launches[1];
    assert!(reopened.contains(&format!("--resume={native}")), "{reopened:?}");
    assert_eq!(arg_value(reopened, "--resume-session-at=").as_deref(), Some(first_cursor.as_str()));
    fixture.until(|view| view["providerThreads"][0]["nativeConversationHeadRef"].is_null().then_some(())).await;

    // Rolling back to the start gives the thread a fresh native session, created rather than resumed.
    let exit = rpc(&mut operator, 3, "orchestration.dispatchCommand", rollback(&fixture, "to-start", 0)).await;
    assert!(success(&exit)["sequence"].is_number(), "{exit}");
    fixture.until(|view| (view["thread"]["rollbackFailure"].is_object() || view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "rolled_back")).then_some(())).await;
    let fresh = fixture.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    assert_ne!(fresh, native);
    fixture.turn(&mut operator, "again", 4).await;
    let launches = fixture.launches();
    let last = launches.last().unwrap();
    assert_eq!(arg_value(last, "--session-id=").as_deref(), Some(fresh.as_str()), "{last:?}");
    assert!(arg_value(last, "--resume=").is_none() && arg_value(last, "--resume-session-at=").is_none(), "{last:?}");
    let capabilities = &fixture.view()["providerSessions"][0]["capabilities"];
    assert_eq!((capabilities["threads"]["canRollbackThread"].clone(), capabilities["checkpointing"]["providerCanRollbackConversation"].clone()), (json!(true), json!(true)));
    fixture.finish().await;
}

// --- usage limits ---------------------------------------------------------------------------

fn usage_reply() -> String {
    json!({"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":10,"resets_at":"2030-01-01T05:00:00Z"},"seven_day":{"utilization":20,"resets_at":null},"model_scoped":[{"display_name":"Fable 5","utilization":30,"resets_at":"2030-01-05T00:00:00.000Z"}]}}).to_string()
}
fn window<'a>(snapshot: &'a Value, id: &str) -> &'a Value {
    snapshot["usageLimits"]["windows"].as_array().unwrap().iter().find(|window| window["id"] == id).unwrap_or_else(|| panic!("no window {id}: {}", snapshot["usageLimits"]))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_probe_draws_usage_windows_and_a_blocked_turn_announces_the_limit_once_and_fails_with_its_reset() {
    let mut fixture = Fixture::start(Options::new("limit").env("CLAUDE_FIXTURE_USAGE", usage_reply())).await;
    let snapshot = fixture.snapshot();
    let ids: Vec<&str> = snapshot["usageLimits"]["windows"].as_array().unwrap().iter().map(|window| window["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["five_hour", "seven_day", "seven_day_fable_5"]);
    assert_eq!((window(&snapshot, "five_hour")["usedPercent"].clone(), window(&snapshot, "five_hour")["resetsAt"].as_str()), (json!(10), Some("2030-01-01T05:00:00.000Z")));
    assert_eq!(window(&snapshot, "seven_day_fable_5")["label"], "Weekly · Fable 5");

    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    let run = fixture.turn(&mut operator, "work", 1).await;
    assert_eq!(run["status"], "failed");
    // The failure item is written just after the run settles.
    fixture.until(|view| view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "error").then_some(())).await;
    let view = fixture.view();
    let notices: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "system_notice").collect();
    assert_eq!(notices.len(), 1, "a repeated window is announced once: {notices:?}");
    assert_eq!(notices[0]["message"], "Claude usage limit reached. This turn is paused until the 5-hour limit resets in 1h 30m.");
    let error = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").unwrap();
    let failure = &error["failure"];
    assert_eq!((failure["class"].as_str(), failure["code"].as_str()), (Some("usage_limit"), Some("blocking_limit")));
    assert_eq!(failure["message"], "Claude stopped: a usage limit blocked the request.");
    let reset = failure["resetAt"].as_str().expect("the failure names when the limit resets");
    let wait = chrono::DateTime::parse_from_rfc3339(reset).unwrap().timestamp() - Utc::now().timestamp();
    assert!((5300..=5400).contains(&wait), "{reset} is {wait}s away");
    // The streamed event also moved the published window to its new reading.
    let updated = fixture.snapshot();
    assert_eq!(window(&updated, "five_hour")["usedPercent"], 100);
    assert_eq!(window(&updated, "seven_day")["usedPercent"], 20, "windows the event does not name keep their reading");
    fixture.finish().await;

    // A warning keeps the turn running and says nothing; a failed `get_usage` keeps the bars off.
    let mut fixture = Fixture::start(Options::new("warning").env("CLAUDE_FIXTURE_USAGE", usage_reply())).await;
    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    assert_eq!(fixture.turn(&mut operator, "work", 1).await["status"], "completed");
    assert!(fixture.view()["turnItems"].as_array().unwrap().iter().all(|item| item["type"] != "system_notice"));
    assert_eq!(window(&fixture.snapshot(), "seven_day")["usedPercent"], 90);
    fixture.finish().await;
    let fixture = Fixture::start(Options::new("turn").env("CLAUDE_FIXTURE_USAGE_FAIL", "1")).await;
    assert_eq!(fixture.snapshot()["usageLimits"]["unavailable"]["reason"], "probeFailed");
    fixture.finish().await;
    let fixture = Fixture::start(Options::new("turn")).await;
    assert_eq!(fixture.snapshot()["usageLimits"]["unavailable"]["reason"], "unsupported");
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn banked_resets_are_read_from_the_oauth_endpoint_and_redeemed_with_a_stable_request_id() {
    let future = (Utc::now() + chrono::Duration::days(30)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let outcome = Arc::new(Mutex::new("unavailable".to_owned()));
    let origin = {
        let (future, outcome) = (future.clone(), outcome.clone());
        Origin::start(move |request| {
            if request.target.starts_with("/api/oauth/usage") {
                (200, json!({"cedar_ember":{"eligible":true,"grants":[{"id":"g_1","resets_left":2,"usable_now":true,"ends_at":future}],"next_grant_id":"g_1"}}).to_string())
            } else if request.target == "/api/organizations/org-1/reset_rate_limits" {
                (200, json!({"result":outcome.lock().unwrap().clone()}).to_string())
            } else {
                (404, "{}".into())
            }
        })
        .await
    };
    let options = Options::new("turn")
        .env("CLAUDE_FIXTURE_USAGE", usage_reply())
        .env("T3CODE_CLAUDE_API_BASE", origin.url())
        .env("T3CODE_HOST_PLATFORM", "linux")
        .file("claude-home/.credentials.json", json!({"claudeAiOauth":{"accessToken":"token-1"}}).to_string())
        .file("claude-home/.claude.json", json!({"oauthAccount":{"organizationUuid":"org-1"}}).to_string());
    let fixture = Fixture::start(options).await;
    let credits = fixture.snapshot()["usageLimits"]["resetCredits"].clone();
    assert_eq!(credits, json!({"availableCount":2,"nextExpiresAt":format!("{}.000Z", future.trim_end_matches('Z')),"nextCreditId":"g_1"}));
    let read = origin.requests().into_iter().find(|request| request.target.starts_with("/api/oauth/usage")).unwrap();
    assert_eq!((read.method.as_str(), read.headers["authorization"].as_str(), read.headers["anthropic-beta"].as_str()), ("GET", "Bearer token-1", "oauth-2025-04-20"));
    assert!(read.headers["user-agent"].starts_with("claude-cli/2.1.111 "), "{:?}", read.headers);
    assert!(read.target.contains("cedar_ember=1") && read.target.contains("skip_spend=1"), "{}", read.target);

    // Spending a credit needs provider management; reading the snapshot does not.
    let mut reader = fixture.connect(&fixture.reader.clone()).await;
    let denied = rpc(&mut reader, 1, "provider.consumeResetCredit", json!({"instanceId":"claudeAgent"})).await;
    assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    // Claude could not confirm: the failure says so and the retry is the same claim.
    let unconfirmed = rpc(&mut operator, 2, "provider.consumeResetCredit", json!({"instanceId":"claudeAgent"})).await;
    let error = failure(&unconfirmed);
    assert_eq!((error["_tag"].as_str(), error["operation"].as_str(), error["detail"].as_str()), (Some("ProviderSetupError"), Some("consume-reset-credit"), Some("Claude could not confirm the reset. If you are still limited in a moment, try again.")));
    *outcome.lock().unwrap() = "reset".into();
    let reset = rpc(&mut operator, 3, "provider.consumeResetCredit", json!({"instanceId":"claudeAgent"})).await;
    assert_eq!(success(&reset)["outcome"], "reset");
    let claims: Vec<Value> = origin.requests().iter().filter(|request| request.method == "POST").map(|request| serde_json::from_str(&request.body).unwrap()).collect();
    assert_eq!(claims.len(), 2);
    assert_eq!((claims[0]["program"].as_str(), claims[0]["grant_id"].as_str()), (Some("cedar_ember"), Some("g_1")));
    assert_eq!(claims[0]["request_id"], claims[1]["request_id"], "an unconfirmed claim is retried as the same claim");
    // A source from `usageLimitSources` is not configured on this server.
    let source = rpc(&mut operator, 4, "provider.consumeResetCredit", json!({"sourceId":"proxy","accountId":"a","creditId":"c"})).await;
    assert_eq!(failure(&source)["_tag"], "UsageLimitSourceError");
    fixture.finish().await;
}

// --- usage summary --------------------------------------------------------------------------

fn rates_document() -> String {
    json!({
        "claude-sonnet-4-6":{"input_cost_per_token":3e-6,"output_cost_per_token":15e-6,"cache_read_input_token_cost":3e-7,"cache_creation_input_token_cost":3.75e-6},
        "gpt-5":{"input_cost_per_token":1.25e-6,"output_cost_per_token":10e-6,"cache_read_input_token_cost":1.25e-7}
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_usage_summary_prices_claude_and_codex_transcripts_with_rates_fetched_over_loopback() {
    let rates = Origin::start(|_| (200, rates_document())).await;
    // The server reads this once per call; every usage test shares the one loopback table.
    unsafe { std::env::set_var("T3CODE_USAGE_RATES_URL", rates.url()) };
    let now = Utc::now();
    let stamp = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let claude = [
        json!({"type":"assistant","timestamp":stamp(30),"requestId":"req_1","sessionId":"claude-session","message":{"id":"msg_1","model":"claude-sonnet-4-6","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":50,"output_tokens":20}}}),
        // One record per content block repeats the message's usage; it counts once.
        json!({"type":"assistant","timestamp":stamp(29),"requestId":"req_1","sessionId":"claude-session","message":{"id":"msg_1","model":"claude-sonnet-4-6","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":50,"output_tokens":20}}}),
        json!({"type":"assistant","timestamp":stamp(20),"sessionId":"claude-session","costUSD":0.25,"message":{"id":"msg_2","model":"made-up-model","usage":{"input_tokens":1000,"output_tokens":1000}}}),
    ];
    let codex = [
        json!({"type":"session_meta","timestamp":stamp(15),"payload":{"id":"codex-session"}}),
        json!({"type":"turn_context","timestamp":stamp(14),"payload":{"model":"gpt-5"}}),
        json!({"type":"event_msg","timestamp":stamp(13),"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50,"reasoning_output_tokens":30}}}}),
    ];
    let lines = |records: &[Value]| records.iter().map(|record| record.to_string() + "\n").collect::<String>();
    let options = Options::new("turn").file("claude-home/projects/-work/session.jsonl", lines(&claude)).file("codex-home/sessions/2026/10/09/rollout-1.jsonl", lines(&codex));
    let fixture = Fixture::start(options).await;
    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    let today = now.format("%Y-%m-%d").to_string();
    let window = json!({"sinceDay":today,"untilDay":today,"timeZone":"UTC"});
    let summary = rpc(&mut operator, 1, "server.getUsageSummary", window.clone()).await;
    let summary = success(&summary);
    assert_eq!((summary["contractVersion"].clone(), summary["timeZone"].as_str()), (json!(6), Some("UTC")));
    assert_eq!(summary["pricing"]["status"], "fresh");
    assert_eq!(summary["pricing"]["knownModels"], 2);
    let buckets = summary["buckets"].as_array().unwrap();
    let bucket = |provider: &str, model: &str| buckets.iter().find(|bucket| bucket["provider"] == provider && bucket["model"] == model).unwrap_or_else(|| panic!("no {provider}/{model} bucket in {buckets:?}"));
    let sonnet = bucket("claude", "claude-sonnet-4-6");
    assert_eq!(sonnet["totals"], json!({"uncachedInputTokens":10,"cachedInputTokens":100,"cacheCreationTokens":50,"outputTokens":20,"reasoningTokens":0}));
    assert_eq!((sonnet["records"].clone(), sonnet["costSource"].as_str()), (json!(1), Some("modelPriced")));
    let expected = 10.0 * 3e-6 + 100.0 * 3e-7 + 50.0 * 3.75e-6 + 20.0 * 15e-6;
    assert!((sonnet["costUsd"].as_f64().unwrap() - expected).abs() < 1e-12, "{sonnet}");
    // An unknown model keeps the cost the transcript reported.
    let unknown = bucket("claude", "made-up-model");
    assert_eq!((unknown["costUsd"].clone(), unknown["costSource"].as_str()), (json!(0.25), Some("providerReported")));
    let gpt = bucket("codex", "gpt-5");
    assert_eq!(gpt["totals"], json!({"uncachedInputTokens":600,"cachedInputTokens":400,"cacheCreationTokens":0,"outputTokens":50,"reasoningTokens":30}));
    let expected = 600.0 * 1.25e-6 + 400.0 * 1.25e-7 + 50.0 * 10e-6;
    assert!((gpt["costUsd"].as_f64().unwrap() - expected).abs() < 1e-12, "{gpt}");
    let sources = summary["sources"].as_array().unwrap();
    let claude_source = sources.iter().find(|source| source["fingerprint"]["provider"] == "claude").unwrap();
    assert_eq!((claude_source["status"].as_str(), claude_source["scannedFiles"].clone(), claude_source["distinctSessions"].clone()), (Some("ok"), json!(1), json!(1)));
    assert!(claude_source["fingerprint"]["resolvedHomePath"].as_str().unwrap().ends_with("claude-home/projects"));
    assert!(fixture.root.join("t3home/userdata/usage-model-rates.json").exists(), "the table is kept for offline use");
    // An explicit refresh within the floor reuses the table just fetched.
    let before = rates.requests().len();
    let pricing = rpc(&mut operator, 2, "server.refreshUsageRates", json!({})).await;
    assert_eq!(success(&pricing)["status"], "fresh");
    assert_eq!(rates.requests().len(), before);
    // A bad window is an invalid-window error, and the summary needs diagnostics:read.
    let invalid = rpc(&mut operator, 3, "server.getUsageSummary", json!({"sinceDay":"2026-10-09","untilDay":"2026-10-08","timeZone":"UTC"})).await;
    assert_eq!((failure(&invalid)["_tag"].as_str(), failure(&invalid)["reason"].as_str()), (Some("UsageReadError"), Some("invalidWindow")));
    let mut outsider = fixture.connect(&fixture.outsider.clone()).await;
    for (id, tag, payload) in [(4, "server.getUsageSummary", window.clone()), (5, "server.refreshUsageRates", json!({}))] {
        assert_eq!(failure(&rpc(&mut outsider, id, tag, payload).await)["_tag"], "EnvironmentAuthorizationError", "{tag}");
    }
    // A day that holds none of the history has no buckets.
    let empty = rpc(&mut operator, 6, "server.getUsageSummary", json!({"sinceDay":"2020-01-01","untilDay":"2020-01-02","timeZone":"America/Los_Angeles"})).await;
    assert!(success(&empty)["buckets"].as_array().unwrap().is_empty());
    fixture.finish().await;
}

// --- skills ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn skills_appear_in_the_snapshot_refresh_per_workspace_and_a_mention_dispatches_as_a_slash_command() {
    let skill = |description: &str| format!("---\ndescription: {description}\n---\nBody\n");
    let options = Options::new("turn")
        .file("claude-home/skills/review/SKILL.md", skill("Review code"))
        .file("work/.claude/skills/local/SKILL.md", skill("Project skill"))
        .file("other/.claude/skills/elsewhere/SKILL.md", skill("Another workspace"));
    let mut fixture = Fixture::start(options).await;
    let machine = fixture.snapshot();
    let names: Vec<(&str, &str)> = machine["skills"].as_array().unwrap().iter().map(|skill| (skill["name"].as_str().unwrap(), skill["scope"].as_str().unwrap())).collect();
    assert_eq!(names, [("local", "project"), ("review", "user")]);
    assert_eq!(fixture.snapshot()["skills"][1]["description"], "Review code");

    // A refresh for another workspace keeps that workspace's skills beside the machine's.
    let mut operator = fixture.connect(&fixture.operator.clone()).await;
    let other = fixture.root.join("other");
    let refreshed = rpc(&mut operator, 1, "server.refreshProviders", json!({"instanceId":"claudeAgent","cwd":other,"fresh":true})).await;
    let provider = success(&refreshed)["providers"].as_array().unwrap().iter().find(|provider| provider["instanceId"] == "claudeAgent").unwrap().clone();
    let scoped = provider["workspaceSnapshots"].as_array().unwrap().iter().find(|snapshot| snapshot["cwd"] == other.to_str().unwrap()).unwrap();
    let scoped_names: Vec<&str> = scoped["skills"].as_array().unwrap().iter().map(|skill| skill["name"].as_str().unwrap()).collect();
    assert_eq!(scoped_names, ["elsewhere", "review"]);

    // `$review` is the composer's spelling; Claude Code only expands a leading `/name` of the last block.
    fixture.turn(&mut operator, "please $review README.md", 1).await;
    fixture.turn(&mut operator, "price is $5 and $unknown", 2).await;
    let prompts: Vec<Value> = fixture.entries().into_iter().filter_map(|entry| entry.get("received").cloned()).filter(|message| message["type"] == "user").map(|message| message["message"]["content"].clone()).collect();
    assert_eq!(prompts[0], json!([{"type":"text","text":"please"},{"type":"text","text":"/review README.md"}]));
    assert_eq!(prompts[1], json!("price is $5 and $unknown"), "a mention of no skill stays prose");
    fixture.finish().await;
}

// --- importing Claude history ---------------------------------------------------------------

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

/// A transcript for `cwd` (`$ROOT` stands for the fixture's directory).
fn transcript(cwd: &str, session: &str, messages: &[(&str, &str)]) -> String {
    let mut lines = vec![json!({"type":"summary","cwd":cwd,"sessionId":session}).to_string()];
    for (index, (role, text)) in messages.iter().enumerate() {
        let stamp = (Utc::now() - chrono::Duration::minutes(10 - index as i64)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let message = if *role == "assistant" { json!({"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":text},{"type":"tool_use","id":"t","name":"Bash","input":{}}]}) } else { json!({"role":"user","content":text}) };
        lines.push(json!({"type":role,"cwd":cwd,"sessionId":session,"timestamp":stamp,"message":message}).to_string());
    }
    lines.join("\n") + "\n"
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_history_is_scanned_into_project_candidates_and_a_session_imports_as_a_resumable_thread() {
    let options = Options::new("turn")
        .file("proj/.git/config", "[remote \"origin\"]\n\turl = git@github.com:acme/widgets.git\n")
        .file("linked/.git", "gitdir: $ROOT/proj/.git/worktrees/linked\n")
        .file("claude-home/projects/-proj/main.jsonl", transcript("$ROOT/proj", SESSION, &[("user", "Add a widget"), ("assistant", "Done, I added it."), ("user", "Thanks")]))
        .file("claude-home/projects/-proj/not-a-uuid.jsonl", transcript("$ROOT/proj", "not-a-uuid", &[("user", "Unresumable")]))
        .file("claude-home/projects/-linked/linked.jsonl", transcript("$ROOT/linked", "22222222-2222-4333-8444-555555555555", &[("user", "In a worktree")]))
        .file("claude-home/projects/-gone/gone.jsonl", transcript("$ROOT/deleted-dir", "33333333-2222-4333-8444-555555555555", &[("user", "Directory removed")]))
        // The working directory is named only after more records than a scan reads from one transcript.
        .file("claude-home/projects/-late/late.jsonl", "{\"type\":\"noise\"}\n".repeat(1200) + &transcript("$ROOT/proj", "44444444-2222-4333-8444-555555555555", &[("user", "Too late")]))
        .file("claude-home/projects/-tmp/tmp.jsonl", transcript("/tmp", "55555555-2222-4333-8444-555555555555", &[("user", "In the temp directory")]));
    let mut fixture = Fixture::start(options).await;
    let project = fixture.root.join("proj");
    let mut operator = fixture.connect(&fixture.operator.clone()).await;

    // Candidates are the directories that still exist, are not worktrees or temp roots, and name
    // their cwd within the read bounds.
    let scanned = rpc(&mut operator, 1, "agentSessions.scan", json!({})).await;
    let candidates = success(&scanned)["candidates"].as_array().unwrap().clone();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    let candidate = &candidates[0];
    assert_eq!((candidate["path"].as_str(), candidate["title"].as_str(), candidate["threadCount"].clone(), candidate["alreadyImported"].clone()), (Some(project.to_str().unwrap()), Some("proj"), json!(2), json!(false)));
    assert_eq!(candidate["sources"], json!(["claudeAgent"]));
    assert_eq!(candidate["git"], json!({"remoteKey":"github.com/acme/widgets","repository":"acme/widgets"}));
    assert!(candidate["lastActiveAt"].is_string());

    // Importing needs a project at the candidate's path, and a client that scanned another path is refused.
    ProjectService::new(fixture.store.clone()).mutate(json!({"type":"project.create","commandId":"imported-project","projectId":"imported","title":"proj","workspaceRoot":project}), Utc::now()).unwrap();
    let scanned = rpc(&mut operator, 2, "agentSessions.scan", json!({})).await;
    assert_eq!((success(&scanned)["candidates"][0]["alreadyImported"].clone(), success(&scanned)["candidates"][0]["projectId"].as_str()), (json!(true), Some("imported")));
    let missing = rpc(&mut operator, 3, "agentSessions.import", json!({"projectId":"nope"})).await;
    assert_eq!(failure(&missing)["_tag"], "AgentSessionImportProjectNotFoundError");
    let changed = rpc(&mut operator, 4, "agentSessions.import", json!({"projectId":"imported","expectedWorkspaceRoot":fixture.root.join("elsewhere")})).await;
    assert_eq!(failure(&changed)["_tag"], "AgentSessionImportProjectChangedError");
    let mut reader = fixture.connect(&fixture.reader.clone()).await;
    assert!(success(&rpc(&mut reader, 5, "agentSessions.scan", json!({})).await)["candidates"].is_array(), "reading needs only orchestration:read");
    assert_eq!(failure(&rpc(&mut reader, 6, "agentSessions.import", json!({"projectId":"imported"})).await)["_tag"], "EnvironmentAuthorizationError");

    let imported = rpc(&mut operator, 7, "agentSessions.import", json!({"projectId":"imported","expectedWorkspaceRoot":project})).await;
    // The session with a valid id imports; the unresumable one and the one whose cwd came too late do not.
    assert_eq!((success(&imported)["importedCount"].clone(), success(&imported)["skippedCount"].clone()), (json!(1), json!(1)));
    let thread_id = format!("import:claudeAgent:{SESSION}");
    let thread = fixture.store.projection("thread", &thread_id).unwrap().expect("the session became a thread");
    assert_eq!((thread["thread"]["title"].as_str(), thread["thread"]["projectId"].as_str(), thread["thread"]["modelSelection"]["model"].as_str()), (Some("Add a widget"), Some("imported"), Some("claude-sonnet-4-6")));
    let texts: Vec<(&str, &str)> = thread["turnItems"].as_array().unwrap().iter().map(|item| (item["type"].as_str().unwrap(), item["text"].as_str().unwrap())).collect();
    assert_eq!(texts, [("user_message", "Add a widget"), ("assistant_message", "Done, I added it."), ("user_message", "Thanks")]);
    assert_eq!(thread["providerThreads"][0]["nativeThreadRef"], json!({"driver":"claudeAgent","nativeId":SESSION,"strength":"strong"}));
    // Importing again finds the thread and does not duplicate it.
    let again = rpc(&mut operator, 8, "agentSessions.import", json!({"projectId":"imported"})).await;
    assert_eq!(success(&again)["importedCount"], 1);
    assert_eq!(fixture.store.projection("thread", &thread_id).unwrap().unwrap()["turnItems"].as_array().unwrap().len(), 3);

    // The first message on the imported thread resumes the native session instead of creating it.
    fixture.thread = thread_id.clone();
    fixture.turn_without_checkpoint(&mut operator, "continue", 1).await;
    let launches = fixture.launches();
    assert!(launches.last().unwrap().contains(&format!("--resume={SESSION}")), "{launches:?}");
    assert!(arg_value(launches.last().unwrap(), "--session-id=").is_none());
    assert_eq!(flag(launches.last().unwrap(), "--model").as_deref(), Some("claude-sonnet-4-6"));
    fixture.finish().await;
}

// --- subagents and background work ------------------------------------------------------------
// Each scenario is a script of the fake `claude` (`claude-provider.py`): a subagent that asks for
// approval, a subagent still running when the parent is stopped, a background command that outlives
// its turn, and one whose end makes Claude wake itself.
impl Fixture {
    async fn command(&mut self, socket: &mut Socket, command: Value) {
        self.next += 1;
        let exit = rpc(socket, 2000 + self.next as u64, "orchestration.dispatchCommand", command).await;
        assert!(success(&exit)["sequence"].is_number(), "{exit}");
    }
    async fn send(&mut self, socket: &mut Socket, text: &str) {
        let id = format!("message-{}", self.next + 1);
        let command = json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}});
        self.command(socket, command).await;
    }
    fn child_threads(&self) -> Vec<Value> {
        self.view()["subagents"].as_array().unwrap().iter().map(|task| self.store.projection("thread", task["childThreadId"].as_str().unwrap()).unwrap().unwrap()).collect()
    }
}
fn settled(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))
}

/// A subagent's approval is raised on the parent thread, where the user sees and answers it; the
/// child thread holds the work but never a request.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_approval_is_asked_on_the_parent_and_answered_there() {
    let mut fixture = Fixture::start(Options::new("subagent_approval")).await;
    let mut socket = fixture.connect(&fixture.operator.clone()).await;
    fixture.send(&mut socket, "Inspect the directory").await;
    let request = fixture.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await;
    assert_eq!(request["kind"], "command");
    let view = fixture.view();
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
    assert_eq!(item["prompt"], "Claude wants to run ls");
    let children = fixture.child_threads();
    assert_eq!(children.len(), 1);
    assert!(children[0]["runtimeRequests"].as_array().unwrap().is_empty(), "the child thread holds no request");
    assert_eq!(children[0]["thread"]["lineage"]["relationshipToParent"], "subagent");
    assert!(children[0]["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "command_execution" && item["status"] == "running"));

    let thread = fixture.thread.clone();
    fixture.command(&mut socket, json!({"type":"runtime-request.respond","commandId":"approve-child","threadId":thread,"requestId":request["id"],"decision":"accept"})).await;
    let run = fixture.until(|view| view["runs"].get(0).filter(|run| settled(run)).cloned()).await;
    assert_eq!(run["status"], "completed");
    let task = fixture.view()["subagents"][0].clone();
    assert_eq!((task["status"].as_str(), task["result"].as_str()), (Some("completed"), Some("listed")));
    let child = &fixture.child_threads()[0];
    let tool = child["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!((tool["status"].as_str(), tool["output"].as_str()), (Some("completed"), Some("a.txt\n")));
    // The CLI was told what the user decided.
    let answered: Vec<Value> = fixture.entries().iter().filter_map(|entry| entry["received"]["response"]["response"].as_object().cloned()).map(Value::Object).collect();
    assert!(answered.iter().any(|response| response["behavior"] == "allow"), "{answered:?}");
    fixture.finish().await;
}

/// Stop on a settled run closes the CLI that owns a background command, which is what ends it; the
/// roster then clears and the work the thread listed is settled.
#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_a_live_background_task_by_closing_the_process_that_runs_it() {
    let mut fixture = Fixture::start(Options::new("background_task")).await;
    let mut socket = fixture.connect(&fixture.operator.clone()).await;
    fixture.send(&mut socket, "Start a background command").await;
    let roster = fixture.until(|view| view["runs"].get(0).filter(|run| run["status"] == "completed")?.get("id").and(view["providerThreads"][0]["pendingBackgroundTasks"].as_array().filter(|tasks| !tasks.is_empty()).cloned())).await;
    assert_eq!((roster[0]["taskId"].as_str(), roster[0]["kind"].as_str()), (Some("bg_1"), Some("command")));
    assert!(t3_server::thread::shell(&fixture.view())["pendingBackgroundTasks"].as_array().unwrap().iter().any(|task| task["taskId"] == "bg_1"));
    assert!(!fixture.entries().iter().any(|entry| entry["stdin"] == "closed"), "the process runs until Stop");

    let (thread, run) = (fixture.thread.clone(), fixture.view()["runs"][0]["id"].clone());
    fixture.command(&mut socket, json!({"type":"run.interrupt","commandId":"stop-background","threadId":thread,"runId":run})).await;
    fixture.until(|view| view["providerThreads"][0]["pendingBackgroundTasks"].as_array().unwrap().is_empty().then_some(())).await;
    // The roster clears only after the process is closed, so its stdin was closed by then.
    assert!(fixture.entries().iter().any(|entry| entry["stdin"] == "closed"), "Stop closed the process that ran the task");
    assert!(t3_server::thread::shell(&fixture.view())["pendingBackgroundTasks"].as_array().unwrap().is_empty());
    assert_eq!(fixture.view()["runs"][0]["status"], "completed");
    fixture.finish().await;
}

/// A background command that ends after the turn did wakes Claude; the wake runs as a run of its
/// own, shown as the report of the work and carrying what Claude said about it.
#[tokio::test(flavor = "multi_thread")]
async fn a_wake_turn_starts_after_a_background_task_finishes() {
    let mut fixture = Fixture::start(Options::new("background_wake")).await;
    let mut socket = fixture.connect(&fixture.operator.clone()).await;
    fixture.send(&mut socket, "Start a background command").await;
    fixture.until(|view| (view["runs"].as_array().unwrap().len() == 2 && view["runs"].as_array().unwrap().iter().all(settled)).then_some(())).await;
    let view = fixture.view();
    assert_eq!(view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(), vec![json!("completed"), json!("completed")]);
    let wake_run = view["runs"][1]["id"].clone();
    let items: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["runId"] == wake_run && item["type"] != "checkpoint").collect();
    let notification = items.iter().find(|item| item["type"] == "notification").expect("the wake is shown as a report");
    assert_eq!((notification["source"]["kind"].as_str(), notification["outcome"].as_str()), (Some("background_command"), Some("completed")));
    assert_eq!(notification["summary"], "Command \"sleep 600\" finished");
    assert!(items.iter().any(|item| item["type"] == "assistant_message" && item["text"] == "the command finished"));
    // The wake is the CLI's own turn: no second prompt reached it.
    let prompts = fixture.entries().iter().filter(|entry| entry["received"]["type"] == "user").count();
    assert_eq!(prompts, 1);
    assert!(view["providerThreads"][0]["pendingBackgroundTasks"].as_array().unwrap().is_empty());
    fixture.finish().await;
}

/// Stopping a parent whose subagent still has a command open ends the subagent, its nodes and the
/// command, in the child thread as well as the parent's.
#[tokio::test(flavor = "multi_thread")]
async fn a_parent_interrupted_with_children_running_ends_them_too() {
    let mut fixture = Fixture::start(Options::new("children_running")).await;
    let mut socket = fixture.connect(&fixture.operator.clone()).await;
    fixture.send(&mut socket, "Run a long command in a subagent").await;
    fixture.until(|view| view["subagents"].as_array().unwrap().first().and_then(|task| fixture_child_has_running_tool(&fixture, task)).map(|_| ())).await;
    let (thread, run) = (fixture.thread.clone(), fixture.view()["runs"][0]["id"].clone());
    fixture.command(&mut socket, json!({"type":"run.interrupt","commandId":"stop-parent","threadId":thread,"runId":run})).await;
    fixture.until(|view| view["runs"].get(0).filter(|run| settled(run)).map(|_| ())).await;
    // Everything the run started settles with it, in each thread: the child's own commit follows the parent's.
    fixture.until(|view| (view["subagents"][0]["status"] == "interrupted").then_some(())).await;
    fixture.until(|view| view["subagents"].as_array().unwrap().first().and_then(|task| fixture.store.projection("thread", task["childThreadId"].as_str()?).ok()??["nodes"].as_array().map(|nodes| nodes.iter().all(|node| node["status"] == "interrupted"))).filter(|settled| *settled).map(|_| ())).await;
    let view = fixture.view();
    assert_eq!(view["runs"][0]["status"], "interrupted");
    assert!(view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "subagent").all(|item| item["status"] == "interrupted"));
    let child = &fixture.child_threads()[0];
    assert!(child["nodes"].as_array().unwrap().iter().all(|node| node["status"] == "interrupted"), "{}", child["nodes"]);
    let tool = child["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").unwrap();
    assert_eq!(tool["status"], "interrupted");
    fixture.finish().await;
}
fn fixture_child_has_running_tool(fixture: &Fixture, task: &Value) -> Option<()> {
    let child = fixture.store.projection("thread", task["childThreadId"].as_str()?).ok()??;
    child["turnItems"].as_array()?.iter().any(|item| item["type"] == "command_execution" && item["status"] == "running").then_some(())
}
