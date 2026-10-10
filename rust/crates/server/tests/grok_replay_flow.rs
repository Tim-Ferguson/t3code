#![cfg(unix)]
//! Replay parity for the Grok driver. The recordings are the Grok ACP sessions the source adapter was
//! recorded against (`apps/server/src/orchestration-v2/testkit/fixtures/*/grok_transcript.ndjson`);
//! `fixtures/grok-replay/<scenario>.json` holds the user-visible projection the unchanged TypeScript
//! orchestrator and `GrokAdapterV2` produced for each (see `tools/generate_grok_replay_fixtures.mjs`).
//! The driver runs `fixtures/grok-fake-cli.py`, which plays a recording through `fixtures/acp-replay-agent.py`:
//! it compares every frame the Rust client sends with the recorded one and fails the scenario on the first
//! difference. No real `grok` runs. Tests wait on persisted events, never on sleeps.
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok-fake-cli.py");

struct Harness {
    status: PathBuf,
    store: Store,
    execution: ExecutionService,
    thread: String,
    next: usize,
}

impl Harness {
    async fn start(root: &std::path::Path, row: &Value, transcript: &Value) -> Self {
        let cwd = std::fs::canonicalize(root).unwrap();
        let status = cwd.join("replay-status.json");
        let path = cwd.join("transcript.json");
        std::fs::write(&path, json!({"scenario":row["name"],"entries":transcript}).to_string()).unwrap();
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
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":cwd}), Utc::now()).unwrap();
        let runtime_mode = if row["approvalPolicy"] == "on-request" { "approval-required" } else { row["runtimeMode"].as_str().unwrap() };
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":format!("Replay fixture: {}", row["name"].as_str().unwrap()),"modelSelection":{"instanceId":"grok","model":row["model"]},"runtimeMode":runtime_mode,"interactionMode":row["interactionMode"],"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers);
        Self { status, store, execution, thread, next: 0 }
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    fn dispatch(&self, command: Value) {
        self.execution.dispatch(&command, Utc::now()).unwrap();
    }
    fn send(&mut self, text: &str, mode: Value) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.dispatch(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":mode}));
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
        outcome.unwrap_or_else(|_| panic!("timed out waiting for a persisted event\nstatus: {}\nprojection: {}", std::fs::read_to_string(&self.status).unwrap_or_default(), normalize(&self.view())))
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

/// Scenarios whose behaviour belongs to the orchestration features the port does not have.
/// Each is named with the reason it is not replayed.
const EXCLUDED: &[(&str, &str)] = &[
    ("grok_background_bash", "holds the run open for a backgrounded command, then continues in Grok's own wake run: deferred finalize, background-task tracking and wake continuation are not in the Grok layer yet"),
    ("grok_background_bash_fast_wake", "as grok_background_bash, with the wake starting inside the finish debounce"),
    ("grok_background_subagent", "holds the run open for a background subagent and its `subagent_finished` wake: the same missing deferred finalize and wake continuation"),
    ("grok_monitor", "holds the run open for a Monitor tool, fed by monitor events, then continues in Grok's wake run: the same missing layer"),
];
/// Scenarios whose source snapshot predates the frames that follow the last reply. The original's adapter
/// republishes the provider thread as it last cached it (active) when such a title arrives, and its snapshot
/// for this scenario was taken before that: the thread the Rust adapter leaves after the same frames is active.
const SNAPSHOT_BEFORE_LATE_FRAMES: &[&str] = &["grok_prompt_error"];
/// The scenarios of the original's Grok replay registry.
const SCENARIOS: [&str; 16] = ["simple", "multi_turn", "queued_turn", "todo_list", "message_steering", "turn_interrupt", "tool_call_read_only", "tool_call_read_only_on_request", "plan_questions", "grok_prompt_error", "grok_auto_blocked_command", "grok_subagent_lineage", "grok_background_bash", "grok_background_bash_fast_wake", "grok_background_subagent", "grok_monitor"];

async fn replay(row: Value) -> Result<(), String> {
    let row = &row;
    let name = row["name"].as_str().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(scratch.path()).unwrap();
    // The recordings name the workspace the source harness used; so do its projections.
    let recorded = row["workspace"].as_str().unwrap_or("").to_owned();
    let rewrite = |value: &Value| serde_json::from_str::<Value>(&value.to_string().replace(&recorded, &workspace.to_string_lossy())).unwrap();
    let (transcript, row) = (rewrite(&row["transcript"]), rewrite(row));
    let mut harness = Harness::start(scratch.path(), &row, &transcript).await;
    let steps = row["steps"].as_array().unwrap();
    let messages = steps.iter().filter(|step| step["type"] == "message" || step["type"] == "queue_message").count();
    let mut runs = 0;
    for (index, step) in steps.iter().enumerate() {
        let next = steps.get(index + 1).map(|step| step["type"].as_str().unwrap());
        match step["type"].as_str().unwrap() {
            "message" => {
                runs = harness.send(step["text"].as_str().unwrap(), json!({"type":"start_immediately"}));
                if !matches!(next, None | Some("interrupt" | "steer" | "queue_message" | "approve_next_runtime_request" | "answer_next_user_input_request" | "stop_background_work")) {
                    harness.settled(runs).await;
                }
            }
            "queue_message" => {
                runs = harness.send(step["text"].as_str().unwrap(), json!({"type":"queue_after_active"}));
            }
            "steer" => {
                let target = harness.view()["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1]["id"].clone();
                // The recording waits for the cancel the steer causes, so the steer may follow the run's start.
                harness.until_raw(|view| view["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1].get("status").filter(|status| *status == "running").map(|_| ())).await;
                harness.send(step["text"].as_str().unwrap(), json!({"type":"steer_active","targetRunId":target}));
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
                let run = harness.until(|view| view["runs"].get(target - 1).filter(|run| run["status"] == "running").cloned()).await;
                harness.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]}));
            }
            "stop_background_work" => {
                harness.idle().await;
                let target = step["targetRunIndex"].as_u64().unwrap() as usize;
                let run = harness.view()["runs"][target - 1]["id"].clone();
                harness.dispatch(json!({"type":"run.interrupt","commandId":"stop-background","threadId":harness.thread,"runId":run,"holdQueue":true}));
                // Stop ends the work in an effect; its result is what the projection shows.
                harness.execution.drained(&harness.thread).await;
            }
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
    // Shutting down closes the session, which the recording ends with.
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
        match tokio::spawn(replay(row.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ:\n{}", failures.len(), rows.len(), failures.join("\n"));
}

/// The exclusion list is exactly the scenarios that hold a run open for background work: those are the ones
/// whose steps release a held run, and no other scenario has such a step.
#[test]
fn exclusions_are_exactly_the_scenarios_that_hold_a_run_open_for_background_work() {
    let mut holding = vec![];
    for name in SCENARIOS {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        if row["steps"].as_array().unwrap().iter().any(|step| matches!(step["type"].as_str(), Some("finish_held_run" | "release_replay_gate"))) {
            holding.push(name);
        }
    }
    let mut excluded: Vec<&str> = EXCLUDED.iter().map(|(name, _)| *name).collect();
    excluded.sort_unstable();
    holding.sort_unstable();
    assert_eq!(excluded, holding);
    assert_eq!(excluded, ["grok_background_bash", "grok_background_bash_fast_wake", "grok_background_subagent", "grok_monitor"]);
    assert!(EXCLUDED.iter().all(|(name, reason)| SCENARIOS.contains(name) && !reason.is_empty()));
    // The late-frame allowance names one scenario whose recording really has a title after its last reply.
    for name in SNAPSHOT_BEFORE_LATE_FRAMES {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/grok-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        let entries = row["transcript"].as_array().unwrap();
        let last_reply = entries.iter().rposition(|entry| entry["label"] == "response:session/prompt").unwrap();
        assert!(entries[last_reply..].iter().any(|entry| entry["frame"]["params"]["update"]["sessionUpdate"] == "session_info_update"));
    }
    assert_eq!(SNAPSHOT_BEFORE_LATE_FRAMES, ["grok_prompt_error"]);
}
