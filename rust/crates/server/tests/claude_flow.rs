#![cfg(unix)]
//! The Claude adapter against a deterministic fake `claude` executable that speaks the stream-json
//! protocol (`fixtures/claude-provider.py`). Tests wait on persisted events, never on sleeps.
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

pub struct Harness {
    pub root: tempfile::TempDir,
    pub cwd: PathBuf,
    pub log: PathBuf,
    pub store: Store,
    pub providers: ProviderRegistry,
    pub execution: ExecutionService,
    pub thread: String,
    next: usize,
}

fn shim(cwd: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-provider.py");
    let binary = cwd.join("claude");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}
fn settings(binary: &std::path::Path, environment: Vec<(&str, String)>) -> t3_contracts::ServerSettings {
    let environment: Vec<Value> = environment.into_iter().map(|(name, value)| json!({"name":name,"value":value})).collect();
    serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","config":{"binaryPath":binary,"homePath":binary.parent().unwrap().join("claude-home")},"environment":environment}}})).unwrap()
}

impl Harness {
    pub async fn start(scenario: &str, runtime_mode: &str) -> Self {
        Self::start_with(vec![("CLAUDE_FIXTURE_SCENARIO", scenario.into())], runtime_mode, "claude-sonnet-4-6").await
    }
    pub async fn start_with(environment: Vec<(&str, String)>, runtime_mode: &str, model: &str) -> Self {
        Self::start_in(environment, runtime_mode, model, false).await
    }
    /// `worktree` runs the thread in its own git worktree, the workspace a checkpoint rollback needs.
    pub async fn start_in(environment: Vec<(&str, String)>, runtime_mode: &str, model: &str, worktree: bool) -> Self {
        Self::start_titled(environment, runtime_mode, model, worktree, "Claude").await
    }
    pub async fn start_titled(mut environment: Vec<(&str, String)>, runtime_mode: &str, model: &str, worktree: bool, title: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let log = cwd.join("fixture.log");
        environment.push(("CLAUDE_FIXTURE_LOG", log.to_string_lossy().into_owned()));
        let providers = ProviderRegistry::discover(&settings(&shim(&cwd), environment), &cwd).await.unwrap();
        let snapshot = providers.snapshots().into_iter().find(|row| row["instanceId"] == "claudeAgent").unwrap();
        assert_eq!(snapshot["status"], "ready", "{snapshot}");
        let store = Store::open(cwd.join("state.db")).unwrap();
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
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":project_root}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":title,"modelSelection":{"instanceId":"claudeAgent","model":model},"runtimeMode":runtime_mode,"interactionMode":"default","workspaceStrategy":strategy}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        Self { root, cwd: workspace, log, store, providers, execution, thread, next: 0 }
    }
    pub fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    pub fn send(&mut self, text: &str) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
        self.view()["runs"].as_array().unwrap().len()
    }
    /// Wait on persisted events until `check` yields a value.
    pub async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                // Lagged receivers re-check the projection, which is the source of truth.
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let view = self.view();
            let summary = |rows: &str, fields: &[&str]| view[rows].as_array().unwrap().iter().map(|row| fields.iter().map(|field| row[*field].to_string()).collect::<Vec<_>>().join("/")).collect::<Vec<_>>().join(" ");
            panic!("timed out waiting for a persisted event; runs: {}; items: {}; roster: {}; log: {}", summary("runs", &["status"]), summary("turnItems", &["type", "status"]), summary("providerThreads", &["pendingBackgroundTasks"]), view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "error").map(|item| item["failure"]["message"].to_string()).collect::<Vec<_>>().join(" | "))
        })
    }
    pub async fn settled(&self, ordinal: usize) -> Value {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await
    }
    pub async fn pending_request(&self) -> Value {
        self.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    pub fn items(&self, kind: &str) -> Vec<Value> {
        self.view()["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == kind).cloned().collect()
    }
    pub fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    pub fn launches(&self) -> Vec<Vec<String>> {
        self.log().iter().filter_map(|entry| entry["launch"].as_array()).map(|args| args.iter().map(|arg| arg.as_str().unwrap().to_owned()).collect()).filter(|args: &Vec<String>| !args.contains(&"--no-session-persistence".to_string())).collect()
    }
    pub fn answers_received(&self) -> Vec<Value> {
        self.log().into_iter().filter_map(|entry| entry.get("received").cloned()).filter(|message| message["type"] == "control_response").map(|message| message["response"]["response"].clone()).collect()
    }
    pub async fn stop(self) {
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
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|arg| arg == name).and_then(|at| args.get(at + 1)).cloned()
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_reports_version_account_models_and_commands_without_a_model_request() {
    let harness = Harness::start("turn", "approval-required").await;
    let snapshot = harness.providers.snapshots().into_iter().find(|row| row["instanceId"] == "claudeAgent").unwrap();
    assert_eq!((snapshot["driver"].as_str(), snapshot["installed"].clone(), snapshot["version"].as_str()), (Some("claudeAgent"), json!(true), Some("2.1.111")));
    assert_eq!(snapshot["auth"], json!({"status":"authenticated","email":"dev@example.test","type":"max","label":"Claude Max Subscription"}));
    let slugs: Vec<&str> = snapshot["models"].as_array().unwrap().iter().map(|model| model["slug"].as_str().unwrap()).collect();
    assert!(slugs.contains(&"claude-sonnet-4-6") && !slugs.contains(&"claude-opus-5-5"), "2.1.111 predates the newest models: {slugs:?}");
    assert!(snapshot["message"].as_str().unwrap().contains("too old for"), "{snapshot}");
    let commands: Vec<&str> = snapshot["slashCommands"].as_array().unwrap().iter().map(|command| command["name"].as_str().unwrap()).collect();
    assert_eq!(commands, ["compact", "review"]);
    // The probe is the SDK's health probe: no session file, no hooks, no MCP servers, and no prompt.
    let probe: Vec<Vec<String>> = harness.log().iter().filter_map(|entry| entry["launch"].as_array()).map(|args| args.iter().map(|arg| arg.as_str().unwrap().to_owned()).collect()).collect();
    assert_eq!(probe.len(), 1);
    assert!(probe[0].contains(&"--no-session-persistence".to_string()) && probe[0].contains(&"--strict-mcp-config".to_string()));
    assert!(harness.log().iter().all(|entry| entry["received"]["type"] != "user"));
    harness.stop().await;

    // A binary that is missing is reported, not launched.
    let settings = settings(std::path::Path::new("/nonexistent/claude"), vec![]);
    let providers = ProviderRegistry::discover(&settings, std::path::Path::new("/tmp")).await.unwrap();
    let missing = providers.snapshots().into_iter().find(|row| row["instanceId"] == "claudeAgent").unwrap();
    assert_eq!((missing["status"].as_str(), missing["installed"].clone()), (Some("error"), json!(false)));
    assert!(missing["message"].as_str().unwrap().contains("not found on PATH"));
    assert!(providers.claude("claudeAgent").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_persists_reasoning_text_tools_todos_and_usage_and_the_next_turn_resumes_the_same_process() {
    let mut harness = Harness::start("turn", "approval-required").await;
    harness.send("first");
    assert_eq!(harness.settled(1).await["status"], "completed");
    let view = harness.view();
    let kinds: Vec<(&str, &str)> = view["turnItems"].as_array().unwrap().iter().map(|item| (item["type"].as_str().unwrap(), item["status"].as_str().unwrap())).collect();
    for expected in [("user_message", "completed"), ("reasoning", "completed"), ("assistant_message", "completed"), ("command_execution", "completed"), ("todo_list", "completed")] {
        assert!(kinds.contains(&expected), "{expected:?} missing from {kinds:?}");
    }
    let reasoning = &harness.items("reasoning")[0];
    assert_eq!((reasoning["text"].as_str(), reasoning["streaming"].clone()), (Some("Considering the request."), json!(false)));
    let command = &harness.items("command_execution")[0];
    assert_eq!((command["input"].as_str(), command["output"].as_str()), (Some("echo hi"), Some("hi\n")));
    let todos = &harness.items("todo_list")[0];
    assert_eq!(todos["steps"][1], json!({"id":"todo-1","text":"Test it","status":"running"}));
    let texts: Vec<String> = harness.items("assistant_message").iter().map(|item| item["text"].as_str().unwrap().to_owned()).collect();
    assert_eq!(texts, ["Working on: first", "done"]);
    let usage = &view["providerTurns"][0]["turnTokenUsage"];
    assert_eq!((usage["usageStatus"].as_str(), usage["inputTokens"].as_u64(), usage["outputTokens"].as_u64()), (Some("complete"), Some(153), Some(7)));
    assert_eq!(view["providerTurns"][0]["tokenUsage"]["usedTokens"], 160);
    assert_eq!(view["providerSessions"][0]["capabilities"]["turns"]["supportsInterrupt"], true);
    // The turn ends through the shared terminal path, which records the checkpoint capture.
    assert!(harness.store.effect(&format!("effect:checkpoint.capture:{}", view["runs"][0]["id"].as_str().unwrap())).unwrap().is_some());

    harness.send("second");
    assert_eq!(harness.settled(2).await["status"], "completed");
    let launches = harness.launches();
    assert_eq!(launches.len(), 1, "the live process serves both turns: {launches:?}");
    let args = &launches[0];
    assert_eq!(flag(args, "--model").as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(flag(args, "--permission-mode").as_deref(), Some("default"));
    assert!(args.windows(2).any(|pair| pair == ["--permission-prompt-tool", "stdio"]));
    assert!(args.iter().any(|arg| arg.starts_with("--session-id=")));
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_actor_resumes_the_native_session_and_a_model_change_reopens_it() {
    let mut harness = Harness::start("turn", "approval-required").await;
    harness.send("one");
    harness.settled(1).await;
    let native = harness.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    harness.execution.dispatch(&json!({"type":"message.dispatch","commandId":"two","threadId":harness.thread,"messageId":"two","text":"two","attachments":[],"modelSelection":{"instanceId":"claudeAgent","model":"claude-haiku-4-5"},"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    harness.settled(2).await;
    let launches = harness.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(flag(&launches[0], "--model").as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(flag(&launches[1], "--model").as_deref(), Some("claude-haiku-4-5"));
    assert!(launches[1].contains(&format!("--resume={native}")), "{:?}", launches[1]);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn approvals_follow_the_runtime_mode_policy_and_persisted_decisions_reach_the_cli() {
    // Supervised: the prompt is a persisted approval and the decision is the CLI's answer.
    let mut harness = Harness::start("approval", "approval-required").await;
    harness.send("write a file");
    let request = harness.pending_request().await;
    assert_eq!(request["kind"], "file-change");
    let item = harness.items("approval_request").remove(0);
    assert_eq!((item["status"].as_str(), item["prompt"].as_str()), (Some("waiting"), Some("Claude wants to write /tmp/fixture.txt")));
    assert_eq!(request["responseCapability"]["type"], "live");
    harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"approve","threadId":harness.thread,"requestId":request["id"],"decision":"acceptForSession"}), Utc::now()).unwrap();
    assert_eq!(harness.settled(1).await["status"], "completed");
    let answer = &harness.answers_received()[0];
    assert_eq!((answer["behavior"].as_str(), answer["toolUseID"].as_str(), answer["decisionClassification"].as_str()), (Some("allow"), Some("toolu_w"), Some("user_permanent")));
    assert_eq!(answer["updatedPermissions"][0]["destination"], "session");
    assert_eq!(harness.view()["runtimeRequests"][0]["status"], "resolved");
    harness.stop().await;

    // Declining sends a denial the model can read.
    let mut harness = Harness::start("approval", "auto-accept-edits").await;
    harness.send("write a file");
    let request = harness.pending_request().await;
    harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"decline","threadId":harness.thread,"requestId":request["id"],"decision":"decline"}), Utc::now()).unwrap();
    assert_eq!(harness.settled(1).await["status"], "completed");
    let answer = &harness.answers_received()[0];
    assert_eq!((answer["behavior"].as_str(), answer["message"].as_str(), answer["decisionClassification"].as_str()), (Some("deny"), Some("User declined tool execution."), Some("user_reject")));
    assert!(harness.items("assistant_message").iter().any(|item| item["text"] == "declined: User declined tool execution."));
    harness.stop().await;

    // Full access and auto never prompt: the callback allows at once.
    for mode in ["full-access", "auto"] {
        let mut harness = Harness::start("approval", mode).await;
        harness.send("write a file");
        assert_eq!(harness.settled(1).await["status"], "completed");
        assert!(harness.view()["runtimeRequests"].as_array().unwrap().is_empty(), "{mode}");
        assert_eq!(harness.answers_received()[0]["behavior"], "allow");
        let args = &harness.launches()[0];
        assert_eq!(flag(args, "--permission-mode").as_deref(), Some(if mode == "auto" { "auto" } else { "bypassPermissions" }));
        assert_eq!(args.contains(&"--allow-dangerously-skip-permissions".to_string()), mode == "full-access");
        harness.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_structured_question_is_answered_from_a_persisted_response() {
    let mut harness = Harness::start("question", "approval-required").await;
    harness.send("choose");
    let request = harness.pending_request().await;
    assert_eq!(request["kind"], "user_input");
    let item = harness.items("user_input_request").remove(0);
    assert_eq!(item["questions"][0]["question"], "Which database?");
    assert_eq!(item["questions"][0]["options"][1], json!({"label":"SQLite","description":"Embedded"}));
    let question_id = item["questions"][0]["id"].clone();
    harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"answer","threadId":harness.thread,"requestId":request["id"],"answers":{question_id.as_str().unwrap():"SQLite"}}), Utc::now()).unwrap();
    assert_eq!(harness.settled(1).await["status"], "completed");
    let answer = &harness.answers_received()[0];
    assert_eq!(answer["behavior"], "allow");
    assert_eq!(answer["updatedInput"]["answers"], json!({"Which database?":"SQLite"}));
    assert_eq!(answer["updatedInput"]["questions"][0]["header"], "Database");
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_closes_the_process_and_the_next_turn_resumes_the_conversation() {
    let mut harness = Harness::start("hang", "approval-required").await;
    harness.send("run forever");
    harness.until(|view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").cloned()).await;
    let run = harness.view()["runs"][0]["id"].clone();
    harness.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run}), Utc::now()).unwrap();
    assert_eq!(harness.settled(1).await["status"], "interrupted");
    let view = harness.view();
    assert_eq!(view["providerTurns"][0]["status"], "interrupted");
    assert!(view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "run_interrupt_result"));
    let tool = harness.items("command_execution").remove(0);
    assert_eq!(tool["status"], "interrupted");
    let requested: Vec<Value> = harness.log().into_iter().filter_map(|entry| entry.get("received").cloned()).filter(|message| message["request"]["subtype"] == "interrupt").collect();
    assert_eq!(requested.len(), 1, "the interrupt control request was sent");
    harness.send("again");
    // The second run's own tool call proves its process started and logged its launch.
    harness.until(|view| { let run = view["runs"].get(1)?["id"].clone(); view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running" && item["runId"] == run).cloned() }).await;
    let launches = harness.launches();
    assert_eq!(launches.len(), 2);
    assert!(launches[1].iter().any(|arg| arg.starts_with("--resume=")), "{:?}", launches[1]);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_during_a_turn_steers_it_natively() {
    let mut harness = Harness::start("steer", "approval-required").await;
    harness.send("start");
    harness.until(|view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "assistant_message" && item["text"] == "started").cloned()).await;
    harness.execution.dispatch(&json!({"type":"message.dispatch","commandId":"steer","threadId":harness.thread,"messageId":"steer","text":"change course","attachments":[],"dispatchMode":{"type":"steer_active","targetRunId":harness.view()["runs"][0]["id"]}}), Utc::now()).unwrap();
    assert_eq!(harness.settled(1).await["status"], "completed");
    assert!(harness.items("assistant_message").iter().any(|item| item["text"] == "steered: change course"));
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dying_process_fails_the_run_with_its_exit_status_and_a_failed_result_reports_the_api_error() {
    let mut harness = Harness::start("crash", "approval-required").await;
    harness.send("crash");
    assert_eq!(harness.settled(1).await["status"], "failed");
    let error = harness.items("error").remove(0);
    let message = error["failure"]["message"].as_str().unwrap();
    assert!(message.contains("exited with code 3") && message.contains("fixture crash"), "{message}");
    harness.stop().await;

    let mut harness = Harness::start("error", "approval-required").await;
    harness.send("limited");
    assert_eq!(harness.settled(1).await["status"], "failed");
    let failure = harness.items("error").remove(0)["failure"].clone();
    assert_eq!((failure["class"].as_str(), failure["code"].as_str(), failure["retryable"].clone()), (Some("usage_limit"), Some("api_error_429"), json!(true)));
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_restart_ends_the_running_turn_and_the_next_turn_resumes_the_native_session() {
    let mut harness = Harness::start("hang", "approval-required").await;
    harness.send("run forever");
    harness.until(|view| view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running").cloned()).await;
    let native = harness.view()["providerThreads"][0]["nativeThreadRef"]["nativeId"].as_str().unwrap().to_owned();
    let Harness { root, cwd, log, store, providers, execution, thread, .. } = harness;
    execution.shutdown().await;
    drop(execution);
    drop(store);
    let store = Store::open(cwd.join("state.db")).unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let view = store.projection("thread", &thread).unwrap().unwrap();
    // Shutdown interrupts the turn; the stopped session cannot be answered or reused.
    assert_eq!(view["runs"][0]["status"], "interrupted");
    assert!(view["providerSessions"].as_array().unwrap().iter().all(|session| session["status"] == "stopped"));
    let mut harness = Harness { root, cwd, log, store, providers, execution, thread, next: 100 };
    harness.send("after restart");
    // The second run's own tool call proves its process started and logged its launch.
    harness.until(|view| { let run = view["runs"].get(1)?["id"].clone(); view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "command_execution" && item["status"] == "running" && item["runId"] == run).cloned() }).await;
    let launches = harness.launches();
    assert!(launches.last().unwrap().contains(&format!("--resume={native}")), "{launches:?}");
    harness.stop().await;
}

// --- replay parity ---------------------------------------------------------------------------
// The recordings are the Claude Agent SDK message streams the source adapter was recorded
// against; `fixtures/claude-replay.json` holds the user-visible projection the unchanged
// TypeScript orchestrator and adapter produced for each (see tools/generate_claude_replay_fixtures.mjs).
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

fn finished(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"))
}

/// Every thread the scenario created, the root first, as the original's projections list them:
/// each thread's own projection, how it relates to its parent, and the subagents it holds.
fn assert_threads(name: &str, harness: &Harness, expected: &[Value]) {
    let mut found: Vec<Value> = vec![harness.view()];
    let mut index = 0;
    while index < found.len() {
        let children: Vec<String> = found[index]["subagents"].as_array().unwrap().iter().filter_map(|task| task["childThreadId"].as_str().map(str::to_owned)).collect();
        for child in children {
            if !found.iter().any(|view| view["thread"]["id"] == child.as_str()) {
                found.push(harness.store.projection("thread", &child).unwrap().unwrap_or_else(|| panic!("{name}: child thread {child} is missing")));
            }
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
        // The projection lists subagents by start time, then id. The original's recordings replay on a
        // frozen clock, where the id decides; here starts differ, so the rows are compared as a set.
        let by_child = |rows: &Value| {
            let mut rows = rows.as_array().unwrap().clone();
            rows.sort_by_key(|row| row["child"].as_str().unwrap_or_default().to_owned());
            Value::Array(rows)
        };
        assert_eq!(by_child(&Value::Array(subagents)), by_child(&want["subagents"]), "{label}: subagents");
        assert_item_rows(&label, &normalize(view), &want["expected"]);
    }
}
fn assert_item_rows(name: &str, actual: &Value, expected: &Value) {
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

async fn replay(row: &Value) {
    let name = row["name"].as_str().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("transcript.ndjson");
    std::fs::write(&transcript, row["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
    let mut environment = vec![("CLAUDE_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned()), ("CLAUDE_FIXTURE_STATE", scratch.path().join("cursor").to_string_lossy().into_owned())];
    let steps = row["steps"].as_array().unwrap();
    // A step that releases a replay gate holds the recording back at the frame with that label until then.
    let mut gates = serde_json::Map::new();
    for step in steps.iter().filter(|step| matches!(step["type"].as_str(), Some("release_replay_gate" | "release_replay_gate_after_waiting"))) {
        let fifo = scratch.path().join(format!("gate-{}.fifo", gates.len()));
        let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        gates.insert(step["label"].as_str().unwrap().to_owned(), json!(fifo));
    }
    if !gates.is_empty() {
        environment.push(("CLAUDE_FIXTURE_GATES", Value::Object(gates.clone()).to_string()));
    }
    let release = |label: &str| {
        use std::io::Write;
        std::fs::OpenOptions::new().write(true).open(gates[label].as_str().unwrap()).unwrap().write_all(b"release").unwrap();
    };
    let rolls_back = steps.iter().any(|step| step["type"] == "rollback");
    let mut harness = Harness::start_titled(environment, "approval-required", row["model"].as_str().unwrap(), rolls_back, &format!("Replay fixture: {name}")).await;
    let mut runs = 0;
    for (position, step) in steps.iter().enumerate() {
        if std::env::var("CLAUDE_REPLAY_TRACE").is_ok() {
            eprintln!("{name}: step {position} {}", step["type"]);
        }
        match step["type"].as_str().unwrap() {
            "message" => {
                runs = harness.send(step["text"].as_str().unwrap());
            }
            "queue_message" => {
                harness.next += 1;
                let id = format!("message-{}", harness.next);
                harness.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":harness.thread,"messageId":id,"text":step["text"],"attachments":[],"dispatchMode":{"type":"queue_after_active"}}), Utc::now()).unwrap();
                runs = harness.view()["runs"].as_array().unwrap().len();
            }
            "steer" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                let run = harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == "running").cloned()).await;
                harness.next += 1;
                let id = format!("message-{}", harness.next);
                harness.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":harness.thread,"messageId":id,"text":step["text"],"attachments":[],"dispatchMode":{"type":"steer_active","targetRunId":run["id"]}}), Utc::now()).unwrap();
            }
            "approve_next_runtime_request" => {
                let request = harness.pending_request().await;
                harness.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"approve","threadId":harness.thread,"requestId":request["id"],"decision":step.get("decision").cloned().unwrap_or(json!("accept"))}), Utc::now()).unwrap();
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
            "await_run_status" => {
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                harness.until(|view| view["runs"].get(index).filter(|run| run["status"] == step["status"]).map(|_| ())).await;
                if let Some(kind) = step.get("waitForTurnItemType").and_then(Value::as_str) {
                    harness.until(|view| {
                        let run = view["runs"].get(index)?["id"].clone();
                        view["turnItems"].as_array().unwrap().iter().any(|item| item["runId"] == run && item["type"] == kind).then_some(())
                    })
                    .await;
                }
            }
            "release_replay_gate" => release(step["label"].as_str().unwrap()),
            "release_replay_gate_after_waiting" => {
                // The run settles with a background task on the roster; the held frame ends the roster.
                let index = step["targetRunIndex"].as_u64().unwrap() as usize - 1;
                harness.until(|view| {
                    let run = view["runs"].get(index).filter(|run| run["status"] == "completed")?;
                    view["providerThreads"].as_array().unwrap().iter().find(|thread| thread["id"] == run["providerThreadId"]).filter(|thread| !thread["pendingBackgroundTasks"].as_array().unwrap().is_empty()).map(|_| ())
                })
                .await;
                release(step["label"].as_str().unwrap());
                harness.until(|view| view["providerThreads"].as_array().unwrap().iter().all(|thread| thread["pendingBackgroundTasks"].as_array().unwrap().is_empty()).then_some(())).await;
            }
            "interrupt" => {
                // The recording interrupts after the model started a tool when it had emitted one.
                let entries = row["transcript"].as_array().unwrap();
                let at = entries.iter().position(|entry| entry["frame"]["type"] == "query.interrupt").unwrap();
                // The recording interrupts after the model started its last tool: wait until that tool shows.
                let last_tool = entries[..at].iter().rev().filter(|entry| entry["frame"]["parent_tool_use_id"].is_null()).find_map(|entry| entry["frame"]["message"]["content"].as_array().and_then(|content| content.iter().rev().find(|block| block["type"] == "tool_use" && block["name"] != "Agent")).map(|block| block["id"].clone()));
                if let Some(id) = last_tool {
                    harness.until(|view| view["turnItems"].as_array().unwrap().iter().find(|item| item["nativeItemRef"]["nativeId"] == id).cloned()).await;
                }
                let run = harness.until(|view| view["runs"].get(step["targetRunIndex"].as_u64().unwrap() as usize - 1).filter(|run| run["status"] == "running").cloned()).await;
                harness.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run["id"]}), Utc::now()).unwrap();
            }
            // The Rust process is not released when idle, so there is no clock to advance.
            "advance_clock" => {}
            other => panic!("{name}: unsupported step {other}"),
        }
        // A following message or rollback waits for the thread to be idle, as the original's does.
        if steps.get(position + 1).is_some_and(|next| matches!(next["type"].as_str(), Some("message" | "rollback" | "advance_clock"))) {
            harness.until(|view| view["runs"].as_array().unwrap().iter().all(finished).then_some(())).await;
        }
    }
    harness.settled(runs).await;
    // Everything after the terminal (checkpoint capture) settles through the same persisted state.
    harness.until(|view| view["runs"].as_array().unwrap().iter().all(finished).then_some(())).await;
    let actual = normalize(&harness.view());
    let expected = &row["expected"];
    assert_item_rows(name, &actual, expected);
    // A subagent's child threads are threads of their own, found by their lineage.
    if let Some(expected_threads) = row.get("threads").and_then(Value::as_array) {
        assert_threads(name, &harness, expected_threads);
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_sdk_streams_persist_what_the_source_adapter_persisted() {
    let file: Value = serde_json::from_str(include_str!("fixtures/claude-replay.json")).unwrap();
    let rows = file["scenarios"].as_array().unwrap();
    assert!(rows.len() >= 20);
    // `CLAUDE_REPLAY_ONLY=a,b` narrows a debugging run.
    let only = std::env::var("CLAUDE_REPLAY_ONLY").ok();
    let mut failures = vec![];
    for row in rows {
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

/// Every recording of the original is replayed or named here with the reason it is not.
#[test]
fn every_excluded_recording_names_why_the_port_does_not_replay_it() {
    let file: Value = serde_json::from_str(include_str!("fixtures/claude-replay.json")).unwrap();
    let (scenarios, excluded) = (file["scenarios"].as_array().unwrap(), file["excluded"].as_array().unwrap());
    for entry in excluded {
        assert!(entry["reason"].as_str().is_some_and(|reason| reason.len() > 20), "{entry}");
        assert!(!scenarios.iter().any(|row| row["name"] == entry["name"]), "{entry} is both replayed and excluded");
    }
    let (recovery, forks) = (file["recovery"].as_array().unwrap(), file["forks"].as_array().unwrap());
    // The forks and merge-backs are replayed over several threads by `claude_fork_replay.rs`.
    for entry in forks {
        assert!(!excluded.iter().any(|row| row["name"] == entry["name"]), "{entry} is both replayed as a fork and excluded");
    }
    assert_eq!(scenarios.len() + recovery.len() + forks.len() + excluded.len(), file["recorded"].as_u64().unwrap() as usize, "every recording is accounted for");
}

/// The recording of a subagent launched, finished, and resumed by `SendMessage` after the server
/// restarted (the original replays it as two runtimes over one database). The resumed subagent keeps
/// its child thread: the launch call is found in the CLI's session storage, the new prompt is appended
/// to the thread, and nothing the subagent says reaches the parent.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_resumed_after_a_server_restart_keeps_its_own_thread() {
    let file: Value = serde_json::from_str(include_str!("fixtures/claude-replay.json")).unwrap();
    let row = &file["recovery"][0];
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("transcript.ndjson");
    std::fs::write(&transcript, row["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
    let environment = vec![("CLAUDE_FIXTURE_TRANSCRIPT", transcript.to_string_lossy().into_owned()), ("CLAUDE_FIXTURE_STATE", scratch.path().join("cursor").to_string_lossy().into_owned())];
    let mut harness = Harness::start_titled(environment, "approval-required", row["model"].as_str().unwrap(), false, "Subagent resume").await;
    let prompts = row["prompts"].as_array().unwrap();
    let runs = harness.send(prompts[0].as_str().unwrap());
    harness.settled(runs).await;
    harness.until(|view| view["runs"].as_array().unwrap().iter().all(finished).then_some(())).await;
    // The server stops and starts again over the same state: the process, the registry of subagents and
    // the running turn are gone, the conversation and the subagent's thread are not.
    harness.execution.shutdown().await;
    harness.execution = ExecutionService::start(harness.store.clone(), harness.providers.clone());
    let runs = harness.send(prompts[1].as_str().unwrap());
    harness.settled(runs).await;
    harness.until(|view| view["runs"].as_array().unwrap().iter().all(finished).then_some(())).await;

    let view = harness.view();
    let tasks = view["subagents"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "the resumed subagent is the one launched before");
    assert_eq!(tasks[0]["status"], "completed");
    let child = harness.store.projection("thread", tasks[0]["childThreadId"].as_str().unwrap()).unwrap().unwrap();
    let mut items: Vec<&Value> = child["turnItems"].as_array().unwrap().iter().collect();
    items.sort_by_key(|item| item["ordinal"].as_u64());
    let conversation: Vec<(String, String)> = items.iter().filter(|item| matches!(item["type"].as_str(), Some("user_message" | "assistant_message"))).map(|item| (item["type"].as_str().unwrap().to_owned(), item["text"].as_str().unwrap().to_owned())).collect();
    assert_eq!(conversation.iter().map(|(kind, _)| kind.as_str()).collect::<Vec<_>>(), ["user_message", "assistant_message", "user_message", "assistant_message"]);
    assert!(conversation[0].1.contains("Your job is just the file"), "{}", conversation[0].1);
    assert!(conversation[1].1.contains("is 1 line long"));
    assert!(conversation[2].1.contains("Look again at"));
    assert!(conversation[3].1.contains("Bug/edge case found and fixed"));
    // The resumed run's own tool calls land in the child thread too.
    assert_eq!(items.iter().filter(|item| item["type"] == "command_execution").count(), 3);
    assert!(
        !view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "assistant_message" && item["text"].as_str().is_some_and(|text| text.contains("Bug/edge case found"))),
        "the resumed subagent's reply leaked into the parent thread"
    );
    harness.stop().await;
}
