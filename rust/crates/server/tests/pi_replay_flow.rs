#![cfg(unix)]
//! Replay parity for the Pi driver. The recordings are the Pi RPC sessions the original adapter was recorded
//! against (`apps/server/src/orchestration-v2/testkit/fixtures/*/pi_transcript.ndjson`);
//! `fixtures/pi-replay/<scenario>.json` holds the user-visible projection the unchanged TypeScript orchestrator and
//! `PiAdapterV2` persisted for each (see `tools/generate_pi_replay_fixtures.mjs`). The driver runs `fixtures/pi-fake-cli.py`,
//! which hands every record the Rust adapter writes to the replay controller of `pi_support`: it compares each frame with the
//! recorded one and fails the scenario at the first difference. No real `pi` runs. Tests wait on persisted events, never on sleeps.
mod pi_support;
use chrono::Utc;
use pi_support::Replay;
use serde_json::{Value, json};
use std::sync::Arc;
use t3_server::{
    execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_idle::{IDLE_TIMEOUT_MS, MAX_PIN_MS},
    provider_registry::ProviderRegistry, scheduled_tasks::ManualClock,
};

/// The scenarios the original recorded against Pi, and the ones this port does not replay (none), by name.
const EXCLUDED: &[(&str, &str)] = &[];

struct Harness {
    _root: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    clock: Arc<ManualClock>,
    replay: Replay,
    thread: String,
    next: usize,
}
fn git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

impl Harness {
    async fn start(row: &Value) -> Self {
        let name = row["name"].as_str().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let transcript = row["transcript"].as_array().unwrap();
        let replay = Replay::start(&cwd, name, transcript).await;
        let binary = pi_support::shim(&cwd);
        // The models Pi listed in the recording, so the thread's model is one the instance offers.
        let models = transcript.iter().find(|entry| entry["label"] == "response:get_available_models").map_or(json!([]), |entry| entry["frame"]["data"]["models"].clone());
        let home = cwd.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let settings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "pi":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":binary,"launchArgs":row["launchArgs"]},"environment":[
                {"name":"HOME","value":home},{"name":"PI_FIXTURE_SOCKET","value":replay.socket},{"name":"PI_FIXTURE_MODELS","value":models.to_string()},{"name":"PI_FIXTURE_LOG","value":cwd.join("fixture.log")}]}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let snapshot = providers.snapshots().into_iter().find(|row| row["instanceId"] == "pi").unwrap();
        assert_eq!(snapshot["status"], "ready", "{snapshot}");
        let clock = ManualClock::new(Utc::now().timestamp_millis());
        providers.idle_release().configure(clock.clone(), IDLE_TIMEOUT_MS, MAX_PIN_MS);
        let store = Store::open(cwd.join("state.db")).unwrap();
        // A rollback restores through the thread's checkpoints, which need a worktree of a repository.
        let rolls_back = row["steps"].as_array().unwrap().iter().any(|step| step["type"] == "rollback");
        let (project_root, strategy) = if rolls_back {
            let repository = cwd.join("repository");
            std::fs::create_dir(&repository).unwrap();
            git(&repository, &["init", "-q"]);
            std::fs::write(repository.join("README.md"), "hello\n").unwrap();
            git(&repository, &["add", "."]);
            git(&repository, &["commit", "-q", "-m", "initial"]);
            git(&repository, &["worktree", "add", "-q", "-b", "feature", cwd.join("worktree").to_str().unwrap()]);
            (repository, json!({"type":"existing_worktree","worktreePath":cwd.join("worktree")}))
        } else {
            (cwd.clone(), json!({"type":"root"}))
        };
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":project_root}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":format!("Replay fixture: {name}"),"modelSelection":{"instanceId":"pi","model":row["model"]},"runtimeMode":row["runtimeMode"],"interactionMode":row["interactionMode"],"workspaceStrategy":strategy}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        Self { _root: root, store, execution, providers, clock, replay, thread, next: 0 }
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    fn dispatch(&mut self, text: &str, mode: Value) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":mode}), Utc::now()).unwrap();
        self.view()["runs"].as_array().unwrap().len()
    }
    /// Wait on persisted events until `check` yields a value; a replay that failed says why instead of timing out.
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        let waited = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                if let Some(failure) = self.replay.failure() {
                    panic!("{failure}");
                }
                // Lagged receivers re-check the projection, which is the source of truth.
                let _ = events.recv().await;
            }
        })
        .await;
        waited.unwrap_or_else(|_| {
            let view = self.view();
            let summary = |rows: &str, fields: &[&str]| view[rows].as_array().unwrap().iter().map(|row| fields.iter().map(|field| row[*field].to_string()).collect::<Vec<_>>().join("/")).collect::<Vec<_>>().join(" ");
            panic!("timed out waiting for a persisted event; runs: {}; items: {}; replay at {:?}; errors: {}", summary("runs", &["status"]), summary("turnItems", &["type", "status"]), self.replay.progress(), view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "error").map(|item| item["failure"]["message"].to_string()).collect::<Vec<_>>().join(" | "))
        })
    }
    async fn settled(&self, ordinal: usize) -> Value {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| finished(run)).cloned()).await
    }
    async fn quiet(&self) {
        self.until(|view| view["runs"].as_array().unwrap().iter().all(finished).then_some(())).await;
    }
}
fn finished(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"))
}

