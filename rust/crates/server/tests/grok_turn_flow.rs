#![cfg(unix)]
//! Grok turns through the real WebSocket transport, against a scripted stand-in for `grok agent stdio`
//! (`fixtures/grok-turn-agent.py`, run by `fixtures/grok-fake-cli.py`): a turn, an approval and the choices it
//! offers, a Stop and the next turn reconnecting to the saved session, and what a Stop ends of the tools Grok
//! started. Commands go over one socket and each stream has a socket of its own, whose chunks are acknowledged.
//! No real `grok` runs; HOME and GROK_HOME stay in the temp directory. Tests wait on persisted events and on
//! the fifo a tool writes when it exits, never on sleeps.
mod grok_support;
mod provider_manage_support;
use chrono::Utc;
use grok_support::{Served, Socket, Stream, serve};
use provider_manage_support::{blocking_step, rpc, success, within};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok-fake-cli.py");
const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok-turn-agent.py");

struct Flow {
    _directory: tempfile::TempDir,
    served: Served,
    store: Store,
    execution: ExecutionService,
    thread: String,
    command: Socket,
    next: u64,
    log: PathBuf,
    detached: PathBuf,
}

impl Flow {
    async fn start(script: Value, runtime_mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = base.join("workspace");
        let (home, detached) = (base.join("home"), base.join("detached"));
        for path in [&workspace, &home.join(".grok"), &detached] {
            std::fs::create_dir_all(path).unwrap();
        }
        let (script_path, log) = (base.join("script.json"), base.join("agent.jsonl"));
        std::fs::write(&script_path, script.to_string()).unwrap();
        // The executable is a wrapper at an absolute path in the temp directory.
        let wrapper = base.join("bin-grok");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n")).unwrap();
        std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "grok":{"driver":"grok","enabled":true,"config":{"enabled":true,"binaryPath":wrapper},"environment":[
                {"name":"HOME","value":home},{"name":"GROK_HOME","value":home.join(".grok")},{"name":"GROK_FIXTURE_AGENT","value":AGENT},
                {"name":"GROK_FIXTURE_SCRIPT","value":script_path},{"name":"GROK_FIXTURE_LOG","value":log},{"name":"GROK_FIXTURE_DETACH_DIR","value":detached}]}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
        let store = Store::open(base.join("rust-state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"Grok","modelSelection":{"instanceId":"grok","model":"grok-4.7"},"runtimeMode":runtime_mode,"interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let served = serve(&store, &providers, &execution).await;
        let command = served.connect().await;
        Self { _directory: directory, served, store, execution, thread, command, next: 0, log, detached }
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    /// One command over the socket, as a client sends it.
    async fn dispatch(&mut self, command: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let exit = rpc(&mut self.command, id, "orchestration.dispatchCommand", command).await;
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
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    /// What a tool the agent detaches writes to its fifo when it is told to end. The fifo exists, and a reader waits
    /// on it, before the tool can end: the tool's write blocks until someone reads.
    fn exit_of(&self, name: &str) -> tokio::task::JoinHandle<String> {
        let fifo = self.detached.join(format!("{name}.fifo"));
        let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let name = name.to_owned();
        tokio::spawn(blocking_step(format!("the detached tool {name} to be told to end"), move || std::fs::read_to_string(&fifo).unwrap()))
    }
    async fn finish(self) {
        self.served.stop().await;
        self.execution.shutdown().await;
    }
}

fn update(value: Value) -> Value {
    json!({"update": value})
}
fn text(body: &str) -> Value {
    update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":body}}))
}

fn permission() -> Value {
    json!({"request":{"method":"session/request_permission","params":{
        "toolCall":{"toolCallId":"run-ls","title":"Run ls","kind":"execute","rawInput":{"command":"ls"}},
        "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"allow-edits-session","name":"Allow edits this session","kind":"allow_always"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]}}})
}
/// A command Grok moved to the background: its tool says so, and the extension announces the task.
fn background_command(call: &str, task: &str) -> Vec<Value> {
    vec![
        update(json!({"sessionUpdate":"tool_call","toolCallId":call,"title":"run_terminal_command","kind":"execute","rawInput":{"command":"sleep 600","description":"Build"}})),
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":call,"status":"completed","title":"[bg] sleep 600","rawOutput":{"type":"BackgroundTaskStarted","task_id":task,"task_type":"bash","command":"sleep 600","status":"running"}})),
        json!({"ext":{"method":"_x.ai/task_backgrounded","params":{"update":{"sessionUpdate":"task_backgrounded","tool_call_id":call,"task_id":task,"command":"sleep 600"}}}}),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_offers_its_choices_a_stop_ends_the_turn_and_the_next_one_reloads_the_session() {
    let script = json!({"prompts":[
        [text("Running it."), permission(), text("Done."), {"end":"end_turn"}],
        [text("Working slowly."), {"hang":true}],
        [text("Back again."), {"end":"end_turn"}]]});
    let mut flow = Flow::start(script, "approval-required").await;
    let mut stream = Stream::open(&flow.served, "orchestration.subscribeThread", json!({"threadId":flow.thread})).await;
    stream.until("the thread snapshot", |chunks| chunks.iter().any(|chunk| chunk["kind"] == "snapshot")).await;

    // A turn that asks first: the approval carries the choices Grok can honor.
    assert_eq!(flow.send("List the files").await, 1);
    let request = flow.until("an approval to wait", |view| view["runtimeRequests"].as_array()?.iter().find(|request| request["status"] == "pending").cloned()).await;
    let approval = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").cloned().unwrap();
    assert_eq!(approval["prompt"], "ls");
    let decisions: Vec<&str> = approval["options"].as_array().unwrap().iter().map(|option| option["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, ["cancel", "decline", "acceptForSession", "accept"]);
    stream.until("the pending approval", |chunks| chunks.iter().any(|chunk| chunk.to_string().contains("approval_request"))).await;
    flow.dispatch(json!({"type":"runtime-request.respond","commandId":"approve","threadId":flow.thread,"requestId":request["id"],"decision":"accept"})).await;
    flow.run_status(1, "completed").await;
    let answer = flow.received().into_iter().find(|entry| entry["in"]["id"] == 101).unwrap();
    assert_eq!(answer["in"]["result"]["outcome"], json!({"outcome":"selected","optionId":"allow"}), "the choice reached the agent");
    let first_process = answer["pid"].clone();

    // A turn the user stops. The process goes with it, so the saved session is loaded by the next one.
    assert_eq!(flow.send("Take your time").await, 2);
    flow.until("the slow turn to start", |view| view["turnItems"].as_array()?.iter().any(|item| item["text"] == "Working slowly.").then_some(())).await;
    let run = flow.view()["runs"][1]["id"].clone();
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run})).await;
    flow.run_status(2, "interrupted").await;
    assert_eq!(flow.send("Continue").await, 3);
    flow.run_status(3, "completed").await;
    let received = flow.received();
    let loads: Vec<&Value> = received.iter().filter(|entry| entry["in"]["method"] == "session/load").collect();
    assert_eq!(loads.len(), 1, "{received:?}");
    assert_eq!(loads[0]["in"]["params"]["sessionId"], "grok-turn-session");
    assert_ne!(loads[0]["pid"], first_process, "a new process loaded it");
    assert_eq!(received.iter().filter(|entry| entry["in"]["method"] == "session/new").count(), 1, "one session was created in all");
    stream.until("the last turn", |chunks| chunks.iter().any(|chunk| chunk.to_string().contains("Back again."))).await;
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_held_turn_ends_the_tools_grok_detached() {
    let steps: Vec<Value> = [text("Starting the build."), json!({"detach":"build"})].into_iter().chain(background_command("bg-1", "task-1")).chain([json!({"end":"end_turn"})]).collect();
    let script = json!({"prompts":[steps]});
    let mut flow = Flow::start(script, "full-access").await;
    let exited = flow.exit_of("build");
    assert_eq!(flow.send("Build in the background").await, 1);
    // The prompt returned, but the command it started still runs: the run is held open for it.
    flow.until("the background command to run", |view| view["turnItems"].as_array()?.iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").map(|_| ())).await;
    flow.execution.drained(&flow.thread).await;
    assert_eq!(flow.view()["runs"][0]["status"], "running", "held open for the command");
    let run = flow.view()["runs"][0]["id"].clone();
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run})).await;
    flow.run_status(1, "interrupted").await;
    // The tool was started with the process, outlived its parent's death and was ended with the process group.
    assert_eq!(exited.await.unwrap().trim(), "exited");
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_settled_turn_ends_the_background_work_it_left() {
    // A persistent monitor does not hold its turn: the run completes while the monitor keeps running.
    let monitor = vec![
        update(json!({"sessionUpdate":"tool_call","toolCallId":"watch","title":"monitor","rawInput":{"variant":"Monitor","command":"tail -f log","description":"Watch the log","persistent":true}})),
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":"watch","status":"completed","rawOutput":{"type":"Monitor","taskId":"watch-1","persistent":true}})),
        json!({"ext":{"method":"_x.ai/task_backgrounded","params":{"update":{"sessionUpdate":"task_backgrounded","tool_call_id":"watch","task_id":"watch-1","command":"tail -f log","monitor_description":"Watch the log"}}}}),
    ];
    let steps: Vec<Value> = [text("Watching."), json!({"detach":"watch"})].into_iter().chain(monitor).chain([json!({"end":"end_turn"})]).collect();
    let script = json!({"prompts":[steps]});
    let mut flow = Flow::start(script, "full-access").await;
    let exited = flow.exit_of("watch");
    assert_eq!(flow.send("Watch the log").await, 1);
    flow.run_status(1, "completed").await;
    flow.execution.drained(&flow.thread).await;
    let monitor = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution").cloned().unwrap();
    assert_eq!(monitor["status"], "running", "the monitor outlives its turn: {monitor}");
    let run = flow.view()["runs"][0]["id"].clone();
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run,"holdQueue":true})).await;
    // What it left is ended in the thread, and the process group it ran in is ended with it.
    flow.until("the monitor to be interrupted", |view| view["turnItems"].as_array()?.iter().find(|item| item["type"] == "command_execution" && item["status"] == "interrupted").map(|_| ())).await;
    assert_eq!(exited.await.unwrap().trim(), "exited");
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_that_ignores_the_request_to_end_is_killed_once_the_grace_has_passed() {
    let steps: Vec<Value> = [text("Starting."), json!({"detach":"stubborn","stubborn":true})].into_iter().chain(background_command("bg-1", "task-1")).chain([json!({"end":"end_turn"})]).collect();
    let mut flow = Flow::start(json!({"prompts":[steps]}), "full-access").await;
    // The tool never says it ended: it holds the fifo open, and only its death closes it.
    let closed = flow.exit_of("stubborn");
    assert_eq!(flow.send("Build in the background").await, 1);
    flow.until("the background command to run", |view| view["turnItems"].as_array()?.iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").map(|_| ())).await;
    let run = flow.view()["runs"][0]["id"].clone();
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run})).await;
    flow.run_status(1, "interrupted").await;
    assert_eq!(closed.await.unwrap(), "", "it was killed, not asked");
    flow.finish().await;
}
