#![cfg(unix)]
//! Replay parity for the Codex adapter. `fixtures/codex-replay.json` holds, for each recorded
//! Codex app-server transcript of the original, what the unchanged TypeScript orchestrator and
//! `CodexAdapterV2` persisted (see `tools/generate_codex_replay_fixtures.mjs`). The fake Codex
//! (`fixtures/codex-provider.py`) serves the same recording to the Rust adapter, which must persist
//! the same user-visible items, requests, plans and token usage. A scenario that differs is a bug in
//! the adapter, not an expectation to relax. Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry, thread::ThreadService};

struct Harness {
    cwd: PathBuf,
    log: PathBuf,
    store: Store,
    providers: ProviderRegistry,
    execution: ExecutionService,
    thread: String,
    next: usize,
    /// The clock a provider session's idle time runs on; only `advance_clock` moves it.
    clock: std::sync::Arc<t3_server::scheduled_tasks::ManualClock>,
    _root: tempfile::TempDir,
}

fn shim(cwd: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let binary = cwd.join("codex");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

impl Harness {
    async fn start(environment: Vec<(&str, String)>, runtime_mode: &str, interaction_mode: &str, model: &str) -> Self {
        Self::start_in(environment, runtime_mode, interaction_mode, model, false, "Codex").await
    }
    /// `worktree` runs the thread in its own git worktree, the workspace a checkpoint rollback needs.
    async fn start_in(environment: Vec<(&str, String)>, runtime_mode: &str, interaction_mode: &str, model: &str, worktree: bool, title: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let log = cwd.join("fixture.log");
        let (project_root, workspace, strategy) = if worktree {
            let repository = cwd.join("repository");
            std::fs::create_dir(&repository).unwrap();
            git(&repository, &["init", "-q"]);
            std::fs::write(repository.join("README.md"), "hello\n").unwrap();
            git(&repository, &["add", "."]);
            git(&repository, &["commit", "-q", "-m", "initial"]);
            git(&repository, &["worktree", "add", "-q", "-b", "feature", cwd.join("worktree").to_str().unwrap()]);
            (repository, cwd.join("worktree"), json!({"type":"existing_worktree","worktreePath":cwd.join("worktree")}))
        } else {
            (cwd.clone(), cwd.clone(), json!({"type":"root"}))
        };
        let mut environment: Vec<Value> = environment.into_iter().map(|(name, value)| json!({"name":name,"value":value})).collect();
        environment.push(json!({"name":"CODEX_FIXTURE_LOG","value":log}));
        environment.push(json!({"name":"CODEX_FIXTURE_WORKSPACE","value":workspace}));
        // The original's replay runs on a test clock, so streamed text is only ever persisted at the
        // end of its item; the flush timer is stretched so that it cannot reorder items here.
        environment.push(json!({"name":"T3CODE_CODEX_STREAM_FLUSH_MS","value":"600000"}));
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
            "claudeAgent":{"driver":"claudeAgent","enabled":false},
            "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":shim(&cwd),"homePath":cwd.join("codex-home")},"environment":environment}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let clock = t3_server::scheduled_tasks::ManualClock::new(Utc::now().timestamp_millis());
        providers.idle_release().configure(clock.clone(), t3_server::provider_idle::IDLE_TIMEOUT_MS, t3_server::provider_idle::MAX_PIN_MS);
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":project_root}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":title,"modelSelection":{"instanceId":"codex","model":model},"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"workspaceStrategy":strategy}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        Self { cwd, log, store, providers, execution, thread, next: 0, clock, _root: root }
    }
    fn view(&self) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", &self.thread).unwrap().unwrap())
    }
    fn dispatch_message(&mut self, text: &str, extra: Value) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":{"type":"start_immediately"}});
        for (key, value) in extra.as_object().unwrap() {
            command[key] = value.clone();
        }
        self.execution.dispatch(&command, Utc::now()).unwrap();
        self.view()["runs"].as_array().unwrap().len()
    }
    /// Wait on persisted events until `check` yields a value. A fixture that rejected a frame the
    /// adapter sent is reported instead of waiting out the timeout.
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        let found = tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        match found {
            Ok(found) => found,
            Err(_) => panic!("timed out waiting for a persisted event; fixture log: {:?}; view: {}", self.mismatch(), self.summary()),
        }
    }
    fn summary(&self) -> String {
        let view = self.view();
        format!("runs {:?} items {:?}", view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(), view["turnItems"].as_array().unwrap().iter().map(|item| format!("{}:{}", item["type"], item["status"])).collect::<Vec<_>>())
    }
    /// The thread shell as the scenarios capture it, with run ids as their position in the thread.
    fn shell_snapshot(&self) -> Value {
        let view = self.view();
        let shell = t3_server::thread::shell(&view);
        let position = |id: &Value| if id.is_null() { Value::Null } else { json!(view["runs"].as_array().unwrap().iter().position(|run| run["id"] == *id).unwrap() + 1) };
        json!({"latestRun":position(&shell["latestRunId"]),"activeRun":position(&shell["activeRunId"]),"status":shell["status"],"activityRunStatus":shell["activityRunStatus"],"pendingBackgroundTasks":shell["pendingBackgroundTasks"]})
    }
    fn mismatch(&self) -> Option<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find(|entry| entry.get("mismatch").is_some())
    }
    async fn settled(&self, ordinal: usize) -> Value {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await
    }
    async fn pending_request(&self) -> Value {
        self.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    async fn stop(self) {
        self.execution.shutdown().await;
    }
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

// --- normalised projection ----------------------------------------------------------------------
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

/// `permissions` of a `thread/start`-time recorded runtime policy name the runtime mode whose
/// native approval policy is closest; the fake does not compare those fields (see the fake).
fn runtime_mode(policy: &Value) -> &'static str {
    match policy["approvalPolicy"].as_str() {
        Some("never") => "full-access",
        Some("untrusted") => "approval-required",
        Some("on-request") => "auto-accept-edits",
        _ if policy["approvalPolicy"].is_object() => "approval-required",
        _ => "full-access",
    }
}

