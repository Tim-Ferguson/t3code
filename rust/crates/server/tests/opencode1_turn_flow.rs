#![cfg(unix)]
//! OpenCode 1.x turns through the real WebSocket transport, against the stand-in server a spawned `opencode serve`
//! would be (`fixtures/opencode-fake-cli.py` in its 1.x mode, started by the driver itself from a wrapper script at an
//! absolute path in the temp directory): a turn, an approval, a question, a Stop followed by another turn, a subagent's
//! approval answered while the parent runs, and a thread continued after the server restarted. The recordings are the
//! source's (`opencode-replay/*.json`, recorded at the SDK client's boundary); the sessions this needs beyond them
//! are cut from them. Commands go over one socket and each stream has a socket of its own, whose chunks are
//! acknowledged. The server comes up through `grok_support::serve`, which attaches what `main.rs` attaches. HOME and
//! the data home stay in the temp directory, no real `opencode` runs, and the server a test started is ended on every
//! exit path.
mod grok_support;
mod opencode_support;
mod provider_manage_support;
use chrono::Utc;
use grok_support::{Served, Socket, serve};
use provider_manage_support::{rpc, success, within};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

fn row_of(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/opencode-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
}

/// Ends the server a test started if the test did not: only by the pid the server recorded for itself.
struct PidGuard(PathBuf);
impl PidGuard {
    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.0).ok()?.trim().parse().ok().filter(|pid| *pid > 1)
    }
}
impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

struct Flow {
    /// Declared before the directory: it signals the pid the directory holds.
    _pids: PidGuard,
    _previous: Option<tempfile::TempDir>,
    _directory: tempfile::TempDir,
    base: PathBuf,
    served: Option<Served>,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    thread: String,
    command: Option<Socket>,
    next: u64,
    log: PathBuf,
    status: PathBuf,
    _dead: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// The recording's entries with the directory it ran in replaced by where the test runs.
fn entries_of(row: &Value, workspace: &Path) -> Vec<Value> {
    let text = row["transcript"].to_string().replace(row["workspace"].as_str().unwrap_or("\u{0}"), &workspace.to_string_lossy());
    let mut entries: Vec<Value> = serde_json::from_str(&text).unwrap();
    fn locate(value: &mut Value, workspace: &str) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(|item| locate(item, workspace)),
            Value::Object(object) => {
                if object.get("directory").is_some_and(|directory| directory == "<work>") {
                    object.insert("directory".into(), json!(workspace));
                }
                object.values_mut().for_each(|item| locate(item, workspace));
            }
            _ => {}
        }
    }
    entries.iter_mut().for_each(|entry| locate(entry, &workspace.to_string_lossy()));
    entries
}
/// What a server T3 started does not do: the source's closing abort of an external server's sessions.
fn without_cleanup(entries: Vec<Value>) -> Vec<Value> {
    entries.into_iter().filter(|entry| entry["type"] != "runtime_exit" && !entry["label"].as_str().is_some_and(|label| label.contains("cleanup"))).collect()
}
fn session_of(entries: &[Value]) -> String {
    entries.iter().find(|entry| entry["label"] == "session.created").unwrap()["frame"]["data"]["id"].as_str().unwrap().to_owned()
}
fn position(entries: &[Value], label: &str) -> usize {
    entries.iter().position(|entry| entry["label"] == label).unwrap_or_else(|| panic!("no {label}"))
}

