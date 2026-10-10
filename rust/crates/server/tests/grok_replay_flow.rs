#![cfg(unix)]
//! Replay parity for the Grok driver. The recordings are the Grok ACP sessions the source adapter was
//! recorded against (`apps/server/src/orchestration-v2/testkit/fixtures/*/grok_transcript.ndjson`);
//! `fixtures/grok-replay/<scenario>.json` holds the user-visible projection the unchanged TypeScript
//! orchestrator and `GrokAdapterV2` produced for each (see `tools/generate_grok_replay_fixtures.mjs`).
//! The driver runs `fixtures/grok-fake-cli.py`, which plays a recording through `fixtures/acp-replay-agent.py`:
//! it compares every frame the Rust client sends with the recorded one and fails the scenario on the first
//! difference. No real `grok` runs. Tests wait on persisted events, never on sleeps.
mod grok_support;
mod provider_manage_support;
use chrono::Utc;
use grok_support::{Served, Socket, Stream, serve};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use t3_server::{
    deferred_finalize::DEBOUNCE_MS, execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_idle::{IDLE_TIMEOUT_MS, MAX_PIN_MS},
    provider_registry::ProviderRegistry, scheduled_tasks::ManualClock,
};

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok-fake-cli.py");

struct Harness {
    status: PathBuf,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    /// The clock the quiet window of a held turn runs on: only the test moves it.
    clock: Arc<ManualClock>,
    thread: String,
    next: usize,
    /// The barriers the recording has, the `finish_held_run` steps the scenario has, and how many of those ran.
    barriers: usize,
    finishes: (usize, usize),
    gate: Option<PathBuf>,
    /// The scenario driven through the WebSocket transport instead of in process.
    remote: Option<Remote>,
    /// In process, the authentication service `main.rs` attaches (`serve` attaches its own).
    _auth: Option<t3_server::provider_auth_service::ProviderAuthService>,
}
struct Remote {
    served: Served,
    command: Socket,
    next: u64,
    /// The thread's stream, on a socket of its own, every chunk acknowledged.
    stream: Stream,
}