fn assert_projection(name: &str, actual: &Value, expected: &Value) {
    let types = |items: &Value| items.as_array().unwrap().iter().map(|item| format!("{}:{}", item["type"].as_str().unwrap(), item["status"].as_str().unwrap())).collect::<Vec<_>>().join(" ");
    assert_eq!(types(&actual["items"]), types(&expected["items"]), "{name}: the items, in order");
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            let short = |value: &Value| value.to_string().chars().take(400).collect::<String>();
            assert!(got.get(key) == want.get(key), "{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}", want["type"], short(&got[key]), short(&want[key]));
        }
    }
    assert_eq!(actual["items"].as_array().unwrap().len(), expected["items"].as_array().unwrap().len(), "{name}: item count");
    assert_eq!(actual, expected, "{name}");
}

/// Every thread the scenario created, the root first, as the original's projections list them:
/// each thread's own projection, how it relates to its parent, and the subagents it holds.
fn assert_threads(name: &str, harness: &Harness, expected: &[Value]) {
    let mut found: Vec<Value> = vec![harness.view()];
    let mut index = 0;
    while index < found.len() {
        let children: Vec<String> = found[index]["subagents"].as_array().unwrap().iter().filter_map(|task| task["childThreadId"].as_str().map(str::to_owned)).collect();
        for child in children {
            found.push(harness.store.projection("thread", &child).unwrap().unwrap_or_else(|| panic!("{name}: child thread {child} is missing")));
        }
        index += 1;
    }
    assert_eq!(found.len(), expected.len(), "{name}: the threads created: {:?}", found.iter().map(|view| view["thread"]["title"].clone()).collect::<Vec<_>>());
    let title_of = |id: &Value| found.iter().find(|view| view["thread"]["id"] == *id).map_or(Value::Null, |view| view["thread"]["title"].clone());
    for want in expected {
        let view = found.iter().find(|view| view["thread"]["title"] == want["title"]).unwrap_or_else(|| panic!("{name}: no thread titled {}", want["title"]));
        let label = format!("{name}: thread {}", want["title"]);
        assert_eq!(title_of(&view["thread"]["lineage"]["parentThreadId"]), want["parent"], "{label}: parent");
        assert_eq!(view["thread"]["lineage"]["relationshipToParent"], want["relationship"], "{label}: relationship");
        let subagents: Vec<Value> = view["subagents"].as_array().unwrap().iter().map(|task| {
            let mut row = serde_json::Map::new();
            for key in ["origin", "createdBy", "driver", "status", "prompt", "title", "model", "result"] {
                row.insert(key.to_owned(), task[key].clone());
            }
            row.insert("child".into(), title_of(&task["childThreadId"]));
            Value::Object(row)
        }).collect();
        assert_eq!(Value::Array(subagents), want["subagents"], "{label}: subagents");
        assert_projection(&label, &normalize(view), &want["expected"]);
    }
}