impl Flow {
    /// `entries` is what the spawned server replays; `store` continues an earlier flow's thread (a restart).
    async fn start(entries: impl FnOnce(&Path) -> Vec<Value>, runtime_mode: &str, selection: &Value, resume: Option<(Store, PathBuf, String, tempfile::TempDir)>) -> Self {
        Self::start_with(entries, runtime_mode, selection, resume, false).await
    }
    /// `mcp`: the registry issues a credential for T3's MCP endpoint to each thread, as the real server does.
    async fn start_with(entries: impl FnOnce(&Path) -> Vec<Value>, runtime_mode: &str, selection: &Value, resume: Option<(Store, PathBuf, String, tempfile::TempDir)>, mcp: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let (workspace, home) = match &resume {
            Some((_, workspace, _, _)) => (workspace.clone(), base.join("home")),
            None => (base.join("workspace"), base.join("home")),
        };
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let transcript = base.join("transcript.json");
        std::fs::write(&transcript, json!({"scenario":"turn","protocol":"opencode-sdk.sse","version":"1.14.39","entries":entries(&workspace)}).to_string()).unwrap();
        let (pidfile, log, status, death) = (base.join("server.pid"), base.join("requests.jsonl"), base.join("replay-status.json"), base.join("death.fifo"));
        let path = std::ffi::CString::new(death.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let wrapper = base.join("bin-opencode");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexport OPENCODE_FIXTURE_VERSION=1.14.39\nexport OPENCODE_FIXTURE_TRANSCRIPT={}\nexport OPENCODE_FIXTURE_PIDFILE={}\nexport OPENCODE_FIXTURE_LOG={}\nexport OPENCODE_FIXTURE_STATUS={}\nexport OPENCODE_FIXTURE_DEATHPIPE={}\nexec python3 {} \"$@\"\n", transcript.display(), pidfile.display(), log.display(), status.display(), death.display(), opencode_support::FAKE)).unwrap();
        std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // The server opens its end of this pipe when it starts, which blocks until something reads; the read ends when it dies.
        let dead = {
            let death = death.clone();
            tokio::task::spawn_blocking(move || std::fs::read(death).map(|_| ()))
        };
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"responseStreamingMode":"turn","providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "opencode":{"driver":"opencode","enabled":true,"config":{"enabled":true,"binaryPath":wrapper},"environment":[
                {"name":"HOME","value":home},{"name":"XDG_DATA_HOME","value":home.join("share")},{"name":"OPENCODE_API_KEY","value":""}]}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &workspace).await.unwrap();
        let mut previous_directory = None;
        let (store, thread) = match resume {
            Some((store, _, thread, previous)) => {
                previous_directory = Some(previous);
                (store, thread)
            }
            None => {
                let store = Store::open(base.join("rust-state.sqlite")).unwrap();
                ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
                let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
                    .launch(json!({"commandId":"launch","projectId":"project","title":"OpenCode","modelSelection":selection,"runtimeMode":runtime_mode,"interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
                    .unwrap()["threadId"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                (store, thread)
            }
        };
        if mcp {
            let registry = t3_server::mcp_sessions::McpSessionRegistry::new("environment".parse().unwrap(), Some("127.0.0.1:45999".parse().unwrap()), std::sync::Arc::new(|| Utc::now().timestamp_millis()), 10000);
            providers.set_mcp_sessions(t3_server::provider_mcp::ProviderMcpSessions::new(registry, PathBuf::from(env!("CARGO_BIN_EXE_t3-server"))));
        }
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let served = serve(&store, &providers, &execution).await;
        let command = served.connect().await;
        let next = if previous_directory.is_some() { 100 } else { 0 };
        Self { _pids: PidGuard(pidfile), _previous: previous_directory, _directory: directory, base, served: Some(served), store, execution, providers, thread, command: Some(command), next, log, status, _dead: Some(dead) }
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    async fn dispatch(&mut self, command: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let exit = rpc(self.command.as_mut().unwrap(), id, "orchestration.dispatchCommand", command).await;
        success(&exit).clone()
    }
    async fn send(&mut self, text: &str) -> usize {
        let id = format!("message-{}", self.next + 1);
        self.dispatch(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        within(format!("{what}; runs {}; replay {}", self.view()["runs"], std::fs::read_to_string(&self.status).unwrap_or_default()), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
    }
    async fn run_status(&self, ordinal: usize, wanted: &str) {
        self.until(&format!("run {ordinal} to be {wanted}"), |view| (view["runs"].get(ordinal - 1)?["status"] == wanted).then_some(())).await
    }
    async fn pending(&self) -> Value {
        self.until("a pending request", |view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    /// What the server was asked, in order: the operation names with their inputs.
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    fn operations(&self) -> Vec<String> {
        self.received().iter().map(|request| request["op"].as_str().unwrap().to_owned()).collect()
    }
    /// The replay's own verdict: no request differed from the recording, however the adapter answered it.
    fn replay_failure(&self) -> Value {
        serde_json::from_str::<Value>(&std::fs::read_to_string(&self.status).unwrap()).unwrap()["failure"].clone()
    }
    /// Stops the transport and the execution service. The server the instance started ends with its registry (the
    /// driver test proves that); a registry still held by a background task would keep it for the idle window, so the
    /// recorded pid ends it here instead.
    async fn stop(mut self) -> (Store, PathBuf, String, tempfile::TempDir) {
        self.command = None;
        self.served.take().unwrap().stop().await;
        self.execution.shutdown().await;
        let (store, thread, workspace) = (self.store.clone(), self.thread.clone(), self.base.join("workspace"));
        drop(self.providers);
        (store, workspace, thread, self._directory)
    }
}

fn texts(view: &Value, kind: &str) -> Vec<String> {
    view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == kind).filter_map(|item| item["text"].as_str().map(str::to_owned)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_runs_on_the_server_the_instance_started() {
    let simple = row_of("simple");
    let mut flow = Flow::start(|work| without_cleanup(entries_of(&simple, work)), "full-access", &simple["modelSelection"].clone(), None).await;
    // The 1.x install is listed ready, with its models, before any thread uses it.
    let listed = flow.providers.snapshots().into_iter().find(|snapshot| snapshot["driver"] == "opencode").unwrap();
    assert_eq!((listed["status"].clone(), listed["version"].clone()), (json!("ready"), json!("1.14.39")), "{listed}");
    let runs = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    flow.run_status(runs, "completed").await;
    let view = flow.view();
    assert_eq!(texts(&view, "assistant_message"), ["fixture simple ok"]);
    // The thread's mode is the session's ruleset, and the prompt named its model and agent.
    let created = flow.received().into_iter().find(|request| request["op"] == "session.create").unwrap();
    assert_eq!(created["input"]["permission"], json!([{"permission":"*","pattern":"*","action":"allow"}]));
    let prompted = flow.received().into_iter().find(|request| request["op"] == "session.promptAsync").unwrap();
    assert_eq!((prompted["input"]["model"].clone(), prompted["input"]["agent"].clone()), (json!({"providerID":"openai","modelID":"gpt-5.4-mini"}), json!("build")));
    assert!(prompted["input"]["messageID"].as_str().unwrap().starts_with("msg_"));
    // T3's runtime instructions ride in the prompt's `system`, as OpenCode 1.x has no other place for them.
    assert!(prompted["input"]["system"].as_str().unwrap().contains("OpenCode"), "{prompted}");
    assert_eq!(view["providerThreads"][0]["driver"], "opencode");
    assert_eq!(flow.replay_failure(), Value::Null);
    // The server T3 started was reached with no password (the install has none), on loopback.
    flow.stop().await;
}

/// `simple` with a permission request of the root session between its first step and its text.
fn with_permission(simple: &[Value], reply: &str) -> Vec<Value> {
    let session = session_of(simple);
    let at = position(simple, "step-start");
    let mut entries = simple[..at].to_vec();
    entries.extend([
        json!({"type":"emit_inbound","label":"permission.asked","frame":{"type":"sdk.event","event":{"id":"evt_perm","type":"permission.asked","properties":{"id":"per_root0001","sessionID":session,"permission":"bash","patterns":["git status"],"metadata":{},"always":["git status"],"tool":{"messageID":"msg_tool","callID":"call_bash"}}}}}),
        json!({"type":"expect_outbound","label":"permission.reply","frame":{"type":"permission.reply","input":{"requestID":"per_root0001","reply":reply}}}),
        json!({"type":"emit_inbound","label":"permission.reply.response","frame":{"type":"sdk.response","operation":"permission.reply","data":true}}),
        json!({"type":"emit_inbound","label":"permission.replied","frame":{"type":"sdk.event","event":{"id":"evt_replied","type":"permission.replied","properties":{"sessionID":session,"requestID":"per_root0001","reply":reply}}}}),
    ]);
    entries.extend(simple[at..].iter().cloned());
    entries
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_is_answered_over_the_socket() {
    let simple = row_of("simple");
    for (decision, reply) in [("accept", "once"), ("acceptForSession", "always"), ("decline", "reject")] {
        let mut flow = Flow::start(|work| without_cleanup(with_permission(&entries_of(&simple, work), reply)), "approval-required", &simple["modelSelection"].clone(), None).await;
        let runs = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
        let request = flow.pending().await;
        assert_eq!(request["kind"], "command");
        let item = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").cloned().unwrap();
        assert_eq!((item["title"].clone(), item["prompt"].clone(), item["status"].clone()), (json!("bash"), json!("git status"), json!("waiting")));
        flow.dispatch(json!({"type":"runtime-request.respond","commandId":"approve","threadId":flow.thread,"requestId":request["id"],"decision":decision})).await;
        flow.run_status(runs, "completed").await;
        let replies: Vec<Value> = flow.received().into_iter().filter(|request| request["op"] == "permission.reply").collect();
        assert_eq!(replies.len(), 1, "{decision}");
        assert_eq!(replies[0]["input"]["reply"], reply);
        assert_eq!(flow.view()["runtimeRequests"][0]["status"], "resolved");
        // Supervised, the session was made with rules that ask before a command.
        let created = flow.received().into_iter().find(|request| request["op"] == "session.create").unwrap();
        assert!(created["input"]["permission"].as_array().unwrap().iter().any(|rule| rule["permission"] == "bash" && rule["action"] == "ask"));
        assert_eq!(flow.replay_failure(), Value::Null);
        flow.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_answered_over_the_socket() {
    let row = row_of("plan_questions");
    let mut flow = Flow::start(|work| without_cleanup(entries_of(&row, work)), "full-access", &row["modelSelection"].clone(), None).await;
    let runs = flow.send(row["steps"][0]["text"].as_str().unwrap()).await;
    let request = flow.pending().await;
    assert_eq!(request["kind"], "user_input");
    let asked = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_input_request").cloned().unwrap();
    assert_eq!(asked["questions"][0]["id"], "question-0-schema-vs-flexibility");
    flow.dispatch(json!({"type":"runtime-request.respond","commandId":"answer","threadId":flow.thread,"requestId":request["id"],"answers":row["steps"][1]["answers"]})).await;
    flow.run_status(runs, "completed").await;
    let reply = flow.received().into_iter().find(|request| request["op"] == "question.reply").unwrap();
    assert_eq!((reply["input"]["requestID"].clone(), reply["input"]["answers"].clone()), (json!("que_edc9624d9001e2GFKX724Z8b5Z"), json!([["Strict schemas"]])));
    assert_eq!(texts(&flow.view(), "assistant_message"), ["plan questions fixture complete"]);
    assert_eq!(flow.replay_failure(), Value::Null);
    flow.stop().await;
}

/// The first recording's turn, then the second's prompt and what follows it on the first's session.
fn continued(first: &[Value], second: &[Value]) -> Vec<Value> {
    let (from, to) = (session_of(second), session_of(first));
    let prompt = position(second, "session.promptAsync");
    let tail = Value::Array(second[prompt..].to_vec()).to_string().replace(&from, &to);
    let mut entries = without_cleanup(first.to_vec());
    entries.extend(without_cleanup(serde_json::from_str(&tail).unwrap()));
    entries
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_the_turn_and_the_next_turn_runs_on_the_same_session() {
    let interrupted = row_of("turn_interrupt");
    let simple = row_of("simple");
    let mut flow = Flow::start(|work| continued(&entries_of(&interrupted, work), &entries_of(&simple, work)), "full-access", &interrupted["modelSelection"].clone(), None).await;
    let runs = flow.send(interrupted["steps"][0]["text"].as_str().unwrap()).await;
    let run = flow.until("the turn to run", |view| (view["runs"][0]["status"] == "running").then(|| view["runs"][0].clone())).await;
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run["id"]})).await;
    flow.run_status(runs, "interrupted").await;
    let second = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    flow.run_status(second, "completed").await;
    assert_eq!(texts(&flow.view(), "assistant_message"), ["fixture simple ok"]);
    let operations = flow.operations();
    // One session served both turns; the Stop aborted it and looked for sessions under it.
    assert_eq!(operations.iter().filter(|op| *op == "session.create").count(), 1);
    assert_eq!(operations.iter().filter(|op| *op == "session.abort").count(), 1, "{operations:?}");
    assert!(operations.contains(&"session.children".to_owned()));
    assert_eq!(flow.replay_failure(), Value::Null);
    flow.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_approval_is_answered_while_its_parent_runs() {
    let row = row_of("opencode_child_approval");
    let mut flow = Flow::start(|work| without_cleanup(entries_of(&row, work)), "approval-required", &row["modelSelection"].clone(), None).await;
    let runs = flow.send(row["steps"][0]["text"].as_str().unwrap()).await;
    // The subagent's session asked, and the user sees it on the parent's turn while the parent has not finished.
    let request = flow.pending().await;
    assert_eq!(request["kind"], "command");
    assert_eq!(flow.view()["runs"][0]["status"], "running");
    flow.dispatch(json!({"type":"runtime-request.respond","commandId":"approve","threadId":flow.thread,"requestId":request["id"],"decision":"accept"})).await;
    flow.run_status(runs, "completed").await;
    let reply = flow.received().into_iter().find(|request| request["op"] == "permission.reply").unwrap();
    assert_eq!((reply["input"]["requestID"].clone(), reply["input"]["reply"].clone()), (json!("perm_childbash0001"), json!("once")));
    // The subagent has a thread of its own, holding what it said.
    let view = flow.view();
    let child = view["subagents"][0]["childThreadId"].as_str().unwrap().to_owned();
    let child_view = flow.store.projection("thread", &child).unwrap().unwrap();
    assert_eq!(texts(&child_view, "assistant_message"), ["CHILD_OK"]);
    assert_eq!(texts(&child_view, "user_message"), ["Respond exactly CHILD_OK."]);
    // The child's session was given the thread's complete policy (supervised), not only the denials it inherited.
    let update = flow.received().into_iter().find(|request| request["op"] == "session.update").unwrap();
    assert!(update["input"]["permission"].as_array().unwrap().len() > 3, "{update}");
    assert_eq!(flow.replay_failure(), Value::Null);
    flow.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_continues_on_its_session_after_the_server_restarted() {
    let simple = row_of("simple");
    let mut first = Flow::start(|work| without_cleanup(entries_of(&simple, work)), "approval-required", &simple["modelSelection"].clone(), None).await;
    let runs = first.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    first.run_status(runs, "completed").await;
    let session = session_of(&entries_of(&simple, Path::new("/unused")));
    let (store, workspace, thread, directory) = first.stop().await;

    // A new server knows the session: it is read back, and the thread's rules are written to it again, since the thread's
    // mode may have changed while nothing was watching.
    let source = entries_of(&simple, &workspace);
    let prompt = position(&source, "session.promptAsync");
    let mut entries: Vec<Value> = vec![source[0].clone()];
    entries.extend([
        json!({"type":"expect_outbound","label":"session.get","frame":{"type":"session.get","input":{"sessionID":session}}}),
        json!({"type":"emit_inbound","label":"session.get.response","frame":{"type":"sdk.response","operation":"session.get","data":source[2]["frame"]["data"]}}),
        json!({"type":"expect_outbound","label":"session.update","frame":{"type":"session.update","input":{"sessionID":session,"permission":"<any>"}}}),
        json!({"type":"emit_inbound","label":"session.update.response","frame":{"type":"sdk.response","operation":"session.update","data":source[2]["frame"]["data"]}}),
    ]);
    entries.extend(without_cleanup(source[prompt..].to_vec()));
    let mut second = Flow::start(|_| entries, "approval-required", &simple["modelSelection"].clone(), Some((store, workspace, thread, directory))).await;
    let runs = second.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    second.run_status(runs, "completed").await;
    let operations = second.operations();
    assert!(operations.contains(&"session.get".to_owned()), "the session is read back: {operations:?}");
    assert!(!operations.contains(&"session.create".to_owned()), "{operations:?}");
    let update = second.received().into_iter().find(|request| request["op"] == "session.update").unwrap();
    assert_eq!(update["input"]["sessionID"], session.as_str());
    assert!(update["input"]["permission"].as_array().unwrap().iter().any(|rule| rule["permission"] == "bash" && rule["action"] == "ask"));
    assert_eq!(second.view()["runs"].as_array().unwrap().len(), 2);
    assert_eq!(second.replay_failure(), Value::Null);
    second.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn t3s_mcp_server_is_registered_for_the_thread_and_dropped_with_its_session() {
    let simple = row_of("simple");
    let mut flow = Flow::start_with(
        |work| {
            let recorded = without_cleanup(entries_of(&simple, work));
            // Added before the stream is subscribed, under the thread's own name, and disconnected once the session closes.
            let mut entries = vec![
                json!({"type":"expect_outbound","label":"mcp.add","frame":{"type":"mcp.add","input":{"name":"<any>","config":{"type":"remote","url":"<any>","headers":{"Authorization":"<any>"},"oauth":false}}}}),
                json!({"type":"emit_inbound","label":"mcp.add.response","frame":{"type":"sdk.response","operation":"mcp.add","data":{}}}),
            ];
            entries.extend(recorded);
            entries.extend([
                json!({"type":"expect_outbound","label":"mcp.disconnect","frame":{"type":"mcp.disconnect","input":{"name":"<any>"}}}),
                json!({"type":"emit_inbound","label":"mcp.disconnect.response","frame":{"type":"sdk.response","operation":"mcp.disconnect","data":true}}),
            ]);
            entries
        },
        "approval-required",
        &simple["modelSelection"].clone(),
        None,
        true,
    )
    .await;
    let runs = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    flow.run_status(runs, "completed").await;
    let received = flow.received();
    let added = received.iter().find(|request| request["op"] == "mcp.add").unwrap();
    let name = added["input"]["name"].as_str().unwrap().to_owned();
    assert!(name.starts_with("t3-code-"), "{name}");
    assert!(added["input"]["config"]["headers"]["Authorization"].as_str().unwrap().starts_with("Bearer "), "{added}");
    // The thread's sessions may use its own server's tools and no other thread's.
    let rules = received.iter().find(|request| request["op"] == "session.create").unwrap()["input"]["permission"].as_array().unwrap().clone();
    let (deny, allow) = (rules.len() - 2, rules.len() - 1);
    assert_eq!((rules[deny]["permission"].clone(), rules[deny]["action"].clone()), (json!("t3-code-*"), json!("deny")));
    assert_eq!((rules[allow]["permission"].clone(), rules[allow]["action"].clone()), (json!(format!("{name}_*")), json!("allow")));
    // The server's tools are described to the model.
    let prompted = received.iter().find(|request| request["op"] == "session.promptAsync").unwrap();
    assert!(prompted["input"]["system"].as_str().unwrap().len() > 200, "{prompted}");
    let (status, log) = (flow.status.clone(), flow.log.clone());
    let kept = flow.stop().await;
    let status: Value = serde_json::from_str(&std::fs::read_to_string(status).unwrap()).unwrap();
    assert_eq!(status["failure"], Value::Null, "{status}");
    assert!(std::fs::read_to_string(log).unwrap().contains("mcp.disconnect"));
    drop(kept);
}