/// Where a barrier's marker shows: the context usage a frame the recording does not have reports.
const BARRIER_USAGE: u64 = 424_240;
/// The recording as the test plays it. A held turn settles when the clock moves past its quiet window, and the
/// test must move the clock only after every frame before that point has been taken in, so it places a barrier
/// there: a usage update that the actor handles in order with the frames around it. It stands before the frame a
/// `release_replay_gate` step holds back and before the session close the recording ends with. A gate holds its
/// frame until the test writes to a pipe.
fn with_barriers(transcript: &Value, gates: &[&str], root: &std::path::Path) -> (Value, Option<PathBuf>) {
    let entries = transcript.as_array().unwrap();
    let session = entries.iter().find(|entry| entry["label"] == "response:session/new").map(|entry| entry["frame"]["result"]["sessionId"].clone()).unwrap();
    let close = entries.iter().rposition(|entry| entry["type"] == "expect_outbound" && entry["frame"]["method"] == "session/close");
    let mut out = vec![];
    let mut marked = 0;
    // Every chunk of a turn shares its label; the gate holds the first.
    let mut open: Vec<&str> = gates.to_vec();
    for (index, entry) in entries.iter().enumerate() {
        let gated = open.iter().position(|label| entry["label"] == *label).map(|at| open.remove(at)).is_some();
        if gated || Some(index) == close {
            marked += 1;
            out.push(json!({"type":"emit_inbound","label":format!("barrier:{marked}"),"frame":{"kind":"notification","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"usage_update","used":BARRIER_USAGE + marked,"size":1_000_000}}}}));
        }
        out.push(entry.clone());
    }
    let fifo = (!gates.is_empty()).then(|| {
        let fifo = root.join("replay-gate.fifo");
        let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        fifo
    });
    (Value::Array(out), fifo)
}

impl Harness {
    async fn start(root: &std::path::Path, row: &Value, transcript: &Value, over_socket: bool) -> Self {
        let cwd = std::fs::canonicalize(root).unwrap();
        let status = cwd.join("replay-status.json");
        let path = cwd.join("transcript.json");
        let steps = row["steps"].as_array().unwrap();
        let gates: Vec<&str> = steps.iter().filter(|step| step["type"] == "release_replay_gate").filter_map(|step| step["label"].as_str()).collect();
        assert!(gates.len() <= 1, "the replay agent holds one gate");
        // Only a scenario that holds a run open needs the clock moved, and so needs the barriers.
        let holding = steps.iter().any(|step| step["type"] == "finish_held_run");
        let gates_or_holds = holding;
        let (transcript, gate) = if holding { with_barriers(transcript, &gates, &cwd) } else { (transcript.clone(), None) };
        let mut recording = json!({"scenario":row["name"],"entries":transcript});
        if let (Some(label), Some(fifo)) = (gates.first(), &gate) {
            recording["gate"] = json!({"label":label,"fifo":fifo});
        }
        std::fs::write(&path, recording.to_string()).unwrap();
        // The executable is a wrapper at an absolute path in the scratch directory; HOME and GROK_HOME stay inside it.
        let wrapper = cwd.join("bin-grok");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n")).unwrap();
        std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let home = cwd.join("home");
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        let settings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "grok":{"driver":"grok","enabled":true,"config":{"enabled":true,"binaryPath":wrapper},"environment":[
                {"name":"HOME","value":home},{"name":"GROK_HOME","value":home.join(".grok")},
                {"name":"GROK_FIXTURE_TRANSCRIPT","value":path},{"name":"GROK_FIXTURE_STATUS","value":status},{"name":"GROK_FIXTURE_WORKSPACE","value":cwd}]}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let clock = ManualClock::new(Utc::now().timestamp_millis());
        providers.idle_release().configure(clock.clone(), IDLE_TIMEOUT_MS, MAX_PIN_MS);
        // The original's in-memory replay delivers a whole recorded flood before the run that reports it can start;
        // a process pipe does not, so the run waits at the gate until the flood has been taken in.
        if gates_or_holds {
            providers.attach_gate().close();
        }
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":cwd}), Utc::now()).unwrap();
        let runtime_mode = if row["approvalPolicy"] == "on-request" { "approval-required" } else { row["runtimeMode"].as_str().unwrap() };
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":format!("Replay fixture: {}", row["name"].as_str().unwrap()),"modelSelection":{"instanceId":"grok","model":row["model"]},"runtimeMode":runtime_mode,"interactionMode":row["interactionMode"],"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let finishes = row["steps"].as_array().unwrap().iter().filter(|step| step["type"] == "finish_held_run").count();
        let barriers = if finishes == 0 { 0 } else { gates.len() + 1 };
        let remote = if over_socket {
            let served = serve(&store, &providers, &execution).await;
            let stream = Stream::open(&served, "orchestration.subscribeThread", json!({"threadId":thread})).await;
            let command = served.connect().await;
            Some(Remote { served, command, next: 0, stream })
        } else {
            None
        };
        let auth = (!over_socket).then(|| grok_support::attach_auth(&execution, &providers, &cwd));
        Self { status, store, execution, providers, clock, thread, next: 0, barriers, finishes: (finishes, 0), gate, remote, _auth: auth }
    }
    /// The projection once the actor has taken in every frame up to barrier `number`.
    async fn barrier(&self, number: u64) {
        self.until(|view| view["providerThreads"].as_array().unwrap().iter().any(|thread| thread["contextUsage"]["usedTokens"].as_u64().is_some_and(|used| used >= BARRIER_USAGE + number)).then_some(())).await
    }
    fn run_status(&self, ordinal: usize) -> Value {
        self.view()["runs"].get(ordinal - 1).map_or(Value::Null, |run| run["status"].clone())
    }
    /// `finish_held_run`: a run the adapter holds open for background work settles when its quiet window passes.
    /// The window is moved past only once the frames before it have been taken in (a barrier), and as often as
    /// late frames arm it again. A run that settles by itself (the agent's own wake turn ends it) needs no move:
    /// the barriers belong to the last steps, so the first of a scenario with fewer barriers than steps has none.
    async fn finish_held_run(&mut self, ordinal: usize, status: &str) {
        let (total, ran) = self.finishes;
        self.finishes.1 += 1;
        let done = |harness: &Self| harness.run_status(ordinal) == status;
        let mut events = self.store.subscribe();
        let by_itself = async {
            loop {
                if done(self) {
                    return;
                }
                let _ = events.recv().await;
            }
        };
        let needed = (self.barriers + ran + 1).saturating_sub(total) as u64;
        if needed == 0 {
            by_itself.await;
            return;
        }
        let reached = self.barrier(needed);
        tokio::select! {
            _ = by_itself => return,
            _ = reached => {}
        }
        // The last of the flood is in: the run that reports it may attach.
        if ran + 1 == total {
            self.providers.attach_gate().open();
        }
        let mut armed = self.providers.finish_debounce().armed();
        let mut consumed = 0;
        loop {
            if done(self) {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_secs(20), armed.wait_for(|count| *count > consumed)).await.expect("the held run never armed its quiet window");
            consumed = *armed.borrow();
            self.clock.advance(DEBOUNCE_MS);
            let mut events = self.store.subscribe();
            tokio::select! {
                _ = async { loop { if done(self) { break; } let _ = events.recv().await; } } => return,
                _ = armed.wait_for(|count| *count > consumed) => {}
            }
        }
    }
    /// `release_replay_gate`: the frame the recording held back is let through.
    async fn release_gate(&self) {
        let fifo = self.gate.clone().expect("the scenario names a gate");
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            std::fs::OpenOptions::new().write(true).open(fifo).unwrap().write_all(b"release").unwrap();
        })
        .await
        .unwrap();
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    async fn dispatch(&mut self, command: Value) {
        if let Some(remote) = &mut self.remote {
            remote.next += 1;
            let exit = provider_manage_support::rpc(&mut remote.command, remote.next, "orchestration.dispatchCommand", command).await;
            provider_manage_support::success(&exit);
            return;
        }
        self.execution.dispatch(&command, Utc::now()).unwrap();
    }
    async fn send(&mut self, text: &str, mode: Value) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.dispatch(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":mode})).await;
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn until_raw<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(found) = self.store.projection("thread", &self.thread).unwrap().and_then(|view| check(&view)) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .expect("timed out waiting for a persisted event")
    }
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        outcome.unwrap_or_else(|_| panic!("timed out waiting for a persisted event\nstatus: {}\nthreads: {}\nsessions: {}\nprojection: {}", std::fs::read_to_string(&self.status).unwrap_or_default(), self.view()["providerThreads"], self.view()["providerSessions"], normalize(&self.view())))
    }
    async fn settled(&self, ordinal: usize) -> Value {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await
    }
    async fn idle(&self) {
        self.until(|view| view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).then_some(())).await
    }
    async fn pending_request(&self) -> Value {
        self.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    fn replay_status(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.status).unwrap()).unwrap()
    }
}