/// The recordings the port replays; the generator lists what it leaves out and why.
fn rows() -> (Vec<Value>, Vec<Value>) {
    let file: Value = serde_json::from_str(include_str!("fixtures/codex-replay.json")).unwrap();
    (file["scenarios"].as_array().unwrap().clone(), file["excluded"].as_array().unwrap().clone())
}

async fn replay(row: &Value) {
    let name = row["name"].as_str().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("transcript.ndjson");
    std::fs::write(&transcript, row["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
    let mut environment = vec![("CODEX_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned())];
    let steps = row["steps"].as_array().unwrap();
    // A step that releases the replay gate holds the recording back at that frame until then.
    let gate = steps.iter().find(|step| step["type"] == "release_replay_gate").map(|step| {
        let fifo = scratch.path().join("gate.fifo");
        let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        environment.push(("CODEX_FIXTURE_GATE_LABEL", step["label"].as_str().unwrap().to_owned()));
        environment.push(("CODEX_FIXTURE_GATE_FIFO", fifo.to_string_lossy().into_owned()));
        fifo
    });
    let mut shells = serde_json::Map::new();
    let rolls_back = steps.iter().any(|step| step["type"] == "rollback");
    let mut harness = Harness::start_in(environment, runtime_mode(&row["runtimePolicy"]), row["interactionMode"].as_str().unwrap(), row["model"].as_str().unwrap(), rolls_back, &format!("Replay fixture: {name}")).await;
    let mut runs = 0;
    for step in steps {
        match step["type"].as_str().unwrap() {
            "message" => {
                runs = harness.dispatch_message(step["text"].as_str().unwrap(), json!({}));
            }
            "queue_message" => {
                harness.next += 1;
                let id = format!("message-{}", harness.next);
                let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":harness.thread,"messageId":id,"text":step["text"],"attachments":[],"dispatchMode":{"type":"queue_after_active"}});
                for key in ["createdBy", "creationSource"] {
                    if let Some(value) = step.get(key) {
                        command[key] = value.clone();
                    }
                }
                harness.execution.dispatch(&command, Utc::now()).unwrap();
                runs = harness.view()["runs"].as_array().unwrap().len();
            }
            "approve_next_runtime_request" => {
                let request = harness.pending_request().await;
                if let Some(key) = step.get("shellSnapshotKeyWhilePending").and_then(Value::as_str) {
                    shells.insert(key.to_owned(), harness.shell_snapshot());
                }
                harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"approve","threadId":harness.thread,"requestId":request["id"],"decision":step.get("decision").cloned().unwrap_or(json!("accept"))}), Utc::now()).unwrap();
            }
            "answer_next_user_input_request" => {
                let request = harness.pending_request().await;
                harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"answer","threadId":harness.thread,"requestId":request["id"],"answers":step["answers"]}), Utc::now()).unwrap();
            }
            "rollback" => {
                // The checkpoint the step names is the one after that run of the root scope.
                let view = harness.view();
                let scope = view["checkpointScopes"][0]["id"].clone();
                let ordinal: u64 = step["checkpointSuffix"].as_str().unwrap().parse().unwrap();
                let target = view["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal).unwrap()["id"].clone();
                harness.execution.dispatch(&json!({"type":"checkpoint.rollback","commandId":"rewind","threadId":harness.thread,"scopeId":scope,"checkpointId":target,"restoreFiles":false}), Utc::now()).unwrap();
                // Codex has been asked to drop the later turns and the later runs are history.
                harness.until(|view| (view["thread"]["rollbackFailure"].is_object() || view["runs"].as_array().unwrap().iter().any(|run| run["status"] == "rolled_back")).then_some(())).await;
                assert!(harness.view()["thread"]["rollbackFailure"].is_null(), "{}", harness.view()["thread"]["rollbackFailure"]);
            }
            "steer" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                let run = harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == "running").cloned()).await;
                harness.next += 1;
                let id = format!("message-{}", harness.next);
                harness.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":harness.thread,"messageId":id,"text":step["text"],"attachments":[],"dispatchMode":{"type":"steer_active","targetRunId":run["id"]}}), Utc::now()).unwrap();
            }
            "interrupt" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                if let Some(kind) = step.get("waitForTurnItemType").and_then(Value::as_str) {
                    harness.until(|view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == kind && item["status"] == "running").cloned()).await;
                }
                let run = harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == "running").cloned()).await;
                harness.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]}), Utc::now()).unwrap();
            }
            "cancel_queued_run" => {
                let run = harness.view()["runs"][step["targetRunIndex"].as_u64().unwrap() as usize - 1]["id"].clone();
                harness.execution.dispatch(&json!({"type":"queued-run.cancel","commandId":"cancel-queued","threadId":harness.thread,"runId":run}), Utc::now()).unwrap();
            }
            "await_run_status" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == step["status"]).map(|_| ())).await;
            }
            "capture_shell_snapshot" => {
                shells.insert(step["key"].as_str().unwrap().to_owned(), harness.shell_snapshot());
            }
            "advance_clock" => {
                // The thread is quiet before time passes: its turns have settled and nothing is left to run.
                harness.until(|view| (view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back")))).then_some(())).await;
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
            "release_replay_gate" => {
                use std::io::Write;
                std::fs::OpenOptions::new().write(true).open(gate.as_ref().unwrap()).unwrap().write_all(b"release").unwrap();
            }
            other => panic!("{name}: unsupported step {other}"),
        }
        // The next message, like a rollback, waits for the thread to be idle, as the original's does.
        let position = steps.iter().position(|s| s == step).unwrap();
        if steps.get(position + 1).is_some_and(|next| matches!(next["type"].as_str(), Some("message" | "rollback" | "advance_clock"))) {
            harness.until(|view| (view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back")))).then_some(())).await;
        }
    }
    harness.settled(runs).await;
    // Everything after the terminal (checkpoint capture) settles through the same persisted state.
    harness.until(|view| view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"))).then_some(())).await;
    assert!(harness.mismatch().is_none(), "{name}: the adapter sent a frame the recording does not have: {:?}", harness.mismatch());
    for (key, snapshot) in &shells {
        assert_eq!(snapshot, &row["shells"][key], "{name}: the thread shell captured as {key}");
    }
    assert_eq!(shells.len(), row["shells"].as_object().unwrap().len(), "{name}: captured shells");
    assert_projection(name, &normalize(&harness.view()), &row["expected"]);
    // A subagent's child threads are threads of their own, found by their lineage.
    if let Some(expected_threads) = row.get("threads").and_then(Value::as_array) {
        assert_threads(name, &harness, expected_threads);
    }
    let _ = &harness.cwd;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_app_server_streams_persist_what_the_source_adapter_persisted() {
    let (scenarios, _) = rows();
    assert!(scenarios.len() >= 10);
    let only = std::env::var("CODEX_REPLAY_ONLY").ok();
    let mut failures = vec![];
    for row in &scenarios {
        if only.as_deref().is_some_and(|only| !only.split(',').any(|name| name == row["name"].as_str().unwrap())) {
            continue;
        }
        let outcome = tokio::spawn({
            let row = row.clone();
            async move { replay(&row).await }
        })
        .await;
        if let Err(error) = outcome {
            let panic = error.into_panic();
            let message = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned())).unwrap_or_default();
            failures.push(format!("{}: {}", row["name"], message.chars().take(1800).collect::<String>()));
        }
    }
    assert!(failures.is_empty(), "{} replay scenario(s) differ:\n{}", failures.len(), failures.join("\n---\n"));
}

#[test]
fn every_excluded_recording_names_why_the_port_does_not_replay_it() {
    let (scenarios, excluded) = rows();
    for entry in &excluded {
        assert!(entry["reason"].as_str().is_some_and(|reason| reason.len() > 20), "{entry}");
        assert!(!scenarios.iter().any(|row| row["name"] == entry["name"]), "{entry} is both replayed and excluded");
        let file: Value = serde_json::from_str(include_str!("fixtures/codex-replay.json")).unwrap();
        assert!(!file["forks"].as_array().unwrap().iter().any(|row| row["name"] == entry["name"]), "{entry} is both replayed as a fork and excluded");
    }
}

/// The recording whose second turn runs in a new server process: the thread is resumed natively
/// (`thread/resume`, no history) and the turn continues from what Codex remembers.
#[tokio::test(flavor = "multi_thread")]
async fn a_native_thread_is_resumed_after_a_server_restart() {
    let file: Value = serde_json::from_str(include_str!("fixtures/codex-replay.json")).unwrap();
    let row = &file["recovery"][0];
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("transcript.ndjson");
    std::fs::write(&transcript, row["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
    let mut harness = Harness::start(vec![("CODEX_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned()), ("T3CODE_CODEX_STREAM_FLUSH_MS", "600000".into())], "full-access", "default", row["model"].as_str().unwrap()).await;
    let prompts = row["prompts"].as_array().unwrap();
    let runs = harness.dispatch_message(prompts[0].as_str().unwrap(), json!({}));
    harness.settled(runs).await;
    let native = harness.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"].clone();
    // The server stops and starts again over the same state: the process, its callbacks and the
    // running turn are gone, the native thread id is not.
    let Harness { cwd, log, store, providers, execution, thread, next, clock, _root } = harness;
    execution.shutdown().await;
    drop(execution);
    drop(store);
    let store = Store::open(cwd.join("state.db")).unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let mut harness = Harness { cwd, log, store, providers, execution, thread, next, clock, _root };
    let runs = harness.dispatch_message(prompts[1].as_str().unwrap(), json!({}));
    harness.settled(runs).await;
    harness.until(|view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed").then_some(())).await;
    assert!(harness.mismatch().is_none(), "the adapter sent a frame the recording does not have: {:?}", harness.mismatch());
    let view = harness.view();
    assert_eq!(view["providerThreads"][0]["nativeThreadRef"]["nativeId"], native, "one native thread across both processes");
    let texts: Vec<&str> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "assistant_message").map(|item| item["text"].as_str().unwrap()).collect();
    let finals: Vec<&str> = row["finals"].as_array().unwrap().iter().map(|text| text.as_str().unwrap()).collect();
    assert_eq!(texts, finals);
    harness.stop().await;
}

// --- thread forks and merge-backs -------------------------------------------------------------------
// `forks` in the fixture: one recording, several threads. The port serves each thread from a process
// of its own, so the fake hands every process the part of the recording for the native thread it opens
// (see the fake). What the original persisted for each thread, with ids named by what they identify,
// is compared here.

struct Forks {
    cwd: PathBuf,
    log: PathBuf,
    inject_log: PathBuf,
    store: Store,
    execution: ExecutionService,
    /// Alias to thread id, in the order the scenario created them.
    ids: Vec<(String, String)>,
    sent: std::collections::HashMap<String, usize>,
    next: usize,
    _root: tempfile::TempDir,
}

impl Forks {
    async fn start(row: &Value) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(scratch.path()).unwrap();
        let transcript = cwd.join("transcript.ndjson");
        std::fs::write(&transcript, row["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
        let (log, inject_log) = (cwd.join("fixture.log"), cwd.join("inject.log"));
        let workspace = cwd.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("README.md"), "# thread fork\n").unwrap();
        git(&workspace, &["init", "-q"]);
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-q", "-m", "initial"]);
        let environment: Vec<Value> = [
            ("CODEX_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned()),
            ("CODEX_FIXTURE_CLAIMS", cwd.join("claims").to_string_lossy().into_owned()),
            ("CODEX_FIXTURE_LOG", log.to_string_lossy().into_owned()),
            ("CODEX_FIXTURE_INJECT_LOG", inject_log.to_string_lossy().into_owned()),
            ("CODEX_FIXTURE_WORKSPACE", workspace.to_string_lossy().into_owned()),
            ("T3CODE_CODEX_STREAM_FLUSH_MS", "600000".into()),
        ]
        .into_iter()
        .map(|(name, value)| json!({"name":name,"value":value}))
        .collect();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
            "claudeAgent":{"driver":"claudeAgent","enabled":false},
            "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":shim(&cwd),"homePath":cwd.join("codex-home")},"environment":environment}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":workspace}), Utc::now()).unwrap();
        let source = row["threadIds"]["source"].as_str().unwrap();
        ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","threadId":source,"projectId":"project","title":row["sourceTitle"],"modelSelection":{"instanceId":"codex","model":row["model"]},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap();
        let execution = ExecutionService::start(store.clone(), providers);
        let ids = row["threadIds"].as_object().unwrap().iter().map(|(alias, id)| (alias.clone(), id.as_str().unwrap().to_owned())).collect();
        Self { cwd, log, inject_log, store, execution, ids, sent: Default::default(), next: 0, _root: scratch }
    }
    fn id(&self, alias: &str) -> &str {
        &self.ids.iter().find(|(name, _)| name == alias).unwrap_or_else(|| panic!("no thread {alias}")).1
    }
    fn view_of(&self, id: &str) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", id).unwrap().unwrap_or_else(|| panic!("thread {id} does not exist")))
    }
    fn mismatch(&self) -> Option<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find(|entry| entry.get("mismatch").is_some())
    }
    async fn until(&self, id: &str, mut check: impl FnMut(&Value) -> bool) {
        let mut events = self.store.subscribe();
        let waited = tokio::time::timeout(HANG_BOUND, async {
            loop {
                if check(&self.view_of(id)) {
                    return;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        if waited.is_err() {
            let view = self.view_of(id);
            panic!("timed out waiting on {id}; fixture mismatch: {:?}; runs {:?}", self.mismatch(), view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>());
        }
    }
    /// The thread has taken every message sent to it and all its runs are over.
    async fn idle(&self, alias: &str) {
        let wanted = self.sent.get(alias).copied().unwrap_or(0);
        self.until(self.id(alias), |view| {
            let runs = view["runs"].as_array().unwrap();
            runs.len() >= wanted && runs.iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back")))
        })
        .await;
    }
    fn run_id(&self, point: &Value) -> Value {
        if point["type"] != "run" {
            return Value::Null;
        }
        self.view_of(self.id(point["thread"].as_str().unwrap()))["runs"][point["ordinal"].as_u64().unwrap() as usize - 1]["id"].clone()
    }
    fn source_point(&self, point: &Value) -> Value {
        if point["type"] == "run" { json!({"type":"run","runId":self.run_id(point)}) } else { point.clone() }
    }
    fn thread_command(&mut self, command: Value) -> t3_server::persistence::Receipt {
        ThreadService::new(self.store.clone()).dispatch(&command, Utc::now()).unwrap()
    }
}

/// The rows of a thread the original's description names (`describeThread` of the generator).
fn describe(forks: &Forks, id: &str) -> Value {
    let view = forks.view_of(id);
    let alias = |thread: &Value| thread.as_str().map_or(Value::Null, |id| forks.ids.iter().find(|(_, known)| known == id).map_or(json!(id), |(alias, _)| json!(alias)));
    let thread_view = |id: &Value| id.as_str().and_then(|id| forks.store.projection("thread", id).unwrap());
    let run_ordinal = |thread: &Value, run: &Value| {
        if run.is_null() { return Value::Null }
        thread_view(thread).and_then(|view| view["runs"].as_array().unwrap().iter().find(|candidate| candidate["id"] == *run).map(|run| run["ordinal"].clone())).unwrap_or(Value::Null)
    };
    let thread = &view["thread"];
    let forked = &thread["forkedFrom"];
    let source_view = thread_view(&forked["threadId"]);
    let native_of = |view: &Option<Value>, field: &str, id: &Value, native: &str| {
        view.as_ref().and_then(|view| view[field].as_array().unwrap().iter().find(|row| row["id"] == *id).map(|row| row[native]["nativeId"].clone())).unwrap_or(Value::Null)
    };
    let pick = |value: &Value, keys: &[&str]| Value::Object(keys.iter().filter_map(|key| value.get(*key).map(|found| ((*key).to_owned(), found.clone()))).collect());
    json!({
        "title":thread["title"],"parent":alias(&thread["lineage"]["parentThreadId"]),"relationship":thread["lineage"]["relationshipToParent"],"root":alias(&thread["lineage"]["rootThreadId"]),
        "forkedFrom":if forked.is_null() { Value::Null } else { json!({"type":forked["type"],"thread":alias(&forked["threadId"]),"runOrdinal":run_ordinal(&forked["threadId"], &forked["runId"])}) },
        "expected":normalize(&view),
        "providerThreads":view["providerThreads"].as_array().unwrap().iter().map(|provider| json!({
            "nativeId":provider["nativeThreadRef"].get("nativeId").cloned().unwrap_or(Value::Null),
            "handoffs":provider["handoffIds"].as_array().unwrap().len(),
            "forkedFrom":match provider.get("forkedFrom").filter(|forked| !forked.is_null()) {
                None => Value::Null,
                Some(forked) => json!({"nativeThreadId":native_of(&source_view, "providerThreads", &forked["providerThreadId"], "nativeThreadRef"),
                    "nativeTurnId":match forked.get("providerTurnId") { None => Value::Null, Some(turn) => native_of(&source_view, "providerTurns", turn, "nativeTurnRef") }}),
            },
        })).collect::<Vec<_>>(),
        "runs":view["runs"].as_array().unwrap().iter().map(|run| json!({"status":run["status"],"carriesHandoff":!run["contextHandoffId"].is_null()})).collect::<Vec<_>>(),
        "transfers":view["contextTransfers"].as_array().unwrap().iter().map(|transfer| json!({
            "type":transfer["type"],"source":alias(&transfer["sourceThreadId"]),"target":alias(&transfer["targetThreadId"]),"status":transfer["status"],
            "resolution":match transfer["resolution"]["strategy"].as_str() { None => Value::Null, Some("native_fork") => json!({"strategy":"native_fork","nativeId":transfer["resolution"]["providerThreadRef"]["nativeId"]}), Some(strategy) => json!({"strategy":strategy}) },
            "sourceRunOrdinal":run_ordinal(&transfer["sourceThreadId"], &transfer["sourcePoint"]["runId"]),
            "baseRunOrdinal":if transfer["basePoint"].is_null() { Value::Null } else { run_ordinal(&transfer["basePoint"]["threadId"], &transfer["basePoint"]["runId"]) },
            "targetRunOrdinal":run_ordinal(&transfer["targetThreadId"], &transfer["targetRunId"]),
            "sourceProviderInstanceId":transfer["sourceProviderInstanceId"],"targetProviderInstanceId":transfer["targetProviderInstanceId"],
            "error":transfer["error"],"consumed":!transfer["consumedAt"].is_null(),
        })).collect::<Vec<_>>(),
        "handoffs":view["contextHandoffs"].as_array().unwrap().iter().map(|handoff| {
            let mut row = json!({"strategy":handoff["strategy"],"status":handoff["status"],"summaryText":handoff["summaryText"],"coveredRunOrdinals":handoff["coveredRunOrdinals"],
                "delivery":if handoff["delivery"].is_null() { Value::Null } else { json!({"status":handoff["delivery"]["status"],"items":handoff["delivery"]["itemIds"].as_array().unwrap().len()}) }});
            if let Some(history) = handoff["history"]["messages"].as_array() {
                row["history"] = Value::Array(history.iter().map(|message| pick(message, &["role", "text", "kind", "status", "threadId"])).collect());
            }
            row
        }).collect::<Vec<_>>(),
        "handoffItems":view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "handoff").map(|item| pick(item, &["title", "strategy", "fromProviderInstanceIds", "toProviderInstanceId", "toModel", "summary", "status"])).collect::<Vec<_>>(),
        "visible":view["visibleTurnItems"].as_array().unwrap().iter().map(|row| {
            let mut out = json!({"visibility":row["visibility"],"type":row["item"]["type"]});
            if matches!(row["item"]["type"].as_str(), Some("user_message" | "assistant_message")) {
                out["text"] = row["item"]["text"].clone();
            }
            out
        }).collect::<Vec<_>>(),
        "shell":{"visibleItemCount":t3_server::thread::shell(&view)["visibleItemCount"],"relationship":thread["lineage"]["relationshipToParent"],"forkedFrom":forked.get("type").cloned().unwrap_or(Value::Null)},
    })
}

async fn replay_forks(row: &Value) {
    let name = row["name"].as_str().unwrap();
    let mut forks = Forks::start(row).await;
    for step in row["steps"].as_array().unwrap() {
        match step["type"].as_str().unwrap() {
            "message" => {
                let alias = step["thread"].as_str().unwrap().to_owned();
                forks.next += 1;
                let id = format!("message-{}", forks.next);
                let command = json!({"type":"message.dispatch","commandId":id,"threadId":forks.id(&alias),"messageId":id,"text":step["text"],"attachments":[],"dispatchMode":{"type":"start_immediately"}});
                forks.execution.dispatch(&command, Utc::now()).unwrap_or_else(|error| panic!("{name}: message to {alias}: {error}"));
                *forks.sent.entry(alias.clone()).or_default() += 1;
                forks.idle(&alias).await;
            }
            "fork" => {
                forks.next += 1;
                let command = json!({"type":"thread.fork","commandId":format!("fork-{}",forks.next),"sourceThreadId":forks.id(step["from"].as_str().unwrap()),"targetThreadId":forks.id(step["to"].as_str().unwrap()),"sourcePoint":forks.source_point(&step["sourcePoint"]),"title":step["title"]});
                let receipt = forks.thread_command(command.clone());
                assert_eq!(receipt.status, "accepted", "{name}: fork: {:?}", receipt.error);
                if step["twice"] == true {
                    // The same command again answers with its receipt and creates nothing more.
                    assert_eq!(forks.thread_command(command), receipt, "{name}: a replayed fork command");
                }
            }
            "merge_back" => {
                forks.next += 1;
                let command = json!({"type":"thread.merge_back","commandId":format!("merge-{}",forks.next),"sourceThreadId":forks.id(step["from"].as_str().unwrap()),"targetThreadId":forks.id(step["into"].as_str().unwrap()),"sourcePoint":forks.source_point(&step["sourcePoint"])});
                let receipt = forks.thread_command(command);
                assert_eq!(receipt.status, "accepted", "{name}: merge back: {:?}", receipt.error);
            }
            other => panic!("{name}: unsupported step {other}"),
        }
    }
    for (alias, _) in forks.ids.clone() {
        forks.idle(&alias).await;
    }
    assert!(forks.mismatch().is_none(), "{name}: the adapter sent a frame the recording does not have: {:?}", forks.mismatch());
    for (alias, id) in forks.ids.clone() {
        let want = &row["threads"][&alias];
        let got = describe(&forks, &id);
        for key in want.as_object().unwrap().keys() {
            if key == "expected" {
                assert_projection(&format!("{name}: thread {alias}"), &got["expected"], &want["expected"]);
            } else {
                assert_eq!(got[key], want[key], "{name}: thread {alias}: {key}");
            }
        }
    }
    assert_eq!(forks.view_of(forks.id("source"))["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "fork").count(), row["steps"].as_array().unwrap().iter().filter(|step| step["type"] == "fork").count(), "{name}: one fork transfer per fork command");
    // The history injected into a native thread: its messages and the context that frames them.
    let injected: Vec<Value> = std::fs::read_to_string(&forks.inject_log).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let wanted = row["injections"].as_array().unwrap();
    assert_eq!(injected.len(), wanted.len(), "{name}: injections");
    let body = |text: &str| text.split_once("]\n").map_or(text.to_owned(), |(_, body)| body.to_owned());
    let frame = |context: &str| context.lines().filter(|line| !line.starts_with("Source item range:")).collect::<Vec<_>>().join("\n");
    for (got, want) in injected.iter().zip(wanted) {
        let items = got["items"].as_array().unwrap();
        assert_eq!(frame(items[0]["content"][0]["text"].as_str().unwrap()), frame(want["context"].as_str().unwrap()), "{name}: injected context");
        let messages = want["messages"].as_array().unwrap();
        assert_eq!(items.len() - 1, messages.len(), "{name}: injected messages");
        for (item, message) in items[1..].iter().zip(messages) {
            assert_eq!(item["role"], message["role"], "{name}: injected role");
            assert_eq!(body(item["content"][0]["text"].as_str().unwrap()), message["text"].as_str().unwrap(), "{name}: injected text");
        }
    }
    let _ = &forks.cwd;
    forks.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forks_and_merge_backs_persist_what_the_source_orchestrator_persisted() {
    let file: Value = serde_json::from_str(include_str!("fixtures/codex-replay.json")).unwrap();
    let rows = file["forks"].as_array().unwrap();
    assert_eq!(rows.len(), 6);
    let only = std::env::var("CODEX_REPLAY_ONLY").ok();
    let mut failures = vec![];
    for row in rows {
        if only.as_deref().is_some_and(|only| !only.split(',').any(|name| name == row["name"].as_str().unwrap())) {
            continue;
        }
        let outcome = tokio::spawn({
            let row = row.clone();
            async move { replay_forks(&row).await }
        })
        .await;
        if let Err(error) = outcome {
            let panic = error.into_panic();
            let message = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned())).unwrap_or_default();
            failures.push(format!("{}: {}", row["name"], message.chars().take(2500).collect::<String>()));
        }
    }
    assert!(failures.is_empty(), "{} fork scenario(s) differ:\n{}", failures.len(), failures.join("\n---\n"));
}
