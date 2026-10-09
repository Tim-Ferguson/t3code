#![cfg(unix)]
//! Replay parity for the ACP adapter. The recordings are the ACP sessions the source adapter was
//! recorded against (`apps/server/src/orchestration-v2/testkit/fixtures/*/registry_transcript.ndjson`);
//! `fixtures/acp-replay.json` holds the user-visible projection the unchanged TypeScript orchestrator
//! and `AcpRegistryAdapterV2` produced for each (see `tools/generate_acp_replay_fixtures.mjs`).
//! `fixtures/acp-replay-agent.py` plays a recording: it compares every frame the Rust client sends
//! with the recorded one and fails the scenario on the first difference. Tests wait on persisted
//! events, never on sleeps.
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/acp-replay-agent.py");

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
        let settings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "acpRegistry":{"driver":"acpRegistry","enabled":true,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[AGENT,path,status,cwd]}}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":cwd}), Utc::now()).unwrap();
        let runtime_mode = if row["approvalPolicy"] == "on-request" { "approval-required" } else { "full-access" };
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"ACP","modelSelection":{"instanceId":"acpRegistry","model":row["model"]},"runtimeMode":runtime_mode,"interactionMode":row["interactionMode"],"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers);
        Self { status, store, execution, thread, next: 0 }
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
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
const EXCLUDED: &[(&str, &str)] = &[];

async fn replay(row: Value) -> Result<(), String> {
    let row = &row;
    let name = row["name"].as_str().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(scratch.path()).unwrap();
    // The recordings name the workspace the source harness used; the fixtures run in this one.
    let recorded = format!("/tmp/claude-replay-{name}");
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
                harness.until(|view| view["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1].get("status").filter(|status| *status == "running").map(|_| ())).await;
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
    let view = harness.view();
    let ordinals: Vec<String> = view["turnItems"].as_array().unwrap().iter().map(|item| format!("{}#{}@{}", item["type"].as_str().unwrap(), item["ordinal"], item["runId"].as_str().unwrap_or("-").len())).collect();
    let actual = normalize(&view);
    let status = harness.replay_status();
    let consumed = status["failure"].is_null() && status["cursor"] == status["total"];
    harness.execution.shutdown().await;
    let expected = &row["expected"];
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
        return Err(format!("{name}: projection differs\n  rust: {}\n  ts:   {}", short(&actual), short(expected)));
    }
    if !consumed {
        return Err(format!("{name}: the recording was not fully consumed: {status}"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_acp_sessions_persist_what_the_source_adapter_persisted() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("fixtures/acp-replay.json")).unwrap();
    assert_eq!(rows.len(), 11);
    let mut failures = vec![];
    for row in &rows {
        let name = row["name"].as_str().unwrap();
        if EXCLUDED.iter().any(|(excluded, _)| *excluded == name) {
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
