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
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
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

/// Scenarios that are not replayed, each with the reason.
const ONE_X: &str = "recorded against OpenCode 1.x at the SDK client's boundary; this port drives 2.x and reports a 1.x install as one it does not drive yet";
const EXCLUDED: &[(&str, &str)] = &[
    ("simple", ONE_X),
    ("turn_interrupt", ONE_X),
    ("plan_questions", ONE_X),
    ("opencode_subagent", ONE_X),
    ("opencode_child_approval", ONE_X),
    ("opencode_running_child_approval", ONE_X),
];
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
    let actual = normalize(&view);
    let forked = harness.target.clone().map(|target| harness.view_of(&target));
    harness.execution.shutdown().await;
    let status = harness.server.replay_status();
    let consumed = status["failure"].is_null() && status["cursor"] == status["total"];
    let expected = &row["expected"];
    let short = |value: &Value| value.to_string().chars().take(2500).collect::<String>();
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
        match tokio::time::timeout(Duration::from_secs(25), tokio::spawn(replay(row_of(name)))).await.unwrap_or_else(|_| panic!("{name} did not finish in 25s")) {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ:\n{}", failures.len(), SCENARIOS.len(), failures.join("\n"));
}

/// Every excluded scenario is a recorded one, with a reason; nothing else is left out.
#[test]
fn the_exclusions_name_recorded_scenarios_with_reasons() {
    for (name, reason) in EXCLUDED {
        assert!(SCENARIOS.contains(name), "{name} is not a recorded scenario");
        assert!(!reason.is_empty());
    }
    // Only the 1.x recordings are left out, and every 2.x one is replayed.
    for name in SCENARIOS {
        let one_x = row_of(name)["protocol"] == "opencode-sdk.sse";
        assert_eq!(one_x, EXCLUDED.iter().any(|(excluded, _)| *excluded == name), "{name}");
    }
    for name in SCENARIOS {
        let row = row_of(name);
        assert_eq!(row["name"], name);
        assert!(row["transcript"].as_array().is_some_and(|entries| !entries.is_empty()));
    }
}