fn pick(item: &Value, keys: &[&str]) -> serde_json::Map<String, Value> {
    keys.iter().filter_map(|key| item.get(*key).map(|value| ((*key).to_owned(), value.clone()))).collect()
}
fn normalize_item(item: &Value) -> Value {
    let by_type: &[&str] = match item["type"].as_str().unwrap() {
        "user_message" => &["text"],
        "assistant_message" | "reasoning" => &["text", "streaming"],
        "command_execution" => &["input", "output", "exitCode"],
        "file_change" => &["fileName", "diffStr"],
        "web_search" => &["patterns", "results"],
        "dynamic_tool" => &["toolName", "input", "output", "viewedImagePath", "toolSource"],
        "todo_list" => &["steps"],
        "proposed_plan" => &["markdown"],
        "approval_request" => &["requestKind", "prompt"],
        "user_input_request" => &["questions"],
        "run_interrupt_request" | "run_interrupt_result" | "system_notice" => &["message"],
        "subagent" => &["prompt", "result"],
        _ => &[],
    };
    let mut out = serde_json::Map::new();
    out.insert("type".into(), item["type"].clone());
    out.insert("status".into(), item["status"].clone());
    out.insert("title".into(), item["title"].clone());
    if let Some(kind) = item.get("toolNonExecutionKind") {
        out.insert("toolNonExecutionKind".into(), kind.clone());
    }
    out.extend(pick(item, by_type));
    if item["type"] == "error" {
        out.insert("failure".into(), Value::Object(pick(&item["failure"], &["class", "message", "code", "retryable"])));
    }
    Value::Object(out)
}
fn normalize(view: &Value) -> Value {
    let mut items: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] != "checkpoint").collect();
    items.sort_by_key(|item| item["ordinal"].as_u64());
    json!({
        "runs": view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(),
        "attempts": view["attempts"].as_array().unwrap().iter().map(|a| a["status"].clone()).collect::<Vec<_>>(),
        "providerTurns": view["providerTurns"].as_array().unwrap().iter().map(|turn| json!({"status":turn["status"],"turnTokenUsage":turn.get("turnTokenUsage").cloned().unwrap_or(Value::Null),"tokenUsage":turn.get("tokenUsage").filter(|u| !u.is_null()).map_or(Value::Null, |u| Value::Object(pick(u, &["usedTokens","maxTokens","inputTokens","cachedInputTokens","outputTokens","reasoningOutputTokens"])))})).collect::<Vec<_>>(),
        "items": items.into_iter().map(normalize_item).collect::<Vec<_>>(),
        "requests": view["runtimeRequests"].as_array().unwrap().iter().map(|r| json!({"kind":r["kind"],"status":r["status"]})).collect::<Vec<_>>(),
        "plans": view["plans"].as_array().unwrap().iter().map(|p| Value::Object(pick(p, &["kind","status","steps","markdown"]))).collect::<Vec<_>>(),
        "providerThread": view["providerThreads"].as_array().unwrap().iter().map(|t| t["status"].clone()).collect::<Vec<_>>(),
    })
}