// --- what the original's projection says, as a client sees it ---------------------------------------
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
        "compaction" => &["summary", "beforeTokenCount", "afterTokenCount"],
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
fn reference(value: &Value, with_weak_id: bool) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let strong = value["strength"] == "strong";
    Value::Object(pick(value, if strong || with_weak_id { &["driver", "nativeId", "strength"] } else { &["driver", "strength"] }))
}
fn normalize(view: &Value) -> Value {
    let mut items: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] != "checkpoint").collect();
    items.sort_by_key(|item| item["ordinal"].as_u64());
    let sessions = view["providerSessions"].as_array().unwrap();
    json!({
        "runs": view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(),
        "attempts": view["attempts"].as_array().unwrap().iter().map(|attempt| attempt["status"].clone()).collect::<Vec<_>>(),
        "providerTurns": view["providerTurns"].as_array().unwrap().iter().map(|turn| json!({"status":turn["status"],"turnTokenUsage":turn.get("turnTokenUsage").cloned().unwrap_or(Value::Null),"nativeTurnRef":reference(&turn["nativeTurnRef"], false),"tokenUsage":turn.get("tokenUsage").filter(|usage| !usage.is_null()).map_or(Value::Null, |usage| Value::Object(pick(usage, &["usedTokens","maxTokens","inputTokens","cachedInputTokens","outputTokens","reasoningOutputTokens"])))})).collect::<Vec<_>>(),
        "items": items.into_iter().map(normalize_item).collect::<Vec<_>>(),
        "requests": view["runtimeRequests"].as_array().unwrap().iter().map(|request| json!({"kind":request["kind"],"status":request["status"]})).collect::<Vec<_>>(),
        "plans": view["plans"].as_array().unwrap().iter().map(|plan| Value::Object(pick(plan, &["kind","status","steps","markdown"]))).collect::<Vec<_>>(),
        "providerThread": view["providerThreads"].as_array().unwrap().iter().map(|thread| json!({"status":thread["status"],"nativeThreadRef":reference(&thread["nativeThreadRef"], true),"nativeConversationHeadRef":reference(&thread["nativeConversationHeadRef"], true)})).collect::<Vec<_>>(),
        "providerSessions": sessions.iter().rev().take(1).map(|session| json!({"status":session["status"],"model":session["model"],"lastError":session["lastError"]})).collect::<Vec<_>>(),
    })
}
fn assert_rows(name: &str, actual: &Value, expected: &Value) {
    let types = |items: &Value| items.as_array().unwrap().iter().map(|item| format!("{}:{}", item["type"].as_str().unwrap(), item["status"].as_str().unwrap())).collect::<Vec<_>>().join(" ");
    assert_eq!(types(&actual["items"]), types(&expected["items"]), "{name}: the items, in order");
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            let short = |value: &Value| value.to_string().chars().take(300).collect::<String>();
            assert!(got.get(key) == want.get(key), "{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}", want["type"], short(&got[key]), short(&want[key]));
        }
    }
    for key in expected.as_object().unwrap().keys() {
        assert_eq!(actual[key], expected[key], "{name}: {key}");
    }
    assert_eq!(actual, expected, "{name}");
}

