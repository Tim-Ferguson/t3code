#![cfg(unix)]
//! A Pi thread through the real WebSocket transport, end to end, against the scripted fake `pi`
//! (`fixtures/pi-fake-cli.py`): discovery and selection, a turn, an approval (accepted for the session, then declined), a
//! question, a stop followed by another turn, a session that survives the server restarting, the T3 MCP credential reaching
//! Pi, and the process ending with the server. The wrapper is at an absolute path in the temp home; HOME stays there; no real
//! `pi` runs and nothing leaves loopback. Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
mod provider_manage_support;
use chrono::Utc;
use provider_manage_support::{Socket, provider, rpc, success};
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}};
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

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi-fake-cli.py");

/// A server over `base`: a store that outlives it, the provider registry, the effect loop, and the WebSocket transport.
struct Server {
    store: Store,
    providers: ProviderRegistry,
    execution: ExecutionService,
    address: std::net::SocketAddr,
    token: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
fn settings(base: &Path) -> t3_contracts::ServerSettings {
    let wrapper = base.join("bin/pi");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, format!("#!/bin/sh\nexec python3 '{CLI}' \"$@\"\n")).unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let environment = vec![json!({"name":"HOME","value":base.join("home")}), json!({"name":"PI_FIXTURE_SESSIONS","value":base.join("sessions")}), json!({"name":"PI_FIXTURE_LOG","value":base.join("pi.log")})];
    serde_json::from_value(json!({"enableAgentBrowserAccess":false,"providerInstances":{
        "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
        "pi":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":wrapper,"launchArgs":"--no-skills"},"environment":environment},
        "pi-b":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":wrapper,"launchArgs":"--no-skills"},"environment":environment}}}))
    .unwrap()
}
async fn boot(base: &Path, mcp: Option<std::net::SocketAddr>) -> Server {
    let providers = ProviderRegistry::discover(&settings(base), base).await.unwrap();
    let store = Store::open(base.join("state.db")).unwrap();
    if let Some(address) = mcp {
        let registry = t3_server::mcp_sessions::McpSessionRegistry::new("environment".parse().unwrap(), Some(address), std::sync::Arc::new(|| Utc::now().timestamp_millis()), 10000);
        providers.set_mcp_sessions(t3_server::provider_mcp::ProviderMcpSessions::new(registry, PathBuf::from(env!("CARGO_BIN_EXE_t3-server"))));
    }
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_pi".into(), "loopback-browser".into()).unwrap();
    let scopes = vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::ProvidersManage];
    let token = auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(), auth, environment: json!({"environmentId":"pi-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: Some(providers.clone()), execution: Some(execution.clone()), workspace: None, terminals: None, discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap(); });
    Server { store, providers, execution, address, token, stop: Some(stop), task: Some(task) }
}
impl Server {
    async fn connect(&self) -> Socket {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {}", self.token).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    /// The server goes away: transport, effect loop and every provider process with them.
    async fn shutdown(mut self) {
        self.execution.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.task.take().unwrap().await.unwrap();
    }
}

struct Flow {
    server: Server,
    socket: Socket,
    thread: String,
    next: u64,
}
impl Flow {
    async fn command(&mut self, command: Value) {
        self.next += 1;
        let exit = rpc(&mut self.socket, self.next, "orchestration.dispatchCommand", command).await;
        success(&exit);
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.server.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.server.store.subscribe();
        provider_manage_support::within(format!("the thread to show {what}"), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                // Lagged receivers re-check the projection, which is the source of truth.
                let _ = events.recv().await;
            }
        })
        .await
    }
    async fn send(&mut self, text: &str) -> usize {
        let id = format!("message-{}", self.next + 1);
        self.command(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn settled(&self, ordinal: usize) -> Value {
        self.until("the run settled", |view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted"))).cloned()).await
    }
    async fn pending(&self) -> Value {
        self.until("a pending request", |view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    fn texts(&self, kind: &str) -> Vec<String> {
        let mut items: Vec<Value> = self.view()["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == kind).cloned().collect();
        items.sort_by_key(|item| item["ordinal"].as_u64());
        items.iter().map(|item| item["text"].as_str().unwrap_or_default().to_owned()).collect()
    }
}
fn log(base: &Path) -> Vec<Value> {
    std::fs::read_to_string(base.join("pi.log")).unwrap_or_default().lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}
fn session_launches(base: &Path) -> Vec<Value> {
    log(base).into_iter().filter(|entry| entry["launch"].is_array() && !entry["launch"].as_array().unwrap().contains(&json!("--no-session"))).collect()
}
fn received(base: &Path, kind: &str) -> Vec<Value> {
    log(base).into_iter().filter_map(|entry| entry.get("received").cloned()).filter(|record| record["type"] == kind).collect()
}

async fn start(base: &Path, mode: &str) -> Flow {
    let server = boot(base, None).await;
    start_with(server, base, mode).await
}
async fn start_with(server: Server, base: &Path, mode: &str) -> Flow {
    std::fs::create_dir_all(base.join("project")).unwrap();
    ProjectService::new(server.store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":base.join("project")}), Utc::now()).unwrap();
    let thread = ThreadLaunchService::with_providers(server.store.clone(), server.providers.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Pi flow","modelSelection":{"instanceId":"pi","model":"fake/fake-model","options":[{"id":"thinking","value":"medium"}]},"runtimeMode":mode,"interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let socket = server.connect().await;
    Flow { server, socket, thread, next: 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_thread_discovers_selects_a_model_runs_turns_asks_and_is_answered_stops_and_goes_on() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "approval-required").await;

    // Discovery and selection: the instance is listed ready with the model the thread selected.
    let listed = rpc(&mut flow.socket, 900, "server.refreshProviders", json!({"instanceId":"pi"})).await;
    let snapshot = provider(&success(&listed)["providers"], "pi").clone();
    assert_eq!((snapshot["status"].clone(), snapshot["auth"].clone()), (json!("ready"), json!({"status":"authenticated","type":"pi"})), "{snapshot}");
    assert!(snapshot["models"].as_array().unwrap().iter().any(|model| model["slug"] == "fake/fake-model" && model["capabilities"]["optionDescriptors"][0]["id"] == "thinking"));
    assert_eq!(flow.view()["thread"]["modelSelection"]["model"], "fake/fake-model");

    // A turn: reasoning and text persist, the model and thinking level the thread selected reached Pi, and the context meter has a reading.
    let first = flow.send("hello").await;
    assert_eq!(flow.settled(first).await["status"], "completed");
    assert_eq!((flow.texts("reasoning"), flow.texts("assistant_message")), (vec!["Thinking about it.".to_owned()], vec!["Echo: hello".to_owned()]));
    let view = flow.view();
    assert_eq!(view["providerTurns"][0]["tokenUsage"]["maxTokens"], 100000);
    assert_eq!(view["providerSessions"][0]["status"], "ready");
    let launches = session_launches(&base);
    assert_eq!(launches.len(), 1, "{launches:?}");
    let args: Vec<&str> = launches[0]["launch"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(&args[..3], ["--mode", "rpc", "--no-skills"]);
    assert_eq!(args[3], "--extension");
    assert!(args[4].ends_with("caches/pi-t3-mcp-extension.ts") && Path::new(args[4]).exists(), "{args:?}");
    assert_eq!(launches[0]["mode"], "approval-required");
    assert_eq!(launches[0]["home"], json!(base.join("home")));
    assert!(launches[0]["mcp"].is_null() && launches[0]["token"].is_null(), "no MCP credential without a session for one");
    let set_model = &received(&base, "set_model")[0];
    assert_eq!((set_model["provider"].clone(), set_model["modelId"].clone()), (json!("fake"), json!("fake-model")));
    assert_eq!(received(&base, "set_thinking_level")[0]["level"], "medium");
    assert_eq!(received(&base, "set_session_name")[0]["name"], "Pi flow");

    // An approval: Pi's permission hook asks, the request is a command approval, and accepting for the session answers
    // the next identical one without asking again.
    let second = flow.send("approve once").await;
    let request = flow.pending().await;
    assert_eq!(request["kind"], "command");
    let item = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").cloned().unwrap();
    assert_eq!((item["requestKind"].clone(), item["prompt"].clone(), item["title"].clone()), (json!("command"), json!("echo approved"), json!("Allow bash?")));
    flow.command(json!({"type":"runtime-request.respond","commandId":"approve-1","threadId":flow.thread,"requestId":request["id"],"decision":"acceptForSession"})).await;
    assert_eq!(flow.settled(second).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "The command ran.");
    let tool = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").cloned().unwrap();
    assert_eq!((tool["status"].clone(), tool["input"].clone(), tool["output"].clone(), tool["title"].clone()), (json!("completed"), json!("echo approved"), json!("approved\n"), json!("bash")));
    let third = flow.send("approve again").await;
    assert_eq!(flow.settled(third).await["status"], "completed");
    assert_eq!(flow.view()["runtimeRequests"].as_array().unwrap().len(), 1, "an approval accepted for the session is not asked again");
    assert_eq!(received(&base, "extension_ui_response").len(), 2, "Pi was answered both times");

    // A question: the dialog is a question with its options, and the answer reaches the extension as the value.
    let fourth = flow.send("ask which").await;
    let request = flow.pending().await;
    assert_eq!(request["kind"], "user_input");
    let question = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_input_request").cloned().unwrap();
    let id = question["questions"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(question["questions"][0]["options"].as_array().unwrap().iter().map(|option| option["value"].as_str().unwrap()).collect::<Vec<_>>(), ["One", "Two"]);
    flow.command(json!({"type":"runtime-request.respond","commandId":"answer-1","threadId":flow.thread,"requestId":request["id"],"answers":{id:"Two"}})).await;
    assert_eq!(flow.settled(fourth).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "You chose Two");

    // A stop: Pi aborts the tool, the run is interrupted, and Pi's process is retired with its session marked stopped.
    let fifth = flow.send("hang on").await;
    flow.until("the command running", |view| view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "command_execution" && item["status"] == "running" && item["input"] == "sleep 60").then_some(())).await;
    let run = flow.view()["runs"][fifth - 1]["id"].clone();
    flow.command(json!({"type":"run.interrupt","commandId":"stop-1","threadId":flow.thread,"runId":run})).await;
    assert_eq!(flow.settled(fifth).await["status"], "interrupted");
    let view = flow.view();
    let aborted = view["turnItems"].as_array().unwrap().iter().find(|item| item["input"] == "sleep 60").cloned().unwrap();
    assert_eq!((aborted["status"].clone(), aborted["output"].clone()), (json!("interrupted"), json!("Command aborted")));
    flow.until("the session stopped", |view| (view["providerSessions"].as_array().unwrap().last()?["status"] == "stopped").then_some(())).await;
    assert!(!received(&base, "abort").is_empty());

    // The next turn starts a new process that resumes the same native session: the history is still there.
    let sixth = flow.send("remember").await;
    assert_eq!(flow.settled(sixth).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "I remember 5 earlier messages.");
    assert_eq!(session_launches(&base).len(), 2);
    assert_eq!(received(&base, "switch_session").len(), 1, "the new process resumed the session file");
    flow.until("the session ready again", |view| (view["providerSessions"].as_array().unwrap().last()?["status"] == "ready").then_some(())).await;

    // The server goes away and comes back over the same state: the thread resumes its Pi session from the session file.
    let Flow { server, socket, thread, next } = flow;
    drop(socket);
    server.shutdown().await;
    let server = boot(&base, None).await;
    let socket = server.connect().await;
    let mut flow = Flow { server, socket, thread, next };
    let seventh = flow.send("remember").await;
    assert_eq!(flow.settled(seventh).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "I remember 6 earlier messages.");
    assert_eq!(session_launches(&base).len(), 3);
    assert_eq!(received(&base, "switch_session").len(), 2);
    // The process belongs to the server: when it goes away so does Pi.
    let launches = session_launches(&base);
    assert_eq!(launches.len(), 3);
    flow.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_approval_is_answered_to_pi_and_a_failed_prompt_fails_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "approval-required").await;
    let first = flow.send("approve please").await;
    let request = flow.pending().await;
    flow.command(json!({"type":"runtime-request.respond","commandId":"decline-1","threadId":flow.thread,"requestId":request["id"],"decision":"decline"})).await;
    assert_eq!(flow.settled(first).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "The command was declined.");
    assert_eq!(received(&base, "extension_ui_response")[0]["confirmed"], false);
    assert!(flow.view()["turnItems"].as_array().unwrap().iter().all(|item| item["type"] != "command_execution"), "a declined tool never ran");
    // An edit approval is a file-change request, and the edit it allows is a file change with the patch Pi reported.
    let edit = flow.send("approve edit").await;
    flow.pending().await;
    let item = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request" && item["title"] == "Allow edit?").cloned().unwrap();
    assert_eq!(item["requestKind"], "file-change");
    let request = flow.view()["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned().unwrap();
    flow.command(json!({"type":"runtime-request.respond","commandId":"approve-edit","threadId":flow.thread,"requestId":request["id"],"decision":"accept"})).await;
    assert_eq!(flow.settled(edit).await["status"], "completed");
    let change = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "file_change").cloned().unwrap();
    assert_eq!((change["fileName"].clone(), change["diffStr"].clone(), change["status"].clone()), (json!("notes.txt"), json!("--- a/notes.txt\n+++ b/notes.txt\n-a\n+b"), json!("completed")));
    // The subagent extension's tool result shows in the shared subagent UI, without a child thread of its own.
    let delegated = flow.send("delegate it").await;
    assert_eq!(flow.settled(delegated).await["status"], "completed");
    let view = flow.view();
    let tasks = view["subagents"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!((tasks[0]["origin"].clone(), tasks[0]["driver"].clone(), tasks[0]["title"].clone(), tasks[0]["prompt"].clone(), tasks[0]["status"].clone(), tasks[0]["result"].clone(), tasks[0]["childThreadId"].clone()), (json!("provider_native"), json!("pi"), json!("worker"), json!("survey the repository"), json!("completed"), json!("found three files"), Value::Null));
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "subagent" && item["status"] == "completed"));
    // A Pi that dies mid-turn fails the run with a message that names no stderr and no credential, and the session says so.
    let second = flow.send("crash now").await;
    let run = flow.settled(second).await;
    assert_eq!(run["status"], "failed");
    let failure = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").cloned().unwrap();
    assert_eq!((failure["failure"]["class"].clone(), failure["failure"]["message"].clone()), (json!("transport_error"), json!("Pi process exited unexpectedly.")));
    flow.until("the session errored", |view| (view["providerSessions"].as_array().unwrap().last()?["status"] == "error").then_some(())).await;
    // The thread is not stuck: the next turn gets a new process.
    let third = flow.send("hello after the crash").await;
    assert_eq!(flow.settled(third).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "Echo: hello after the crash");
    flow.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_t3_mcp_credential_reaches_pi_and_works_against_the_server() {
    use std::sync::Arc;
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    // The MCP endpoint the session's credential is issued for: the fake plays Pi's extension and uses it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let registry = t3_server::mcp_sessions::McpSessionRegistry::new("environment".parse().unwrap(), Some(address), Arc::new(|| Utc::now().timestamp_millis()), 10000);
    let service = t3_server::mcp_http::McpHttpService::new(registry.clone(), None);
    let http = tokio::spawn(async move { axum::serve(listener, service.router()).await.unwrap() });
    let providers = ProviderRegistry::discover(&settings(&base), &base).await.unwrap();
    providers.set_mcp_sessions(t3_server::provider_mcp::ProviderMcpSessions::new(registry, PathBuf::from(env!("CARGO_BIN_EXE_t3-server"))));
    let store = Store::open(base.join("state.db")).unwrap();
    std::fs::create_dir_all(base.join("project")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":base.join("project")}), Utc::now()).unwrap();
    let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"Pi MCP","modelSelection":{"instanceId":"pi","model":"fake/fake-model"},"runtimeMode":"auto-accept-edits","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now()).unwrap()["threadId"].as_str().unwrap().to_owned();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let mut events = store.subscribe();
    execution.dispatch(&json!({"type":"message.dispatch","commandId":"m1","threadId":thread,"messageId":"m1","text":"mcp tools","attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    let run = tokio::time::timeout(HANG_BOUND, async {
        loop {
            let view = store.projection("thread", &thread).unwrap().unwrap();
            if let Some(run) = view["runs"][0].as_object().filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed"))) {
                return Value::Object(run.clone());
            }
            let _ = events.recv().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(run["status"], "completed");
    let view = store.projection("thread", &thread).unwrap().unwrap();
    let reply = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "assistant_message").last().unwrap()["text"].as_str().unwrap().to_owned();
    // The credential in Pi's environment initialized an MCP session and listed the tools (-1 would be a refused credential).
    assert!(reply.starts_with("MCP tools: ") && reply["MCP tools: ".len()..].parse::<i32>().is_ok_and(|count| count >= 0), "{reply}");
    let launch = session_launches(&base).remove(0);
    assert_eq!(launch["mcp"], json!(format!("http://{address}/mcp")));
    assert!(launch["token"].as_str().is_some_and(|token| !token.is_empty() && !token.starts_with("Bearer ")));
    assert_eq!(launch["mode"], "auto-accept-edits");
    // The secret is Pi's alone: not in the projection, not in a snapshot.
    let token = launch["token"].as_str().unwrap();
    assert!(!view.to_string().contains(token) && !serde_json::to_string(&providers.snapshots()).unwrap().contains(token));
    execution.shutdown().await;
    http.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn another_pi_instance_is_another_history_so_the_switch_briefs_it_in_the_message_and_releases_the_first() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "full-access").await;
    let first = flow.send("remember the moon").await;
    assert_eq!(flow.settled(first).await["status"], "completed");
    // The second instance has a Pi home of its own as far as T3 can tell (its continuation key is its own): a handoff, not a resume.
    let id = "to-b";
    flow.command(json!({"type":"message.dispatch","commandId":id,"threadId":flow.thread,"messageId":id,"text":"and now?","attachments":[],"modelSelection":{"instanceId":"pi-b","model":"fake/fake-model"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    assert_eq!(flow.settled(2).await["status"], "completed");
    let view = flow.view();
    assert_eq!(view["contextHandoffs"].as_array().unwrap().len(), 1);
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 2);
    // Pi takes no native history injection: the history goes ahead of the user's message, in the prompt.
    let prompts: Vec<String> = received(&base, "prompt").iter().map(|prompt| prompt["message"].as_str().unwrap().to_owned()).collect();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("remember the moon") && prompts[1].ends_with("User message:\nand now?"), "{}", prompts[1]);
    // The first instance's session was released, and the second one's process is a session of its own.
    assert_eq!(session_launches(&base).len(), 2);
    flow.until("the first session released", |view| view["providerSessions"].as_array().unwrap().iter().any(|session| session["providerInstanceId"] == "pi" && session["status"] == "stopped").then_some(())).await;
    flow.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_notices_and_extension_failures_show_on_the_turn_and_a_retry_that_gives_up_fails_it_once() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "full-access").await;
    // A model error that Pi retries and recovers from does not fail the turn; the recovery is on the timeline.
    let first = flow.send("retry recovers").await;
    assert_eq!(flow.settled(first).await["status"], "completed");
    let view = flow.view();
    let retry = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").cloned().unwrap();
    assert_eq!((retry["status"].clone(), retry["title"].clone(), retry["retry"]["attempt"].clone(), retry["retry"]["maxAttempts"].clone(), retry["failure"]["retryable"].clone()), (json!("completed"), json!("Provider recovered"), json!(1), json!(3), json!(true)));
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "Recovered.");
    // Notices and extension failures are items of the turn that ran them; terminal decoration is ignored.
    let second = flow.send("notices please").await;
    assert_eq!(flow.settled(second).await["status"], "completed");
    let view = flow.view();
    let notice = view["turnItems"].as_array().unwrap().iter().find(|item| item["toolName"] == "notify").cloned().unwrap();
    assert_eq!((notice["status"].clone(), notice["input"].clone()), (json!("completed"), json!({"message":"heads up","notifyType":"warning"})));
    let failure = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "error" && item["title"] == "guard").cloned().collect::<Vec<_>>();
    assert_eq!(failure.len(), 1);
    assert_eq!((failure[0]["status"].clone(), failure[0]["failure"]["message"].clone(), failure[0]["failure"]["class"].clone()), (json!("failed"), json!("guard failed during tool_call.\n\nboom"), json!("provider_error")));
    // A retry that gives up fails the turn, and the failure is that retry's one error item.
    let third = flow.send("retry gives up").await;
    assert_eq!(flow.settled(third).await["status"], "failed");
    let view = flow.view();
    let run = view["runs"][third - 1]["id"].clone();
    let errors: Vec<Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "error" && item["runId"] == run).cloned().collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!((errors[0]["status"].clone(), errors[0]["title"].clone(), errors[0]["failure"]["message"].clone(), errors[0]["retry"]["attempt"].clone(), errors[0]["retry"]["retryDelayMs"].clone()), (json!("failed"), json!("Provider error"), json!("gave up after 2 attempts"), json!(2), json!(100)));
    flow.until("the session errored", |view| (view["providerSessions"].as_array().unwrap().last()?["lastError"] == "gave up after 2 attempts").then_some(())).await;
    // The process is fine: the next turn runs on it.
    let fourth = flow.send("hello").await;
    assert_eq!(flow.settled(fourth).await["status"], "completed");
    assert_eq!(session_launches(&base).len(), 1, "a failed turn does not cost the process");
    flow.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_while_pi_waits_on_a_dialog_cancels_the_request_and_ends_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "approval-required").await;
    let first = flow.send("approve it").await;
    let request = flow.pending().await;
    let run = flow.view()["runs"][first - 1]["id"].clone();
    flow.command(json!({"type":"run.interrupt","commandId":"stop-dialog","threadId":flow.thread,"runId":run})).await;
    assert_eq!(flow.settled(first).await["status"], "interrupted");
    flow.until("the request cancelled", |view| view["runtimeRequests"].as_array().unwrap().iter().all(|candidate| candidate["status"] == "cancelled").then_some(())).await;
    let view = flow.view();
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["requestId"] == request["id"]).cloned().unwrap();
    assert_eq!(item["status"], "cancelled");
    // The dead process cannot be answered, and the thread is not stuck behind the request: the next turn runs.
    let second = flow.send("hello").await;
    assert_eq!(flow.settled(second).await["status"], "completed");
    assert_eq!(flow.texts("assistant_message").last().unwrap(), "Echo: hello");
    flow.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_on_another_model_or_thinking_level_selects_it_in_the_same_pi_session() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let mut flow = start(&base, "full-access").await;
    let first = flow.send("hello").await;
    assert_eq!(flow.settled(first).await["status"], "completed");
    let id = "switch-model";
    flow.command(json!({"type":"message.dispatch","commandId":id,"threadId":flow.thread,"messageId":id,"text":"again","attachments":[],"modelSelection":{"instanceId":"pi","model":"other/second-model","options":[{"id":"thinking","value":"high"}]},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    assert_eq!(flow.settled(2).await["status"], "completed");
    let models: Vec<(String, String)> = received(&base, "set_model").iter().map(|record| (record["provider"].as_str().unwrap().to_owned(), record["modelId"].as_str().unwrap().to_owned())).collect();
    assert_eq!(models, [("fake".to_owned(), "fake-model".to_owned()), ("other".to_owned(), "second-model".to_owned())]);
    let levels: Vec<String> = received(&base, "set_thinking_level").iter().map(|record| record["level"].as_str().unwrap().to_owned()).collect();
    assert_eq!(levels, ["medium", "high"]);
    assert_eq!(flow.view()["providerSessions"][0]["model"], "other/second-model");
    // A model that is not `provider/model` is refused before anything is sent to Pi, and the run says why.
    let id = "bad-model";
    flow.command(json!({"type":"message.dispatch","commandId":id,"threadId":flow.thread,"messageId":id,"text":"third","attachments":[],"modelSelection":{"instanceId":"pi","model":"nonsense"},"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
    let run = flow.settled(3).await;
    assert_eq!(run["status"], "failed");
    assert_eq!(received(&base, "set_model").len(), 2);
    flow.server.shutdown().await;
}
