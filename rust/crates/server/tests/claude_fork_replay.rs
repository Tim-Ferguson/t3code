#![cfg(unix)]
//! Thread forks and merge-backs of the Claude adapter, replayed from the original's recordings.
//! `forks` in `fixtures/claude-replay.json` holds, for each recorded Claude Agent SDK transcript of a
//! fork or merge-back, what the unchanged TypeScript orchestrator and `ClaudeAdapterV2` persisted for
//! every thread (see `tools/generate_claude_replay_fixtures.mjs`). The fake `claude`
//! (`fixtures/claude-provider.py`) serves the one recording to every `claude` process the port starts:
//! the original forked a session file in a call of its own, the port asks the CLI to fork in the launch
//! of the fork's first turn (`--fork-session`), so the fake passes over the recorded fork call and
//! answers the launch with the fork's session. Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry, thread::ThreadService};

fn shim(cwd: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-provider.py");
    let binary = cwd.join("claude");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
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

fn assert_projection(name: &str, actual: &Value, expected: &Value) {
    let types = |items: &Value| items.as_array().unwrap().iter().map(|item| format!("{}:{}", item["type"].as_str().unwrap(), item["status"].as_str().unwrap())).collect::<Vec<_>>().join(" ");
    assert_eq!(types(&actual["items"]), types(&expected["items"]), "{name}: the items, in order");
    for (index, (got, want)) in actual["items"].as_array().unwrap().iter().zip(expected["items"].as_array().unwrap()).enumerate() {
        for key in got.as_object().unwrap().keys().chain(want.as_object().unwrap().keys()) {
            let short = |value: &Value| value.to_string().chars().take(300).collect::<String>();
            assert!(got.get(key) == want.get(key), "{name}: item {index} ({}) differs at {key}\n  rust: {}\n  ts:   {}", want["type"], short(&got[key]), short(&want[key]));
        }
    }
    assert_eq!(actual, expected, "{name}");
}


struct Forks {
    cwd: PathBuf,
    log: PathBuf,
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
        let log = cwd.join("fixture.log");
        let workspace = cwd.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("README.md"), "# thread fork\n").unwrap();
        git(&workspace, &["init", "-q"]);
        git(&workspace, &["add", "."]);
        git(&workspace, &["commit", "-q", "-m", "initial"]);
        let environment: Vec<Value> = [
            ("CLAUDE_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned()),
            ("CLAUDE_FIXTURE_STATE", cwd.join("cursor").to_string_lossy().into_owned()),
            ("CLAUDE_FIXTURE_LOG", log.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .map(|(name, value)| json!({"name":name,"value":value}))
        .collect();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"responseStreamingMode":"turn","providerInstances":{
            "codex":{"driver":"codex","enabled":false},
            "claudeAgent":{"driver":"claudeAgent","enabled":true,"config":{"binaryPath":shim(&cwd),"homePath":cwd.join("claude-home")},"environment":environment}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":workspace}), Utc::now()).unwrap();
        let source = row["threadIds"]["source"].as_str().unwrap();
        ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","threadId":source,"projectId":"project","title":row["sourceTitle"],"modelSelection":{"instanceId":"claudeAgent","model":row["model"]},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap();
        let execution = ExecutionService::start(store.clone(), providers);
        let ids = row["threadIds"].as_object().unwrap().iter().map(|(alias, id)| (alias.clone(), id.as_str().unwrap().to_owned())).collect();
        Self { cwd, log, store, execution, ids, sent: Default::default(), next: 0, _root: scratch }
    }
    fn id(&self, alias: &str) -> &str {
        &self.ids.iter().find(|(name, _)| name == alias).unwrap_or_else(|| panic!("no thread {alias}")).1
    }
    fn view_of(&self, id: &str) -> Value {
        t3_server::rpc_response::checked_thread(self.store.projection("thread", id).unwrap().unwrap_or_else(|| panic!("thread {id} does not exist")))
    }
    /// The arguments of every `claude` the port started, in order.
    fn launches(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|entry| entry["launch"].as_array().map(|args| args.iter().map(|arg| arg.as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>()))
            .filter(|args| !args.contains(&"--no-session-persistence".to_owned()))
            .collect()
    }
    async fn until_view(&self, id: &str, mut check: impl FnMut(&Value) -> bool) -> Value {
        let mut events = self.store.subscribe();
        let waited = tokio::time::timeout(HANG_BOUND, async {
            loop {
                let view = self.view_of(id);
                if check(&view) {
                    return view;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        waited.unwrap_or_else(|_| {
            let view = self.view_of(id);
            panic!("timed out waiting on {id}; launches: {:?}; runs {:?}", self.launches(), view["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>())
        })
    }
    async fn until(&self, id: &str, check: impl FnMut(&Value) -> bool) {
        self.until_view(id, check).await;
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

fn replace_string(value: Value, from: &str, to: &str) -> Value {
    match value {
        Value::String(text) if text == from => Value::String(to.to_owned()),
        Value::Array(items) => Value::Array(items.into_iter().map(|item| replace_string(item, from, to)).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(key, item)| (key, replace_string(item, from, to))).collect()),
        other => other,
    }
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
            "rollback" => {
                // The checkpoint the step names is the one after that run of the thread's root scope.
                let alias = step["thread"].as_str().unwrap().to_owned();
                let (thread, ordinal) = (forks.id(&alias).to_owned(), step["ordinal"].as_u64().unwrap());
                let view = forks.until_view(&thread, |view| view["checkpoints"].as_array().unwrap().iter().any(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal && checkpoint["status"] == "ready")).await;
                let target = view["checkpoints"].as_array().unwrap().iter().find(|checkpoint| checkpoint["ordinalWithinScope"] == ordinal && checkpoint["status"] == "ready").unwrap()["id"].clone();
                forks.next += 1;
                forks.execution.dispatch(&json!({"type":"checkpoint.rollback","commandId":format!("rewind-{}",forks.next),"threadId":thread,"scopeId":view["checkpointScopes"][0]["id"],"checkpointId":target,"restoreFiles":false}), Utc::now()).unwrap();
                let rolled = forks.until_view(&thread, |view| view["thread"]["rollbackFailure"].is_object() || view["runs"].as_array().unwrap().iter().any(|run| run["status"] == "rolled_back")).await;
                assert!(rolled["thread"]["rollbackFailure"].is_null(), "{name}: {}", rolled["thread"]["rollbackFailure"]);
            }
            other => panic!("{name}: unsupported step {other}"),
        }
    }
    for (alias, _) in forks.ids.clone() {
        forks.idle(&alias).await;
    }
    // The source session was named by the port (`--session-id`); the recording names the one the original
    // chose. They stand for the same session, so the recorded name is put in the port's place.
    let source_session = forks.view_of(forks.id("source"))["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    let recorded_session = row["transcript"][0]["metadata"]["nativeSessionId"].as_str().unwrap().to_owned();
    for (alias, id) in forks.ids.clone() {
        let want = &row["threads"][&alias];
        let got = replace_string(describe(&forks, &id), &source_session, &recorded_session);
        for key in want.as_object().unwrap().keys() {
            if key == "expected" {
                assert_projection(&format!("{name}: thread {alias}"), &got["expected"], &want["expected"]);
            } else {
                assert_eq!(got[key], want[key], "{name}: thread {alias}: {key}");
            }
        }
    }
    assert_eq!(forks.view_of(forks.id("source"))["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "fork").count(), row["steps"].as_array().unwrap().iter().filter(|step| step["type"] == "fork").count(), "{name}: one fork transfer per fork command");
    // Each fork the original made through the SDK, the port made in a launch: it resumed the same session
    // and was cut after the same assistant message.
    let recorded_forks: Vec<&Value> = row["transcript"].as_array().unwrap().iter().filter(|entry| entry["frame"]["type"] == "session.fork").collect();
    let forking: Vec<Vec<String>> = forks.launches().into_iter().filter(|args| args.contains(&"--fork-session".to_owned())).collect();
    assert_eq!(forking.len(), recorded_forks.len(), "{name}: forks launched");
    for (args, recorded) in forking.iter().zip(recorded_forks) {
        assert!(args.contains(&format!("--resume={source_session}")), "{name}: {args:?}");
        assert!(args.contains(&format!("--resume-session-at={}", recorded["frame"]["options"]["upToMessageId"].as_str().unwrap())), "{name}: {args:?}");
    }
    // Claude takes no native history injection: every handoff reaches it in its message.
    assert!(row["injections"].as_array().unwrap().is_empty(), "{name}");
    let _ = &forks.cwd;
    forks.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forks_and_merge_backs_persist_what_the_source_orchestrator_persisted() {
    let file: Value = serde_json::from_str(include_str!("fixtures/claude-replay.json")).unwrap();
    let rows = file["forks"].as_array().unwrap();
    assert_eq!(rows.len(), 7);
    let only = std::env::var("CLAUDE_REPLAY_ONLY").ok();
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