async fn replay(row: &Value) {
    let name = row["name"].as_str().unwrap();
    let mut harness = Harness::start(row).await;
    let steps = row["steps"].as_array().unwrap();
    let mut runs = 0;
    for (position, step) in steps.iter().enumerate() {
        if std::env::var("PI_REPLAY_TRACE").is_ok() {
            eprintln!("{name}: step {position} {}", step["type"]);
        }
        match step["type"].as_str().unwrap() {
            "message" => runs = harness.dispatch(step["text"].as_str().unwrap(), json!({"type":"start_immediately"})),
            "steer" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                let run = harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == "running").cloned()).await;
                harness.dispatch(step["text"].as_str().unwrap(), json!({"type":"steer_active","targetRunId":run["id"]}));
            }
            "rollback" => {
                // The checkpoint the step names is the one after that run of the root scope.
                let view = harness.view();
                let scope = view["checkpointScopes"][0]["id"].clone();
                let ordinal: u64 = step["checkpointSuffix"].as_str().unwrap().parse().unwrap();
                let target = harness.until(|view| view["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal && checkpoint["status"] == "ready").cloned()).await["id"].clone();
                harness.execution.dispatch(&json!({"type":"checkpoint.rollback","commandId":"rewind","threadId":harness.thread,"scopeId":scope,"checkpointId":target,"restoreFiles":false}), Utc::now()).unwrap();
                harness.until(|view| (view["thread"]["rollbackFailure"].is_object() || view["runs"].as_array().unwrap().iter().any(|run| run["status"] == "rolled_back")).then_some(())).await;
                assert!(harness.view()["thread"]["rollbackFailure"].is_null(), "{}", harness.view()["thread"]["rollbackFailure"]);
            }
            "interrupt" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                // The recording stops the turn once the model started the tool: wait until the item shows.
                if let Some(kind) = step.get("waitForTurnItemType").and_then(Value::as_str) {
                    harness.until(|view| {
                        let run = view["runs"].get(index)?["id"].clone();
                        view["turnItems"].as_array().unwrap().iter().any(|item| item["runId"] == run && item["type"] == kind).then_some(())
                    })
                    .await;
                }
                let run = harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == "running").cloned()).await;
                harness.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]}), Utc::now()).unwrap();
            }
            "advance_clock" => {
                // The thread is quiet before time passes: its turns have settled and nothing is left to run.
                harness.quiet().await;
                harness.execution.drained(&harness.thread).await;
                let minutes: i64 = step["duration"].as_str().unwrap().strip_suffix(" minutes").unwrap().parse().unwrap();
                harness.providers.idle_release().caught_up(&harness.store).await;
                harness.clock.advance(minutes * 60_000);
                // The idle window ended, so the provider session is released and its process gone.
                harness.until(|view| {
                    let sessions = view["providerSessions"].as_array().unwrap();
                    (!sessions.is_empty() && sessions.iter().all(|session| session["status"] == "stopped")).then_some(())
                })
                .await;
            }
            other => panic!("{name}: unsupported step {other}"),
        }
        // A following message or rollback waits for the thread to be idle, as the original's does.
        if steps.get(position + 1).is_some_and(|next| matches!(next["type"].as_str(), Some("message" | "rollback" | "advance_clock"))) {
            harness.quiet().await;
        }
    }
    harness.settled(runs).await;
    // Everything after the terminal (checkpoint capture) settles through the same persisted state, and so does the end of a
    // process a Stop retired: the run is interrupted a moment before its session says stopped.
    harness.quiet().await;
    let session = row["expected"]["providerSessions"][0]["status"].clone();
    harness.until(|view| (view["providerSessions"].as_array()?.last()?["status"] == session).then_some(())).await;
    harness.replay.assert_complete();
    assert_rows(name, &normalize(&harness.view()), &row["expected"]);
    harness.execution.shutdown().await;
}

