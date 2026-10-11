#![cfg(unix)]
//! Replay parity for the Antigravity driver. The original has no recordings of Antigravity sessions, so
//! `tools/generate_antigravity_replay_fixtures.mjs` runs the unchanged TypeScript orchestrator and
//! `AntigravityAdapterV2` over scripted ACP peers and writes, per scenario, the transcript it played and the
//! user-visible projection it persisted (`fixtures/antigravity-replay/<scenario>.json`). The driver here runs
//! `fixtures/antigravity-fake-cli.py`, which plays the same transcript through `fixtures/acp-replay-agent.py`:
//! it compares every frame the Rust client sends with the scripted one and fails the scenario on the first
//! difference. No real Antigravity runs. Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
mod antigravity_support;
use antigravity_support::{CLI, install_fake_agent, registry, settings};
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

/// The scenarios the generator plays, in its order.
const SCENARIOS: [&str; 10] = [
    "antigravity_simple", "antigravity_multi_turn", "antigravity_tools", "antigravity_approval", "antigravity_question",
    "antigravity_interrupt", "antigravity_failure", "antigravity_subagent", "antigravity_compact", "antigravity_explicit_model",
];
/// Scenarios that are not replayed, each with the reason. All of them are.
const EXCLUDED: &[(&str, &str)] = &[];

struct Harness {
    status: PathBuf,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    /// Attached as `main.rs` attaches it: each session is admitted by it.
    _auth: t3_server::provider_auth_service::ProviderAuthService,
    thread: String,
    next: usize,
}
impl Harness {
    async fn start(root: &std::path::Path, row: &Value, transcript: &Value) -> Self {
        let cwd = std::fs::canonicalize(root).unwrap();
        let (status, path) = (cwd.join("replay-status.json"), cwd.join("transcript.json"));
        std::fs::write(&path, json!({"scenario":row["name"],"entries":transcript}).to_string()).unwrap();
        let workspace = cwd.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        for (name, contents) in row["workspaceFiles"].as_object().into_iter().flatten() {
            std::fs::write(workspace.join(name), contents.as_str().unwrap()).unwrap();
        }
        let agent = install_fake_agent(&cwd);
        let environment = vec![json!({"name":"ANTIGRAVITY_FIXTURE_TRANSCRIPT","value":path}), json!({"name":"ANTIGRAVITY_FIXTURE_STATUS","value":status}), json!({"name":"ANTIGRAVITY_FIXTURE_WORKSPACE","value":workspace})];
        // The original's replay harness runs in `turn` mode.
        let mut settings = settings(&cwd, &agent, json!({}), environment);
        settings.response_streaming_mode = t3_contracts::ResponseStreamingMode::Turn;
        let (providers, _) = registry(&settings, &cwd, &workspace).await;
        let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":workspace}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":format!("Replay fixture: {}", row["name"].as_str().unwrap()),"modelSelection":{"instanceId":"antigravity","model":row["model"]},"runtimeMode":row["runtimeMode"],"interactionMode":row["interactionMode"],"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let auth = t3_server::provider_auth_service::ProviderAuthService::for_server(&execution, &providers, cwd.clone(), &cwd);
        Self { status, store, execution, providers, _auth: auth, thread, next: 0 }
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    fn dispatch(&mut self, command: Value) {
        self.execution.dispatch(&command, Utc::now()).unwrap();
    }
    fn send(&mut self, text: &str) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.dispatch(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}));
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        let outcome = tokio::time::timeout(HANG_BOUND, async {
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
    async fn settled(&self, ordinal: usize) {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).map(|_| ())).await
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
        "approval_request" => &["requestKind", "prompt", "options"],
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
        "subagents": view["subagents"].as_array().unwrap().iter().map(|s| Value::Object(pick(s, &["origin","createdBy","driver","status","prompt","title","model","result"]))).collect::<Vec<_>>(),
    })
}

