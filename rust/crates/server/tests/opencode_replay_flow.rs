#![cfg(unix)]
//! Replay parity for the OpenCode driver. The recordings are the OpenCode sessions the source adapters were recorded
//! against (`apps/server/src/orchestration-v2/testkit/fixtures/*/opencode_transcript.ndjson`, 1.x over the SDK
//! boundary and 2.x over HTTP and server-sent events); `fixtures/opencode-replay/<scenario>.json` holds the
//! user-visible projection the unchanged TypeScript orchestrator and adapters produced for each (see
//! `tools/generate_opencode_replay_fixtures.mjs`). The test starts `fixtures/opencode-fake-cli.py serve`, which plays
//! a recording over loopback HTTP: it compares every request the Rust client sends with the recorded one and fails the
//! replay on the first difference. No real `opencode` runs. Tests wait on persisted events, never on sleeps.
mod opencode_support;
use chrono::Utc;
use opencode_support::FakeServer;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_server::{
    execution::ExecutionService, launch::ThreadLaunchService, thread::ThreadService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry, scheduled_tasks::ManualClock,
};

/// How long a wait for a persisted state may take before the test calls it a hang: far beyond any loaded machine, and no
/// part of what a passing run depends on.
const HANG_BOUND: Duration = Duration::from_secs(120);

struct Harness {
    server: FakeServer,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    thread: String,
    /// The target thread of a fork.
    target: Option<String>,
    next: usize,
    workspace: std::path::PathBuf,
    _clock: Arc<ManualClock>,
}

/// The recording as the test plays it: the directory the source harness used is this test's workspace, and so is the
/// `<work>` a recording scrubbed a session's location to (the adapter compares it with where the thread runs). Other
/// `<work>` text, such as a tool's input, is what the source projected and stays.
fn materialize(row: &Value, workspace: &std::path::Path) -> Value {
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
    let recorded = row["workspace"].as_str().unwrap_or("");
    let text = if recorded.is_empty() { row["transcript"].to_string() } else { row["transcript"].to_string().replace(recorded, &workspace.to_string_lossy()) };
    let mut transcript: Value = serde_json::from_str(&text).unwrap();
    locate(&mut transcript, &workspace.to_string_lossy());
    transcript
}