async fn run_scenarios(names: &[&str]) {
    let mut failures = vec![];
    for name in names {
        let path = format!("{}/tests/fixtures/pi-replay/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let row: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let outcome = tokio::spawn(async move { replay(&row).await }).await;
        if let Err(error) = outcome {
            let panic = error.into_panic();
            let message = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned())).unwrap_or_default();
            failures.push(format!("{name}: {}", message.chars().take(2400).collect::<String>()));
        }
    }
    assert!(failures.is_empty(), "{} replay scenario(s) differ:\n{}", failures.len(), failures.join("\n---\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_turn_and_a_follow_up_persist_what_the_source_adapter_persisted() {
    run_scenarios(&["simple", "multi_turn"]).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn steering_a_running_turn_and_stopping_one_mid_tool_persist_what_the_source_adapter_persisted() {
    run_scenarios(&["message_steering", "turn_interrupt_mid_tool"]).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn rolling_back_a_thread_with_and_without_a_stopped_turn_persists_what_the_source_adapter_persisted() {
    run_scenarios(&["thread_rollback", "thread_rollback_after_stop"]).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn resuming_after_an_idle_release_and_compacting_persist_what_the_source_adapter_persisted() {
    run_scenarios(&["provider_thread_resume", "pi_compaction"]).await;
}

/// The replay compares every frame the adapter writes with the recording's: a prompt the recording does not have fails the
/// scenario at once, says which frame differed, and the run it was for fails with the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_frame_the_recording_does_not_have_fails_the_replay_and_says_which() {
    let path = format!("{}/tests/fixtures/pi-replay/simple.json", env!("CARGO_MANIFEST_DIR"));
    let mut row: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for entry in row["transcript"].as_array_mut().unwrap() {
        if entry["label"] == "prompt" {
            entry["frame"]["message"] = json!("a prompt nobody recorded");
        }
    }
    let mut harness = Harness::start(&row).await;
    let runs = harness.dispatch("Respond with the following text: fixture simple ok", json!({"type":"start_immediately"}));
    let failure = {
        let mut events = harness.store.subscribe();
        loop {
            if let Some(failure) = harness.replay.failure() {
                break failure;
            }
            let _ = events.recv().await;
        }
    };
    assert!(failure.contains("frame mismatch") && failure.contains("a prompt nobody recorded") && failure.contains("fixture simple ok"), "{failure}");
    // The process the replay ended takes the run with it.
    let mut events = harness.store.subscribe();
    while harness.view()["runs"][runs - 1]["status"] != "failed" {
        let _ = events.recv().await;
    }
    harness.execution.shutdown().await;
}

/// Every recording of the original is replayed or named here with the reason it is not.
#[test]
fn every_recording_is_replayed_or_excluded_by_name_with_a_reason() {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/server/src/orchestration-v2/testkit/fixtures");
    let mut recorded: Vec<String> = std::fs::read_dir(&fixtures).unwrap().flatten().filter(|entry| entry.path().join("pi_transcript.ndjson").exists()).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    recorded.sort();
    let mut replayed: Vec<String> = std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-replay")).unwrap().flatten().map(|entry| entry.path().file_stem().unwrap().to_string_lossy().into_owned()).collect();
    replayed.sort();
    assert_eq!(recorded.len(), 8, "{recorded:?}");
    for (name, reason) in EXCLUDED {
        assert!(!reason.is_empty() && recorded.iter().any(|recorded| recorded == name) && !replayed.iter().any(|replayed| replayed == name), "{name}");
    }
    let unaccounted: Vec<&String> = recorded.iter().filter(|name| !replayed.contains(name) && !EXCLUDED.iter().any(|(excluded, _)| excluded == &name.as_str())).collect();
    assert!(unaccounted.is_empty(), "recordings neither replayed nor excluded: {unaccounted:?}");
    assert!(EXCLUDED.is_empty(), "the list of recordings the port does not replay");
}