async fn replay(row: Value) -> Result<(), String> {
    let name = row["name"].as_str().unwrap().to_owned();
    let scratch = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    // The scripted peer and the projections name the workspace the original ran in; this one runs in its own.
    let recorded = row["workspace"].as_str().unwrap_or("").to_owned();
    let rewrite = |value: &Value| serde_json::from_str::<Value>(&value.to_string().replace(&recorded, &root.join("workspace").to_string_lossy())).unwrap();
    let (transcript, row) = (row["transcript"].clone(), rewrite(&row));
    let mut harness = Harness::start(scratch.path(), &row, &transcript).await;
    let steps = row["steps"].as_array().unwrap();
    let mut runs = 0;
    for (index, step) in steps.iter().enumerate() {
        let next = steps.get(index + 1).map(|step| step["type"].as_str().unwrap());
        match step["type"].as_str().unwrap() {
            "message" => {
                runs = harness.send(step["text"].as_str().unwrap());
                if !matches!(next, None | Some("interrupt" | "approve_next_runtime_request" | "answer_next_user_input_request")) {
                    harness.settled(runs).await;
                }
            }
            "approve_next_runtime_request" => {
                let request = harness.pending_request().await;
                harness.dispatch(json!({"type":"runtime-request.respond","commandId":format!("approve-{index}"),"threadId":harness.thread,"requestId":request["id"],"decision":step["decision"].as_str().unwrap_or("accept")}));
                // The next message starts once the run this answer let go has finished.
                if matches!(next, Some("message")) {
                    harness.settled(runs).await;
                }
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
            other => return Err(format!("{name}: unsupported step {other}")),
        }
    }
    harness.idle().await;
    harness.settled(runs).await;
    let view = harness.view();
    let actual = normalize(&view);
    harness.execution.shutdown().await;
    let status = harness.replay_status();
    let consumed = status["failure"].is_null() && status["cursor"] == status["total"];
    let expected = &row["expected"];
    let short = |value: &Value| value.to_string().chars().take(2500).collect::<String>();
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            if got.get(key) != want.get(key) {
                return Err(format!("{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}\n  rust items: {}\n  status: {}", want["type"], short(&got[key]), short(&want[key]), short(&actual["items"]), harness.replay_status()));
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
        return Err(format!("{name}: the script was not fully consumed: {status}"));
    }
    let _ = CLI;
    let _ = &harness.providers;
    Ok(())
}

fn fixture(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/antigravity-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn scripted_antigravity_sessions_persist_what_the_source_adapter_persisted() {
    let mut failures = vec![];
    for name in SCENARIOS {
        if EXCLUDED.iter().any(|(excluded, _)| *excluded == name) {
            continue;
        }
        if std::env::var("ANTIGRAVITY_REPLAY_ONLY").is_ok_and(|only| !only.split(',').any(|wanted| wanted == name)) {
            continue;
        }
        // A scenario that stalls panics inside `until`; report it with the others.
        match tokio::spawn(replay(fixture(name))).await {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => failures.push(failure),
            Err(cause) => failures.push(format!("{name}: {}", cause.try_into_panic().ok().and_then(|payload| payload.downcast_ref::<String>().cloned()).unwrap_or_default())),
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios differ:\n{}", failures.len(), SCENARIOS.len(), failures.join("\n"));
}

/// The list the test replays is the list the generator wrote, and nothing is excluded without a reason.
#[test]
fn every_scenario_the_generator_wrote_is_replayed_or_excluded_by_name() {
    let mut written: Vec<String> = std::fs::read_dir(format!("{}/tests/fixtures/antigravity-replay", env!("CARGO_MANIFEST_DIR"))).unwrap().filter_map(|entry| entry.unwrap().file_name().to_str()?.strip_suffix(".json").map(str::to_owned)).collect();
    written.sort();
    let mut known: Vec<String> = SCENARIOS.iter().map(|name| (*name).to_owned()).collect();
    known.sort();
    assert_eq!(written, known);
    assert!(EXCLUDED.is_empty(), "every scripted session is replayed");
    for (name, reason) in EXCLUDED {
        assert!(SCENARIOS.contains(name) && !reason.is_empty());
    }
}