impl Harness {
    async fn start(root: &std::path::Path, row: &Value) -> Self {
        let cwd = std::fs::canonicalize(root).unwrap();
        let name = row["name"].as_str().unwrap();
        let workspace = cwd.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        // The fixtures' workspace is a one-file repository, so checkpoints can be taken.
        let git = |args: &[&str]| assert!(std::process::Command::new("git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false"]).args(args).current_dir(&workspace).env("HOME", &cwd).env("GIT_CONFIG_NOSYSTEM", "1").status().unwrap().success());
        git(&["init", "-q"]);
        std::fs::write(workspace.join("hello.txt"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "initial"]);
        // What the source harness did before the stream moved on is done here before the first event after the prompt's
        // answer is let through: a steer or a queued message is dispatched while the first tool call is not yet seen.
        let steps = row["steps"].as_array().unwrap();
        let gate = if steps.iter().any(|step| step["type"] == "steer") {
            Some("session.inbox.enqueued")
        } else if steps.iter().any(|step| step["waitForTurnItemType"] == "command_execution") {
            // The Stop is dispatched when the command appears, before what the model said after it is taken in.
            Some("session.tool.input.ended")
        } else {
            None
        };
        let server = FakeServer::start_gated(&cwd, name, row["protocol"].as_str().unwrap(), row["version"].as_str().unwrap(), &materialize(row, &workspace), Some("replay"), gate);
        let home = cwd.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // The server is the user's own (`serverUrl`), so the binary is never run; its path is absolute and does not exist.
        // The source's replay harness persists a block when it ends, not while it streams.
        let settings = serde_json::from_value(json!({"responseStreamingMode":"turn","providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "opencode":{"driver":"opencode","enabled":true,"config":{"enabled":true,"binaryPath":cwd.join("bin-opencode-unused"),"serverUrl":server.url,"serverPassword":"replay"},"environment":[
                {"name":"HOME","value":home},{"name":"XDG_DATA_HOME","value":home.join("share")},{"name":"OPENCODE_API_KEY","value":""}]}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &workspace).await.unwrap();
        let clock = ManualClock::new(Utc::now().timestamp_millis());
        providers.idle_release().configure(clock.clone(), 30 * 60 * 1000, 4 * 60 * 60 * 1000);
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":workspace}), Utc::now()).unwrap();
        // The recordings that expect rules naming the original's thread get a thread of that name.
        let thread_id = format!("thread:{}", if name == "opencode2_fork" { "opencode2-fork" } else { name });
        let selection = row["modelSelection"].clone();
        let runtime_mode = if row["approvalPolicy"] == "on-request" { "approval-required" } else { row["runtimeMode"].as_str().unwrap() };
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","threadId":thread_id,"title":format!("Replay fixture: {name}"),"modelSelection":selection,"runtimeMode":runtime_mode,"interactionMode":row["interactionMode"],"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        Self { server, store, execution, providers, thread, target: None, next: 0, workspace, _clock: clock }
    }
    fn view_of(&self, thread: &str) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", thread).unwrap().unwrap())
    }
    fn view(&self) -> Value {
        self.view_of(&self.thread)
    }
    fn dispatch(&self, command: Value) {
        self.execution.dispatch(&command, Utc::now()).unwrap();
    }
    fn send_to(&mut self, thread: &str, text: &str, mode: Value, selection: Option<&Value>) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":mode});
        if let Some(selection) = selection {
            command["modelSelection"] = selection.clone();
        }
        self.dispatch(command);
        self.view_of(thread)["runs"].as_array().unwrap().len()
    }
    async fn until_in<T>(&self, thread: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        let outcome = tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(found) = check(&self.view_of(thread)) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        outcome.unwrap_or_else(|_| panic!("timed out waiting for a persisted event\nreplay: {}\nprojection: {}", self.server.replay_status(), normalize(&self.view_of(thread))))
    }
    async fn until<T>(&self, check: impl FnMut(&Value) -> Option<T>) -> T {
        self.until_in(&self.thread.clone(), check).await
    }
    async fn settled_in(&self, thread: &str, ordinal: usize) -> Value {
        self.until_in(thread, |view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"))).cloned()).await
    }
    async fn idle_in(&self, thread: &str) {
        self.until_in(thread, |view| view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"))).then_some(())).await
    }
    /// Waits until `check` holds of what is persisted, looking again after every persisted event. The bound only ends a
    /// hang; what the wait is for is the state, not the time it takes.
    async fn until_persisted(&self, mut check: impl FnMut() -> bool) {
        let mut events = self.store.subscribe();
        let outcome = tokio::time::timeout(HANG_BOUND, async {
            while !check() {
                let _ = events.recv().await;
            }
        })
        .await;
        // On a timeout the caller compares what is there and reports how it differs.
        let _ = outcome;
    }
    async fn pending_request(&self) -> Value {
        self.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
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
        "file_change" => &["fileName", "oldStr", "newStr", "diffStr"],
        "file_search" => &["pattern", "results"],
        "web_search" => &["patterns", "results"],
        "dynamic_tool" => &["toolName", "input", "output", "viewedImagePath", "toolSource"],
        "todo_list" => &["steps"],
        "proposed_plan" => &["markdown"],
        "approval_request" => &["requestKind", "prompt", "options"],
        "user_input_request" => &["questions"],
        "run_interrupt_request" | "run_interrupt_result" | "system_notice" => &["message"],
        "compaction" => &["summary"],
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

/// Scenarios that are not replayed, each with the reason. The six recorded against 1.x are replayed through the 1.x
/// adapter since it was ported, so nothing is left out.
const EXCLUDED: &[(&str, &str)] = &[];
/// The scenarios of the original's OpenCode replay registry: six recorded against 1.x, sixteen against 2.x.
const SCENARIOS: [&str; 22] = [
    "simple", "turn_interrupt", "plan_questions", "opencode_subagent", "opencode_child_approval", "opencode_running_child_approval",
    "opencode2_simple", "opencode2_tool_call", "opencode2_interrupt", "opencode2_inbox", "opencode2_revert", "opencode2_permission", "opencode2_question", "opencode2_subagent",
    "opencode2_background", "opencode2_nested_background", "opencode2_compaction", "opencode2_resume_after_restart", "opencode2_command", "opencode2_skill", "opencode2_switch", "opencode2_fork",
];

fn row_of(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/opencode-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
}

async fn replay(row: Value) -> Result<(), String> {
    let name = row["name"].as_str().unwrap().to_owned();
    let scratch = tempfile::tempdir().unwrap();
    let mut harness = Harness::start(scratch.path(), &row).await;
    let steps = row["steps"].as_array().unwrap().clone();
    let mut runs = 0;
    let mut target_runs = 0;
    for (index, step) in steps.iter().enumerate() {
        let next = steps.get(index + 1).map(|step| step["type"].as_str().unwrap());
        match step["type"].as_str().unwrap() {
            "message" => {
                let selection = step.get("modelSelection");
                if step["thread"] == "target" {
                    let target = harness.target.clone().expect("the fork made its target");
                    target_runs = harness.send_to(&target, step["text"].as_str().unwrap(), json!({"type":"start_immediately"}), selection);
                    harness.settled_in(&target, target_runs).await;
                    continue;
                }
                let thread = harness.thread.clone();
                runs = harness.send_to(&thread, step["text"].as_str().unwrap(), json!({"type":"start_immediately"}), selection);
                // A turn that something else drives next (a Stop, a steer, a queued message, an answer, a wait for a later
                // run) is not waited for.
                let drives_next = matches!(next, Some("interrupt" | "steer" | "queue_message" | "approve_next_runtime_request" | "answer_next_user_input_request")) || (next == Some("await_run_status") && steps[index + 1]["targetRunIndex"].as_u64().unwrap() as usize > runs);
                if !drives_next {
                    harness.settled_in(&thread, runs).await;
                }
            }
            "queue_message" => {
                let thread = harness.thread.clone();
                runs = harness.send_to(&thread, step["text"].as_str().unwrap(), json!({"type":"queue_after_active"}), None);
                if !matches!(next, Some("queue_message" | "cancel_queued_run")) {
                    // The queue drains once the active run ends.
                    harness.idle_in(&thread).await;
                }
            }
            "cancel_queued_run" => {
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let queued = harness.until(|view| view["runs"].get(target - 1).filter(|run| run["status"] == "queued").cloned()).await;
                harness.dispatch(json!({"type":"queued-run.cancel","commandId":format!("cancel-{target}"),"threadId":harness.thread,"runId":queued["id"]}));
            }
            "steer" => {
                let target_index = step["targetRunIndex"].as_u64().unwrap() as usize;
                let target = harness.view()["runs"][target_index - 1]["id"].clone();
                harness.until(|view| view["runs"][target_index - 1].get("status").filter(|status| *status == "running").map(|_| ())).await;
                let thread = harness.thread.clone();
                harness.send_to(&thread, step["text"].as_str().unwrap(), json!({"type":"steer_active","targetRunId":target}), None);
                // Everything the steps did is in; the recorded events may now arrive.
                harness.server.release().await;
            }
            "approve_next_runtime_request" => {
                let request = harness.pending_request().await;
                harness.dispatch(json!({"type":"runtime-request.respond","commandId":format!("approve-{index}"),"threadId":harness.thread,"requestId":request["id"],"decision":step["decision"].as_str().unwrap_or("accept")}));
            }
            "answer_next_user_input_request" => {
                let request = harness.pending_request().await;
                harness.dispatch(json!({"type":"runtime-request.respond","commandId":format!("answer-{index}"),"threadId":harness.thread,"requestId":request["id"],"answers":step["answers"]}));
            }
            "interrupt" => {
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let item_type = step["waitForTurnItemType"].as_str().map(str::to_owned);
                let run = harness.until(|view| view["runs"].get(target - 1).filter(|run| run["status"] == "running" && item_type.as_deref().is_none_or(|wanted| view["turnItems"].as_array().unwrap().iter().any(|item| item["runId"] == run["id"] && item["type"] == wanted))).cloned()).await;
                harness.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]}));
                if item_type.is_some() {
                    harness.server.release().await;
                }
            }
            "await_run_status" => {
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let status = step["status"].as_str().unwrap().to_owned();
                harness.until(|view| view["runs"].get(target - 1).filter(|run| run["status"] == status.as_str()).map(|_| ())).await;
            }
            "rollback" => {
                harness.idle_in(&harness.thread.clone()).await;
                let view = harness.view();
                let scope = view["checkpointScopes"][0]["id"].clone();
                let checkpoint = view["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["scopeId"] == scope && checkpoint["ordinalWithinScope"] == 1).expect("the first run's checkpoint").clone();
                harness.dispatch(json!({"type":"checkpoint.rollback","commandId":"rollback-1","threadId":harness.thread,"scopeId":scope,"checkpointId":checkpoint["id"],"restoreFiles":false}));
            }
            "mode" => {
                ThreadService::new(harness.store.clone()).dispatch(&json!({"type":"thread.interaction-mode.set","commandId":format!("mode-{index}"),"threadId":harness.thread,"interactionMode":step["mode"]}), Utc::now()).unwrap();
            }
            "fork" => {
                let source = harness.view()["runs"][step["sourceRunIndex"].as_u64().unwrap() as usize - 1]["id"].clone();
                let target = format!("{}:target", harness.thread);
                ThreadService::new(harness.store.clone()).dispatch(&json!({"type":"thread.fork","commandId":"fork","createdBy":"user","creationSource":"web","sourceThreadId":harness.thread,"targetThreadId":target,"sourcePoint":{"type":"run","runId":source},"title":"Fork"}), Utc::now()).unwrap();
                harness.target = Some(target);
            }
            other => return Err(format!("{name}: unsupported step {other}")),
        }
    }
    let _ = (runs, target_runs);
    let thread = harness.thread.clone();
    harness.idle_in(&thread).await;
    // Shutting down releases the session, which stops what the recording still held.
    let view = harness.view();
    let mut actual = normalize(&view);
    // A todo step is named after the provider turn, which this port numbers itself; the synthetic sessions compare the rest.
    if name.starts_with("one_x_") {
        let mask = |steps: &mut Value| steps.as_array_mut().into_iter().flatten().enumerate().for_each(|(index, step)| step["id"] = json!(format!("<id>:{}", index + 1)));
        actual["items"].as_array_mut().unwrap().iter_mut().filter(|item| item.get("steps").is_some()).for_each(|item| mask(&mut item["steps"]));
        actual["plans"].as_array_mut().unwrap().iter_mut().for_each(|plan| mask(&mut plan["steps"]));
    }
    let forked = harness.target.clone().map(|target| harness.view_of(&target));
    // The root's run ends when the root's turn does; what a subagent was left running (a background one reports back
    // after it) is persisted afterwards, and shutting down before it is would stop it mid-turn. The tree is compared again
    // below, once shut down.
    if let Some(threads) = row.get("threads") {
        harness.until_persisted(|| assert_threads(&name, &harness, threads.as_array().unwrap()).is_ok()).await;
    }
    harness.execution.shutdown().await;
    let status = harness.server.replay_status();
    let consumed = status["failure"].is_null() && status["settled"] == true;
    let expected = &row["expected"];
    if std::env::var("OPENCODE_REPLAY_DUMP").is_ok() {
        eprintln!("{name}: {actual}\n{status}");
    }
    let short = |value: &Value| value.to_string().chars().take(2500).collect::<String>();
    if expected.is_null() {
        return Err(format!("{name}: no expectation yet; the session persisted {actual}\n  replay: {status}"));
    }
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            if got.get(key) != want.get(key) {
                return Err(format!("{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}\n  replay: {status}", want["type"], short(&got[key]), short(&want[key])));
            }
        }
    }
    if actual["items"].as_array().unwrap().len() != expected["items"].as_array().unwrap().len() {
        return Err(format!("{name}: item count\n  rust: {}\n  ts:   {}\n  replay: {status}", short(&actual["items"]), short(&expected["items"])));
    }
    if actual != *expected {
        let keys: Vec<&String> = actual.as_object().unwrap().keys().filter(|key| actual[key.as_str()] != expected[key.as_str()]).collect();
        return Err(format!("{name}: projection differs at {keys:?}\n  rust: {}\n  ts:   {}\n  replay: {status}", short(&Value::Object(keys.iter().map(|key| ((*key).clone(), actual[key.as_str()].clone())).collect())), short(&Value::Object(keys.iter().map(|key| ((*key).clone(), expected[key.as_str()].clone())).collect()))));
    }
    if let (Some(forked), Some(want)) = (forked, row.get("forked")) {
        let got = normalize(&forked);
        // The original mints a second provider thread for the fork and leaves the one the thread was made with idle
        // forever (its own source says an adapter that does so leaves two live rows per thread); the port gives the fork's
        // session to the row the thread has, so the fork has one.
        let mut wanted = want["expected"].clone();
        let orphan = wanted["providerThread"].as_array().cloned().unwrap_or_default();
        assert_eq!(orphan, [json!("idle"), json!("idle")], "the original's fork keeps two provider threads");
        wanted["providerThread"] = json!(["idle"]);
        if got != wanted {
            return Err(format!("{name}: the forked thread differs\n  rust: {}\n  ts:   {}", short(&got), short(&want["expected"])));
        }
    }
    if !consumed {
        return Err(format!("{name}: the recording was not fully consumed: {status}"));
    }
    if let Some(threads) = row.get("threads") {
        assert_threads(&name, &harness, threads.as_array().unwrap())?;
    }
    let _ = &harness.providers;
    let _ = &harness.workspace;
    Ok(())
}
/// Every thread the scenario created, the root first: each thread's own projection, how it relates to its parent, and the
/// subagents it holds.
fn assert_threads(name: &str, harness: &Harness, expected: &[Value]) -> Result<(), String> {
    let mut found: Vec<Value> = vec![harness.view()];
    let mut index = 0;
    while index < found.len() {
        let children: Vec<String> = found[index]["subagents"].as_array().unwrap().iter().filter_map(|task| task["childThreadId"].as_str().map(str::to_owned)).collect();
        for child in children {
            if !found.iter().any(|view| view["thread"]["id"] == child.as_str()) {
                found.push(harness.view_of(&child));
            }
        }
        index += 1;
    }
    if found.len() != expected.len() {
        return Err(format!("{name}: the threads created: {:?}", found.iter().map(|view| view["thread"]["title"].clone()).collect::<Vec<_>>()));
    }
    let title_of = |id: &Value| found.iter().find(|view| view["thread"]["id"] == *id).map_or(Value::Null, |view| view["thread"]["title"].clone());
    let short = |value: &Value| value.to_string().chars().take(2500).collect::<String>();
    for want in expected {
        let view = found.iter().find(|view| view["thread"]["title"] == want["title"]).ok_or_else(|| format!("{name}: no thread titled {}", want["title"]))?;
        let label = format!("{name}: thread {}", want["title"]);
        if title_of(&view["thread"]["lineage"]["parentThreadId"]) != want["parent"] || view["thread"]["lineage"]["relationshipToParent"] != want["relationship"] {
            return Err(format!("{label}: lineage"));
        }
        let subagents: Vec<Value> = view["subagents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| {
                let mut row = serde_json::Map::new();
                for key in ["origin", "createdBy", "driver", "status", "prompt", "title", "model", "result"] {
                    row.insert(key.to_owned(), task[key].clone());
                }
                row.insert("child".into(), title_of(&task["childThreadId"]));
                Value::Object(row)
            })
            .collect();
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
            return Err(format!("{label}: differs\n  rust: {}\n  ts:   {}", short(&actual), short(&want["expected"])));
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_opencode_sessions_persist_what_the_source_adapters_persisted() {
    let mut failures = vec![];
    for name in SCENARIOS {
        if EXCLUDED.iter().any(|(excluded, _)| *excluded == name) {
            continue;
        }
        if std::env::var("OPENCODE_REPLAY_ONLY").is_ok_and(|only| !only.split(',').any(|wanted| wanted == name)) {
            continue;
        }
        // A scenario that stalls panics inside `until`; report it with the others.
        match tokio::time::timeout(HANG_BOUND * 2, tokio::spawn(replay(row_of(name)))).await.unwrap_or_else(|_| panic!("{name} did not finish within the hang bound")) {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ:\n{}", failures.len(), SCENARIOS.len(), failures.join("\n"));
}

/// Every excluded scenario is a recorded one, with a reason; nothing else is left out. Every recording is replayed: the
/// six against 1.x through the 1.x adapter and the sixteen against 2.x through the 2.x one.
#[test]
fn the_exclusions_name_recorded_scenarios_with_reasons() {
    for (name, reason) in EXCLUDED {
        assert!(SCENARIOS.contains(name), "{name} is not a recorded scenario");
        assert!(!reason.is_empty());
    }
    assert!(EXCLUDED.is_empty(), "every recording is replayed");
    let one_x = SCENARIOS.iter().filter(|name| row_of(name)["protocol"] == "opencode-sdk.sse").count();
    assert_eq!((one_x, SCENARIOS.len() - one_x), (6, 16));
    for name in SCENARIOS {
        let row = row_of(name);
        assert_eq!(row["name"], name);
        assert!(row["transcript"].as_array().is_some_and(|entries| !entries.is_empty()));
    }
}

// ---- 1.x sessions the recordings do not hold ----
//
// The same replay, over transcripts written here: what the 1.x adapter does with a steer, a compaction, a lost stream, an
// error the session reports, a todo list and a rollback. They are not the original's recordings, so what each persists
// is what the adapter is meant to persist (the original's adapter does the same for each of these, in the same words),
// reviewed by hand rather than compared with an unchanged run.
mod synthetic {
    use serde_json::{Value, json};

    pub const SESSION: &str = "ses_synthetic0001";
    pub fn out(label: &str, kind: &str, input: Value) -> Value {
        json!({"type":"expect_outbound","label":label,"frame":{"type":kind,"input":input}})
    }
    pub fn resp(label: &str, operation: &str, data: Value) -> Value {
        json!({"type":"emit_inbound","label":label,"frame":{"type":"sdk.response","operation":operation,"data":data}})
    }
    pub fn ev(label: &str, kind: &str, properties: Value) -> Value {
        json!({"type":"emit_inbound","label":label,"frame":{"type":"sdk.event","event":{"id":format!("evt_{label}"),"type":kind,"properties":properties}}})
    }
    pub fn opening(session: &str) -> Vec<Value> {
        vec![
            json!({"type":"expect_outbound","label":"event.subscribe","frame":{"type":"event.subscribe"}}),
            out("session.create", "session.create", json!({"title":"<any>","permission":"<any>"})),
            resp("session.created", "session.create", json!({"id":session,"slug":"synthetic","version":"1.14.39","projectID":"global","directory":"<work>","title":"Synthetic","time":{"created":1,"updated":1}})),
        ]
    }
    pub fn prompt(session: &str, id: &str, text: &str) -> Value {
        out("session.promptAsync", "session.promptAsync", json!({"sessionID":session,"model":{"providerID":"openai","modelID":"gpt-5.4-mini"},"agent":"build","parts":[{"type":"text","text":text}],"messageID":id}))
    }
    pub fn accepted() -> Value {
        resp("session.promptAsync.response", "session.promptAsync", Value::Null)
    }
    pub fn user(session: &str, id: &str) -> Value {
        ev("message.updated.user", "message.updated", json!({"sessionID":session,"info":{"role":"user","id":id,"sessionID":session,"time":{"created":2}}}))
    }
    pub fn assistant(session: &str, id: &str, parent: &str, completed: bool) -> Value {
        let mut time = json!({"created":3});
        if completed {
            time["completed"] = json!(4);
        }
        ev(if completed { "message.updated.assistant.completed" } else { "message.updated.assistant" }, "message.updated", json!({"sessionID":session,"info":{"role":"assistant","id":id,"parentID":parent,"sessionID":session,"time":time,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}}))
    }
    pub fn text(session: &str, message: &str, part: &str, body: &str) -> Value {
        ev("assistant.completed", "message.part.updated", json!({"sessionID":session,"part":{"id":part,"messageID":message,"sessionID":session,"type":"text","text":body,"time":{"start":3,"end":4}}}))
    }
    pub fn text_of(session: &str, message: &str, part: &str, body: &str) -> Value {
        text(session, message, part, body)
    }
    pub fn status(session: &str, state: &str) -> Value {
        ev(&format!("session.status.{state}"), "session.status", json!({"sessionID":session,"status":{"type":state}}))
    }
    /// What the source did to a server it did not start when the session closed.
    pub fn closing(session: &str) -> Vec<Value> {
        vec![
            out("session.abort.cleanup", "session.abort", json!({"sessionID":session})),
            resp("session.abort.cleanup.response", "session.abort", json!(true)),
            out("session.children.cleanup", "session.children", json!({"sessionID":session})),
            resp("session.children.cleanup.response", "session.children", json!([])),
            json!({"type":"runtime_exit","status":"success"}),
        ]
    }
    pub fn row(name: &str, steps: Value, transcript: Vec<Value>, expected: Value) -> Value {
        json!({"name":name,"protocol":"opencode-sdk.sse","version":"1.14.39","approvalPolicy":null,"sandboxPolicy":null,"runtimeMode":"full-access","interactionMode":"default","workspace":"","model":"openai/gpt-5.4-mini","modelSelection":{"instanceId":"opencode","model":"openai/gpt-5.4-mini","options":[{"id":"agent","value":"build"}]},"steps":steps,"transcript":transcript,"expected":expected})
    }
    /// What one run of a thread persisted: its status, the provider turn's usage (these sessions report none), the user-visible
    /// items in order and the provider thread's state.
    pub fn expected(status: &str, items: Value, plans: Value, thread: &str) -> Value {
        json!({"runs":[status],"attempts":[status],"providerTurns":[{"status":status,"turnTokenUsage":{"usageScope":"main_agent","hasSubagents":false,"usageStatus":"unavailable"},"tokenUsage":null}],"items":items,"requests":[],"plans":plans,"providerThread":[thread]})
    }
    pub fn user_item(text: &str) -> Value {
        json!({"type":"user_message","status":"completed","title":null,"text":text})
    }
    pub fn assistant_item(text: &str) -> Value {
        json!({"type":"assistant_message","status":"completed","title":null,"text":text,"streaming":false})
    }
    pub fn error_item(class: &str, message: &str, code: Value) -> Value {
        json!({"type":"error","status":"failed","title":"Provider error","failure":{"class":class,"message":message,"code":code,"retryable":null}})
    }
}

fn synthetic_rows() -> Vec<Value> {
    use synthetic::*;
    let mut rows = vec![];
    let step = |text: &str| json!({"type":"message","text":text});

    // A steer is another prompt into the running turn; the turn ends with the execution that answers it.
    let mut entries = opening(SESSION);
    entries.extend([
        prompt(SESSION, "msg_p1", "First"),
        accepted(),
        user(SESSION, "msg_p1"),
        status(SESSION, "busy"),
        ev("session.inbox.enqueued", "message.part.updated", json!({"sessionID":SESSION,"part":{"id":"prt_step","messageID":"msg_a1","sessionID":SESSION,"type":"step-start"}})),
        out("session.promptAsync.steer", "session.promptAsync", json!({"sessionID":SESSION,"model":{"providerID":"openai","modelID":"gpt-5.4-mini"},"parts":[{"type":"text","text":"Also this"}],"messageID":"msg_p2"})),
        accepted(),
        user(SESSION, "msg_p2"),
        assistant(SESSION, "msg_a1", "msg_p2", false),
        text(SESSION, "msg_a1", "prt_text", "Both done"),
        assistant(SESSION, "msg_a1", "msg_p2", true),
        status(SESSION, "idle"),
    ]);
    entries.extend(closing(SESSION));
    rows.push(row("one_x_steer", json!([step("First"), {"type":"steer","targetRunIndex":1,"text":"Also this"}]), entries, expected("completed", json!([user_item("First"), user_item("Also this"), assistant_item("Both done")]), json!([]), "idle")));

    // `/compact` is a summarize request that runs beside the stream and leaves a compaction item.
    let mut entries = opening(SESSION);
    entries.extend([
        out("session.summarize", "session.summarize", json!({"sessionID":SESSION,"providerID":"openai","modelID":"gpt-5.4-mini","auto":false})),
        status(SESSION, "busy"),
        ev("session.compacted", "session.compacted", json!({"sessionID":SESSION})),
        status(SESSION, "idle"),
        resp("session.summarize.response", "session.summarize", json!(true)),
    ]);
    entries.extend(closing(SESSION));
    rows.push(row("one_x_compaction", json!([step("/compact")]), entries, expected("completed", json!([user_item("/compact"), {"type":"compaction","status":"completed","title":"Context compacted"}]), json!([]), "idle")));

    // The stream is the only signal an execution ended; when it breaks the turn fails as a transport error and the
    // thread is broken.
    let mut entries = opening(SESSION);
    entries.extend([prompt(SESSION, "msg_p1", "Hello"), accepted(), user(SESSION, "msg_p1"), status(SESSION, "busy"), json!({"type":"runtime_exit","status":"success"})]);
    rows.push(row("one_x_stream_lost", json!([step("Hello")]), entries, expected("failed", json!([user_item("Hello"), error_item("transport_error", "OpenCode event stream ended unexpectedly.", Value::Null)]), json!([]), "error")));

    // An error the session reports fails the turn with its message and name, and the thread stays usable.
    let mut entries = opening(SESSION);
    entries.extend([
        prompt(SESSION, "msg_p1", "Hello"),
        accepted(),
        user(SESSION, "msg_p1"),
        status(SESSION, "busy"),
        ev("session.error", "session.error", json!({"sessionID":SESSION,"error":{"name":"APIError","data":{"message":"quota exceeded"}}})),
        status(SESSION, "idle"),
    ]);
    entries.extend(closing(SESSION));
    // The original reports the thread reusable after such an error; this port's shared run terminal leaves a failed run's
    // provider thread in `error` until the next turn reopens it.
    rows.push(row("one_x_session_error", json!([step("Hello")]), entries, expected("failed", json!([user_item("Hello"), error_item("provider_error", "quota exceeded", json!("APIError"))]), json!([]), "error")));

    // The agent's todo list is a plan, rewritten whole by every update.
    let mut entries = opening(SESSION);
    entries.extend([
        prompt(SESSION, "msg_p1", "Plan it"),
        accepted(),
        user(SESSION, "msg_p1"),
        status(SESSION, "busy"),
        ev("todo.updated", "todo.updated", json!({"sessionID":SESSION,"todos":[{"content":"Write tests","status":"in_progress","priority":"high"},{"content":"Ship","status":"pending","priority":"low"}]})),
        assistant(SESSION, "msg_a1", "msg_p1", false),
        text(SESSION, "msg_a1", "prt_text", "Planned"),
        assistant(SESSION, "msg_a1", "msg_p1", true),
        status(SESSION, "idle"),
    ]);
    entries.extend(closing(SESSION));
    // The turn ends with the list as the agent last wrote it; the item is cancelled with the rest of the turn's open work.
    let steps = json!([{"id":"<id>:1","text":"Write tests","status":"running"},{"id":"<id>:2","text":"Ship","status":"pending"}]);
    rows.push(row("one_x_todo", json!([step("Plan it")]), entries, expected("completed", json!([user_item("Plan it"), {"type":"todo_list","status":"cancelled","title":"Todo list","steps":steps}, assistant_item("Planned")]), json!([{"kind":"todo_list","status":"active","steps":steps}]), "idle")));

    // `/name args` naming one of the workspace's commands is OpenCode's own command, which waits for the generation and so runs
    // beside the stream: the turn is admitted by its user message, not by the request's end.
    let mut entries = opening(SESSION);
    entries.extend([
        out("command.list", "command.list", json!({})),
        resp("command.list.response", "command.list", json!([{"name":"init","description":"create AGENTS.md","source":"command","template":"","hints":["$ARGUMENTS"]}])),
        out("session.command", "session.command", json!({"sessionID":SESSION,"messageID":"msg_p1","command":"init","arguments":"the repo","model":"openai/gpt-5.4-mini","agent":"build","parts":[]})),
        user(SESSION, "msg_p1"),
        status(SESSION, "busy"),
        assistant(SESSION, "msg_a1", "msg_p1", false),
        text(SESSION, "msg_a1", "prt_text", "Initialised"),
        assistant(SESSION, "msg_a1", "msg_p1", true),
        status(SESSION, "idle"),
        resp("session.command.response", "session.command", Value::Null),
    ]);
    entries.extend(closing(SESSION));
    rows.push(row("one_x_command", json!([step("/init the repo")]), entries, expected("completed", json!([user_item("/init the repo"), assistant_item("Initialised")]), json!([]), "idle")));

    // A rollback forks the session before the first message of the turn after the one kept, and the thread goes on in the fork.
    let fork = "ses_fork0000001";
    let turn = |index: u32, session: &str, text: &str, body: &str| {
        let (prompt_id, assistant_id) = (format!("msg_p{index}"), format!("msg_a{index}"));
        vec![
            prompt(session, &prompt_id, text),
            accepted(),
            user(session, &prompt_id),
            status(session, "busy"),
            assistant(session, &assistant_id, &prompt_id, false),
            text_of(session, &assistant_id, &format!("prt_{index}"), body),
            assistant(session, &assistant_id, &prompt_id, true),
            status(session, "idle"),
        ]
    };
    let history = |ids: &[(&str, &str)], session: &str| {
        json!(ids
            .iter()
            .map(|(id, role)| {
                let info = if *role == "user" {
                    json!({"id":id,"role":"user","sessionID":session,"time":{"created":2},"agent":"build","model":{"providerID":"openai","modelID":"gpt-5.4-mini"}})
                } else {
                    json!({"id":id,"role":"assistant","sessionID":session,"time":{"created":3,"completed":4},"parentID":"msg_p0","modelID":"gpt-5.4-mini","providerID":"openai","mode":"build","agent":"build","path":{"cwd":"/","root":"/"},"cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}})
                };
                json!({"info":info,"parts":[]})
            })
            .collect::<Vec<_>>())
    };
    let session_info = |id: &str| json!({"id":id,"slug":"fork","version":"1.14.39","projectID":"global","directory":"<work>","title":"Fork","time":{"created":5,"updated":5}});
    let mut entries = opening(SESSION);
    entries.extend(turn(1, SESSION, "First", "One"));
    entries.extend(turn(2, SESSION, "Second", "Two"));
    entries.extend([
        out("session.messages", "session.messages", json!({"sessionID":SESSION})),
        resp("session.messages.response", "session.messages", history(&[("msg_p1", "user"), ("msg_a1", "assistant"), ("msg_p2", "user"), ("msg_a2", "assistant")], SESSION)),
        out("session.fork", "session.fork", json!({"sessionID":SESSION,"messageID":"msg_p2"})),
        resp("session.fork.response", "session.fork", session_info(fork)),
        out("session.messages.fork", "session.messages", json!({"sessionID":fork})),
        resp("session.messages.fork.response", "session.messages", history(&[("msg_f1", "user"), ("msg_f2", "assistant")], fork)),
        out("session.update", "session.update", json!({"sessionID":fork,"permission":"<any>"})),
        resp("session.update.response", "session.update", session_info(fork)),
        out("session.get", "session.get", json!({"sessionID":fork})),
        resp("session.get.response", "session.get", session_info(fork)),
        out("session.messages.snapshot", "session.messages", json!({"sessionID":fork})),
        resp("session.messages.snapshot.response", "session.messages", history(&[("msg_f1", "user"), ("msg_f2", "assistant")], fork)),
    ]);
    entries.extend(turn(3, fork, "Third", "Three"));
    entries.extend([
        out("session.abort.cleanup", "session.abort", json!({"sessionID":SESSION})),
        resp("session.abort.cleanup.response", "session.abort", json!(true)),
        out("session.children.cleanup", "session.children", json!({"sessionID":SESSION})),
        resp("session.children.cleanup.response", "session.children", json!([])),
        out("session.abort.cleanup.fork", "session.abort", json!({"sessionID":fork})),
        resp("session.abort.cleanup.fork.response", "session.abort", json!(true)),
        out("session.children.cleanup.fork", "session.children", json!({"sessionID":fork})),
        resp("session.children.cleanup.fork.response", "session.children", json!([])),
        json!({"type":"runtime_exit","status":"success"}),
    ]);
    let turn_row = json!({"status":"completed","turnTokenUsage":{"usageScope":"main_agent","hasSubagents":false,"usageStatus":"unavailable"},"tokenUsage":null});
    let kept = json!({"runs":["completed","rolled_back","completed"],"attempts":["completed","completed","completed"],"providerTurns":[turn_row,turn_row,turn_row],"items":[user_item("First"),assistant_item("One"),user_item("Second"),assistant_item("Two"),user_item("Third"),assistant_item("Three")],"requests":[],"plans":[],"providerThread":["idle"]});
    rows.push(row("one_x_rollback", json!([step("First"), step("Second"), {"type":"rollback"}, step("Third")]), entries, kept));
    rows
}

#[tokio::test(flavor = "multi_thread")]
async fn one_x_sessions_the_recordings_do_not_hold_persist_what_the_adapter_is_meant_to() {
    let mut failures = vec![];
    for row in synthetic_rows() {
        let name = row["name"].as_str().unwrap().to_owned();
        match tokio::time::timeout(HANG_BOUND * 2, tokio::spawn(replay(row))).await.unwrap_or_else(|_| panic!("{name} did not finish within the hang bound")) {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
