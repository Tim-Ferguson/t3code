#![cfg(unix)]
//! OpenCode turns through the real WebSocket transport, against the stand-in server a spawned `opencode serve` would be
//! (`fixtures/opencode-fake-cli.py`, started by the driver itself from a wrapper script at an absolute path in the temp
//! directory): a turn, an approval, a question, a Stop followed by another turn, and a thread continued after the
//! server restarted. The recordings are the source's; the two sessions a restart needs are cut from them. Commands go
//! over one socket and each stream has a socket of its own, whose chunks are acknowledged. HOME and the data home stay
//! in the temp directory, no real `opencode` runs, and the server a test started is ended on every exit path.
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
    _dead: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

/// The recording's session id and its `<work>` replaced where the test runs.
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

impl Flow {
    /// `entries` is what the spawned server replays; `store` continues an earlier flow's thread (a restart).
    async fn start(entries: impl FnOnce(&Path) -> Vec<Value>, runtime_mode: &str, selection: &Value, resume: Option<(Store, PathBuf, String, tempfile::TempDir)>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let (workspace, home) = match &resume {
            Some((_, workspace, _, _)) => (workspace.clone(), base.join("home")),
            None => (base.join("workspace"), base.join("home")),
        };
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let transcript = base.join("transcript.json");
        std::fs::write(&transcript, json!({"scenario":"turn","protocol":"opencode2-http.sse","version":"2.0.18","entries":entries(&workspace)}).to_string()).unwrap();
        let (pidfile, log, death) = (base.join("server.pid"), base.join("requests.jsonl"), base.join("death.fifo"));
        let path = std::ffi::CString::new(death.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let wrapper = base.join("bin-opencode");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexport OPENCODE_FIXTURE_VERSION=2.0.18\nexport OPENCODE_FIXTURE_TRANSCRIPT={}\nexport OPENCODE_FIXTURE_PIDFILE={}\nexport OPENCODE_FIXTURE_LOG={}\nexport OPENCODE_FIXTURE_DEATHPIPE={}\nexec python3 {} \"$@\"\n", transcript.display(), pidfile.display(), log.display(), death.display(), opencode_support::FAKE)).unwrap();
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
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let served = serve(&store, &providers, &execution).await;
        let command = served.connect().await;
        let next = if previous_directory.is_some() { 100 } else { 0 };
        Self { _pids: PidGuard(pidfile), _previous: previous_directory, _directory: directory, base, served: Some(served), store, execution, providers, thread, command: Some(command), next, log, _dead: Some(dead) }
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
        within(format!("{what}; runs {}", self.view()["runs"]), async {
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

const SELECTION: fn() -> Value = || json!({"instanceId":"opencode","model":"opencode/space-bunny-free","options":[{"id":"variant","value":"high"}]});

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_runs_on_the_server_the_instance_started() {
    let simple = row_of("opencode2_simple");
    let mut flow = Flow::start(|work| entries_of(&simple, work), "full-access", &SELECTION(), None).await;
    // The instance is listed and ready before any thread uses it.
    let listed = flow.providers.snapshots().into_iter().find(|snapshot| snapshot["driver"] == "opencode").unwrap();
    assert_eq!(listed["status"], "ready", "{listed}");
    let runs = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    flow.run_status(runs, "completed").await;
    let view = flow.view();
    let texts: Vec<&str> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "assistant_message").filter_map(|item| item["text"].as_str()).collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("17"), "{texts:?}");
    // The thread named the model it selected, with its reasoning variant, when its session was made.
    let created = flow.received().into_iter().find(|request| request["op"] == "session.create").unwrap();
    assert_eq!(created["input"]["model"], json!({"providerID":"opencode","id":"space-bunny-free","variant":"high"}));
    assert_eq!(view["providerThreads"][0]["driver"], "opencode");
    flow.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn four_approvals_are_answered_over_the_socket() {
    let row = row_of("opencode2_permission");
    let mut flow = Flow::start(|work| entries_of(&row, work), "approval-required", &row["modelSelection"].clone(), None).await;
    let runs = flow.send(row["steps"][0]["text"].as_str().unwrap()).await;
    for step in 0..4 {
        let request = flow.pending().await;
        assert_eq!(request["kind"], "command");
        // The recording accepts the first command and declines the three after it.
        let decision = if step == 0 { "accept" } else { "decline" };
        flow.dispatch(json!({"type":"runtime-request.respond","commandId":format!("approve-{step}"),"threadId":flow.thread,"requestId":request["id"],"decision":decision})).await;
    }
    flow.run_status(runs, "completed").await;
    let replies: Vec<Value> = flow.received().into_iter().filter(|request| request["op"] == "permission.reply").collect();
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0]["input"]["decision"], "once");
    assert!(replies[1..].iter().all(|reply| reply["input"]["decision"] == "reject"), "{replies:?}");
    flow.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_answered_over_the_socket() {
    let row = row_of("opencode2_question");
    let mut flow = Flow::start(|work| entries_of(&row, work), "full-access", &row["modelSelection"].clone(), None).await;
    let runs = flow.send(row["steps"][0]["text"].as_str().unwrap()).await;
    let request = flow.pending().await;
    assert_eq!(request["kind"], "user_input");
    flow.dispatch(json!({"type":"runtime-request.respond","commandId":"answer","threadId":flow.thread,"requestId":request["id"],"answers":row["steps"][1]["answers"]})).await;
    flow.run_status(runs, "completed").await;
    let reply = flow.received().into_iter().find(|request| request["op"] == "session.form.reply").unwrap();
    assert_eq!(reply["input"]["answer"], json!({"q0":"blue"}));
    flow.stop().await;
}

/// The first recording's session id replaces the second's, so the second's turn runs on the first's session.
fn continued(first: &[Value], second: &[Value], from: &str, to: &str) -> Vec<Value> {
    let prompt = second.iter().position(|entry| entry["label"].as_str().is_some_and(|label| label.starts_with("session.prompt")) && entry["type"] == "expect_outbound").unwrap();
    let tail = Value::Array(second[prompt..].to_vec()).to_string().replace(from, to);
    let mut entries: Vec<Value> = first.iter().filter(|entry| entry["type"] != "runtime_exit").cloned().collect();
    entries.extend(serde_json::from_str::<Vec<Value>>(&tail).unwrap().into_iter().filter(|entry| entry["type"] != "runtime_exit"));
    entries.push(json!({"type":"runtime_exit","status":"success"}));
    entries
}
fn session_of(entries: &[Value]) -> String {
    entries.iter().find(|entry| entry["label"] == "session.create.response").unwrap()["frame"]["data"]["data"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_the_turn_and_the_next_turn_runs_on_the_same_session() {
    let interrupted = row_of("opencode2_interrupt");
    let simple = row_of("opencode2_simple");
    let mut flow = Flow::start(
        |work| {
            let (first, second) = (entries_of(&interrupted, work), entries_of(&simple, work));
            continued(&first, &second, &session_of(&second), &session_of(&first))
        },
        "full-access",
        &interrupted["modelSelection"].clone(),
        None,
    ).await;
    let runs = flow.send(interrupted["steps"][0]["text"].as_str().unwrap()).await;
    let run = flow.until("the command to run", |view| view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "command_execution").then(|| view["runs"][0].clone())).await;
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run["id"]})).await;
    flow.run_status(runs, "interrupted").await;
    let second_runs = flow.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    flow.run_status(second_runs, "completed").await;
    let sessions: std::collections::HashSet<String> = flow.received().iter().filter(|request| matches!(request["op"].as_str(), Some("session.prompt" | "session.interrupt"))).map(|request| request["input"]["sessionID"].as_str().unwrap().to_owned()).collect();
    assert_eq!(sessions.len(), 1, "both turns ran on one session");
    assert_eq!(flow.operations().iter().filter(|op| *op == "session.create").count(), 1);
    flow.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_continues_on_its_session_after_the_server_restarted() {
    let simple = row_of("opencode2_simple");
    let mut first = Flow::start(|work| entries_of(&simple, work), "full-access", &SELECTION(), None).await;
    let runs = first.send(simple["steps"][0]["text"].as_str().unwrap()).await;
    first.run_status(runs, "completed").await;
    let created = first.received().into_iter().find(|request| request["op"] == "session.create").unwrap();
    let session = session_of(&entries_of(&simple, Path::new("/unused")));
    let permissions = created["input"]["permissions"].clone();
    let (store, workspace, thread, directory) = first.stop().await;

    // A new server knows the session: it is read back, its open requests are listed, and the thread's rules already match.
    let source = entries_of(&simple, &workspace);
    let prompt = source.iter().position(|entry| entry["label"] == "session.prompt").unwrap();
    let mut entries: Vec<Value> = source[..3].to_vec();
    let get_response = json!({"data":{"id":session,"projectID":"global","model":{"providerID":"opencode","id":"space-bunny-free","variant":"high"},"cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1,"updated":1},"title":"restarted","location":{"directory":workspace},"permissions":permissions}});
    entries.extend([
        json!({"type":"emit_inbound","label":"server.connected","frame":{"type":"sdk.event","event":{"id":"evt_1","type":"server.connected","data":{}}}}),
        json!({"type":"expect_outbound","label":"session.get","frame":{"type":"session.get","input":{"sessionID":session}}}),
        json!({"type":"emit_inbound","label":"session.get.response","frame":{"type":"sdk.response","operation":"session.get","data":get_response}}),
        json!({"type":"expect_outbound","label":"permission.list","frame":{"type":"permission.list","input":{"sessionID":session}}}),
        json!({"type":"emit_inbound","label":"permission.list.response","frame":{"type":"sdk.response","operation":"permission.list","data":{"data":[]}}}),
        json!({"type":"expect_outbound","label":"form.list","frame":{"type":"session.form.list","input":{"sessionID":session}}}),
        json!({"type":"emit_inbound","label":"form.list.response","frame":{"type":"sdk.response","operation":"session.form.list","data":{"data":[]}}}),
    ]);
    // The recording's own turn, from the instructions it writes to its end.
    let put = source.iter().position(|entry| entry["label"] == "session.instructions.entry.put").unwrap();
    entries.extend(source[put..].iter().cloned());
    let _ = prompt;
    let mut second = Flow::start(|_| entries, "full-access", &SELECTION(), Some((store, workspace, thread, directory))).await;
    let runs = second.send("And again?").await;
    second.run_status(runs, "completed").await;
    let operations = second.operations();
    let get = operations.iter().position(|op| op == "session.get").unwrap_or_else(|| panic!("the session is read back: {operations:?}"));
    assert!(operations[..get].iter().all(|op| op != "session.create") && !operations.contains(&"session.create".to_owned()), "{operations:?}");
    assert_eq!(second.view()["runs"].as_array().unwrap().len(), 2);
    second.stop().await;
}