/// Scenarios that are not replayed, each with the reason. All sixteen are.
const EXCLUDED: &[(&str, &str)] = &[];
/// Scenarios whose source snapshot predates the frames that follow the last reply. The original's adapter
/// republishes the provider thread as it last cached it (active) when such a title arrives, and its snapshot
/// for this scenario was taken before that: the thread the Rust adapter leaves after the same frames is active.
const SNAPSHOT_BEFORE_LATE_FRAMES: &[&str] = &["grok_prompt_error"];
/// The scenarios of the original's Grok replay registry.
const SCENARIOS: [&str; 16] = ["simple", "multi_turn", "queued_turn", "todo_list", "message_steering", "turn_interrupt", "tool_call_read_only", "tool_call_read_only_on_request", "plan_questions", "grok_prompt_error", "grok_auto_blocked_command", "grok_subagent_lineage", "grok_background_bash", "grok_background_bash_fast_wake", "grok_background_subagent", "grok_monitor"];

async fn replay(row: Value, over_socket: bool) -> Result<(), String> {
    let row = &row;
    let name = row["name"].as_str().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(scratch.path()).unwrap();
    // The recordings name the workspace the source harness used; so do its projections.
    let recorded = row["workspace"].as_str().unwrap_or("").to_owned();
    let rewrite = |value: &Value| serde_json::from_str::<Value>(&value.to_string().replace(&recorded, &workspace.to_string_lossy())).unwrap();
    let (transcript, row) = (rewrite(&row["transcript"]), rewrite(row));
    let mut harness = Harness::start(scratch.path(), &row, &transcript, over_socket).await;
    let steps = row["steps"].as_array().unwrap();
    let messages = steps.iter().filter(|step| step["type"] == "message" || step["type"] == "queue_message").count();
    let mut runs = 0;
    for (index, step) in steps.iter().enumerate() {
        let next = steps.get(index + 1).map(|step| step["type"].as_str().unwrap());
        match step["type"].as_str().unwrap() {
            "message" => {
                runs = harness.send(step["text"].as_str().unwrap(), json!({"type":"start_immediately"})).await;
                if !matches!(next, None | Some("interrupt" | "steer" | "queue_message" | "approve_next_runtime_request" | "answer_next_user_input_request" | "stop_background_work" | "finish_held_run" | "release_replay_gate")) {
                    harness.settled(runs).await;
                }
            }
            "queue_message" => {
                runs = harness.send(step["text"].as_str().unwrap(), json!({"type":"queue_after_active"})).await;
            }
            "steer" => {
                let target = harness.view()["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1]["id"].clone();
                // The recording waits for the cancel the steer causes, so the steer may follow the run's start.
                harness.until_raw(|view| view["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1].get("status").filter(|status| *status == "running").map(|_| ())).await;
                harness.send(step["text"].as_str().unwrap(), json!({"type":"steer_active","targetRunId":target})).await;
            }
            "approve_next_runtime_request" => {
                let request = harness.pending_request().await;
                harness.dispatch(json!({"type":"runtime-request.respond","commandId":format!("approve-{index}"),"threadId":harness.thread,"requestId":request["id"],"decision":step["decision"].as_str().unwrap_or("accept")})).await;
            }
            "answer_next_user_input_request" => {
                let request = harness.pending_request().await;
                harness.dispatch(json!({"type":"runtime-request.respond","commandId":format!("answer-{index}"),"threadId":harness.thread,"requestId":request["id"],"answers":step["answers"]})).await;
            }
            "interrupt" => {
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let run = harness.until(|view| view["runs"].get(target - 1).filter(|run| run["status"] == "running").cloned()).await;
                harness.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]})).await;
            }
            "stop_background_work" => {
                harness.idle().await;
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let run = harness.view()["runs"][target - 1]["id"].clone();
                harness.dispatch(json!({"type":"run.interrupt","commandId":"stop-background","threadId":harness.thread,"runId":run,"holdQueue":true})).await;
                // Stop ends the work in an effect; its result is what the projection shows.
                harness.execution.drained(&harness.thread).await;
            }
            "finish_held_run" => harness.finish_held_run(step["targetRunIndex"].as_u64().unwrap() as usize, step["status"].as_str().unwrap()).await,
            "release_replay_gate" => harness.release_gate().await,
            "advance_clock" => {}
            other => return Err(format!("{name}: unsupported step {other}")),
        }
    }
    let _ = messages;
    harness.idle().await;
    harness.settled(runs).await;
    // A title that arrives after the prompt's reply is applied once the run has settled: wait, on the store's
    // own events, for the thread status the source ended with. A scenario that never reaches it fails on the
    // comparison below, which names the difference.
    let mut expected_row = row["expected"].clone();
    if SNAPSHOT_BEFORE_LATE_FRAMES.contains(&name) {
        expected_row["providerThread"] = json!(["active"]);
    }
    let wanted = expected_row["providerThread"].clone();
    let mut events = harness.store.subscribe();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while normalize(&harness.view())["providerThread"] != wanted {
            let _ = events.recv().await;
        }
    })
    .await;
    let view = harness.view();
    let ordinals: Vec<String> = view["turnItems"].as_array().unwrap().iter().map(|item| format!("{}#{}@{}", item["type"].as_str().unwrap(), item["ordinal"], item["runId"].as_str().unwrap_or("-").len())).collect();
    let actual = normalize(&view);
    // What the thread's own stream delivered over its socket includes the run that reported the wake.
    if let Some(remote) = &mut harness.remote {
        remote.stream.until("the notification the wake run starts with", |chunks| chunks.iter().any(|chunk| chunk.to_string().contains("\"type\":\"notification\""))).await;
    }
    if let Some(remote) = harness.remote.take() {
        remote.served.stop().await;
    }
    // Shutting down closes the session, which the recording ends with.
    harness.providers.attach_gate().open();
    harness.execution.shutdown().await;
    let status = harness.replay_status();
    let consumed = status["failure"].is_null() && status["cursor"] == status["total"];
    let expected = &expected_row;
    let short = |value: &Value| value.to_string().chars().take(2500).collect::<String>();
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            if got.get(key) != want.get(key) {
                return Err(format!("{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}\n  ordinals: {ordinals:?}", want["type"], short(&got[key]), short(&want[key])));
            }
        }
    }
    if actual["items"].as_array().unwrap().len() != expected["items"].as_array().unwrap().len() {
        return Err(format!("{name}: item count\n  rust: {}\n  ts:   {}", short(&actual["items"]), short(&expected["items"])));
    }
    if actual != *expected {
        let keys: Vec<&String> = actual.as_object().unwrap().keys().filter(|key| actual[key.as_str()] != expected[key.as_str()]).collect();
        return Err(format!("{name}: projection differs at {keys:?}\n  rust: {}\n  ts:   {}", short(&Value::Object(keys.iter().map(|key| ((*key).clone(), actual[key.as_str()].clone())).collect())), short(&Value::Object(keys.iter().map(|key| ((*key).clone(), expected[key.as_str()].clone())).collect()))));
    }
    if !consumed {
        return Err(format!("{name}: the recording was not fully consumed: {status}"));
    }
    if let Some(threads) = row.get("threads") {
        assert_threads(name, &harness, threads.as_array().unwrap())?;
    }
    Ok(())
}
/// Every thread the scenario created, the root first: each thread's own projection, how it relates to its
/// parent, and the subagents it holds.
fn assert_threads(name: &str, harness: &Harness, expected: &[Value]) -> Result<(), String> {
    let mut found: Vec<Value> = vec![harness.view()];
    let mut index = 0;
    while index < found.len() {
        let children: Vec<String> = found[index]["subagents"].as_array().unwrap().iter().filter_map(|task| task["childThreadId"].as_str().map(str::to_owned)).collect();
        for child in children {
            if !found.iter().any(|view| view["thread"]["id"] == child.as_str()) {
                found.push(harness.store.projection("thread", &child).unwrap().ok_or_else(|| format!("{name}: child thread {child} is missing"))?);
            }
        }
        index += 1;
    }
    if found.len() != expected.len() {
        return Err(format!("{name}: the threads created: {:?}", found.iter().map(|view| view["thread"]["title"].clone()).collect::<Vec<_>>()));
    }
    let title_of = |id: &Value| found.iter().find(|view| view["thread"]["id"] == *id).map_or(Value::Null, |view| view["thread"]["title"].clone());
    for want in expected {
        let view = found.iter().find(|view| view["thread"]["title"] == want["title"]).ok_or_else(|| format!("{name}: no thread titled {}", want["title"]))?;
        let label = format!("{name}: thread {}", want["title"]);
        if title_of(&view["thread"]["lineage"]["parentThreadId"]) != want["parent"] || view["thread"]["lineage"]["relationshipToParent"] != want["relationship"] {
            return Err(format!("{label}: lineage"));
        }
        let subagents: Vec<Value> = view["subagents"].as_array().unwrap().iter().map(|task| {
            let mut row = serde_json::Map::new();
            for key in ["origin", "createdBy", "driver", "status", "prompt", "title", "model", "result"] {
                row.insert(key.to_owned(), task[key].clone());
            }
            row.insert("child".into(), title_of(&task["childThreadId"]));
            Value::Object(row)
        }).collect();
        // The projection lists subagents by start time, then id; here the rows are compared as a set.
        let by_child = |rows: &Value| {
            let mut rows = rows.as_array().unwrap().clone();
            rows.sort_by_key(|row| row["child"].as_str().unwrap_or_default().to_owned());
            Value::Array(rows)
        };
        if by_child(&Value::Array(subagents.clone())) != by_child(&want["subagents"]) {
            return Err(format!("{label}: subagents\n  rust: {}\n  ts:   {}", Value::Array(subagents), want["subagents"]));
        }
        let actual = normalize(view);
        if actual != want["expected"] {
            return Err(format!("{label}: items\n  rust: {}\n  ts:   {}", actual["items"], want["expected"]["items"]));
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_grok_sessions_persist_what_the_source_adapter_persisted() {
    let rows: Vec<Value> = SCENARIOS.iter().map(|name| serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()).collect();
    assert_eq!(rows.len(), 16);
    let mut failures = vec![];
    for row in &rows {
        let name = row["name"].as_str().unwrap();
        if EXCLUDED.iter().any(|(excluded, _)| *excluded == name) {
            continue;
        }
        if std::env::var("GROK_REPLAY_ONLY").is_ok_and(|only| !only.split(',').any(|wanted| wanted == name)) {
            continue;
        }
        // A scenario that stalls panics inside `until`; report it with the others.
        match tokio::spawn(replay(row.clone(), false)).await {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ:\n{}", failures.len(), rows.len(), failures.join("\n"));
}

/// The scenarios that hold a run open for background work are the ones whose steps release a held run, and no
/// other scenario has such a step. Each of them is replayed, none excluded.
#[test]
fn every_scenario_that_holds_a_run_open_for_background_work_is_replayed() {
    let mut holding = vec![];
    for name in SCENARIOS {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        if row["steps"].as_array().unwrap().iter().any(|step| matches!(step["type"].as_str(), Some("finish_held_run" | "release_replay_gate"))) {
            holding.push(name);
        }
    }
    holding.sort_unstable();
    assert!(EXCLUDED.is_empty(), "all sixteen recorded sessions are replayed");
    assert_eq!(holding, ["grok_background_bash", "grok_background_bash_fast_wake", "grok_background_subagent", "grok_monitor"]);
    // The late-frame allowance names one scenario whose recording really has a title after its last reply.
    for name in SNAPSHOT_BEFORE_LATE_FRAMES {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        let entries = row["transcript"].as_array().unwrap();
        let last_reply = entries.iter().rposition(|entry| entry["label"] == "response:session/prompt").unwrap();
        assert!(entries[last_reply..].iter().any(|entry| entry["frame"]["params"]["update"]["sessionUpdate"] == "session_info_update"));
    }
    assert_eq!(SNAPSHOT_BEFORE_LATE_FRAMES, ["grok_prompt_error"]);
}

/// The same recorded session, the commands going over the real WebSocket transport: a message starts the
/// run, the quiet window passes on the test's clock, and the subagent's end wakes a continuation run. The
/// thread's own stream is read on a socket of its own, every chunk acknowledged.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_subagent_and_the_run_its_end_wakes_over_the_real_socket() {
    let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/grok_background_subagent.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
    match tokio::spawn(replay(row, true)).await {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => panic!("{failure}"),
        Err(cause) => std::panic::resume_unwind(cause.into_panic()),
    }
}
