#![cfg(unix)]
//! The Codex adapter against the scripted fake app-server (`fixtures/codex-provider.py`) over the
//! real WebSocket transport: every approval kind with each decision, a request Codex withdraws,
//! what the runtime mode lets Codex do without asking, and reasoning, plan, todo and usage reaching
//! the projection. The replay of the original's recordings is in `codex_replay.rs`. Tests wait on
//! persisted events, never on sleeps.
use chrono::Utc;
use futures_util::{FutureExt, SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::Store,
    project::ProjectService,
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    thread: String,
    socket: Socket,
    responses: std::path::PathBuf,
    requests: std::path::PathBuf,
    next: u64,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness(runtime_mode: &str, interaction_mode: &str) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let responses = cwd.join("codex-responses.jsonl");
    let requests = cwd.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RESPONSE_RECORD","value":responses},{"name":"FIXTURE_RPC_RECORD","value":requests}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_codex".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"codex-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: Some(providers),
        execution: Some(execution.clone()),
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {operator}").parse().unwrap());
    let socket = tokio_tungstenite::connect_async(request).await.unwrap().0;
    Harness { _directory: directory, store, execution, thread, socket, responses, requests, next: 0, stop: Some(stop), server: Some(server) }
}

impl Harness {
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    /// One command over the socket, as a client sends it.
    fn command(&mut self, command: Value) -> BoxFuture<'_, Value> {
        self.next += 1;
        let id = self.next;
        Box::pin(async move {
            self.socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"orchestration.dispatchCommand","payload":command,"headers":[]}).to_string().into())).await.unwrap();
            loop {
                let message = self.socket.next().await.unwrap().unwrap();
                let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                if value["_tag"] == "Exit" && value["requestId"] == id {
                    assert_eq!(value["exit"]["_tag"], "Success", "{value}");
                    return value["exit"]["value"].clone();
                }
            }
        })
    }
    fn send<'a>(&'a mut self, text: &'a str, extra: Value) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let id = format!("message-{}", self.next + 1);
            let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}});
            for (key, value) in extra.as_object().unwrap() {
                command[key] = value.clone();
            }
            self.command(command).await;
        })
    }
    fn respond<'a>(&'a mut self, request: &'a Value, answer: Value) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut command = json!({"type":"runtime-request.respond","commandId":format!("answer-{}", self.next + 1),"threadId":self.thread,"requestId":request["id"]});
            for (key, value) in answer.as_object().unwrap() {
                command[key] = value.clone();
            }
            self.command(command).await;
        })
    }
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a persisted event; runs: {} items: {:?}", self.view()["runs"], self.view()["turnItems"].as_array().unwrap().iter().map(|item| format!("{}:{}", item["type"], item["status"])).collect::<Vec<_>>()))
    }
    /// The run is terminal and so is everything the checkpoint effect does after it.
    async fn settled(&self, runs: usize) -> Value {
        self.until(|view| (view["runs"].as_array().unwrap().len() >= runs && view["runs"].as_array().unwrap().iter().all(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled")))).then(|| view.clone())).await
    }
    async fn pending(&self) -> Value {
        self.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await
    }
    fn answers(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.responses).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["result"].clone()).collect()
    }
    fn methods(&self) -> Vec<String> {
        std::fs::read_to_string(&self.requests).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].as_str().unwrap().to_owned()).collect()
    }
    fn item(&self, kind: &str) -> Option<Value> {
        self.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == kind).cloned()
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

/// Ask with `tag`, answer with `decision`, and return the finished view, the request Codex made
/// and what Codex received.
fn ask(tag: &'static str, answer: Value) -> BoxFuture<'static, (Value, Value, Value)> {
    async move {
        let mut harness = harness("approval-required", "default").await;
        harness.send(tag, json!({})).await;
        let request = harness.pending().await;
        harness.respond(&request, answer).await;
        let view = harness.settled(1).await;
        let received = harness.answers().pop().unwrap_or(Value::Null);
        harness.finish().await;
        (view, request, received)
    }
    .boxed()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_change_approval_reaches_the_user_and_each_decision_reaches_codex() {
    for (decision, received, run, change) in [
        ("accept", json!({"decision":"accept"}), "completed", true),
        ("acceptForSession", json!({"decision":"acceptForSession"}), "completed", true),
        // The original maps "accept always" to the session grant Codex understands.
        ("acceptAlways", json!({"decision":"acceptForSession"}), "completed", true),
        ("decline", json!({"decision":"decline"}), "completed", false),
        ("cancel", json!({"decision":"cancel"}), "interrupted", false),
    ] {
        let (view, request, answer) = ask("[file]", json!({"decision":decision})).await;
        assert_eq!(answer, received, "{decision}");
        assert_eq!(request["kind"], "file-change");
        assert_eq!(view["runs"][0]["status"], run, "{decision}");
        let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
        assert_eq!((item["requestKind"].as_str(), item["prompt"].as_str()), (Some("file-change"), Some("Write the fixture file")));
        let offered: Vec<&str> = item["options"].as_array().unwrap().iter().map(|option| option["decision"].as_str().unwrap()).collect();
        assert_eq!(offered, ["cancel", "decline", "acceptForSession", "accept"]);
        let files: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "file_change").collect();
        assert_eq!(files.len(), usize::from(change), "{decision}");
        if let Some(file) = files.first() {
            assert_eq!((file["fileName"].as_str(), file["diffStr"].as_str(), file["status"].as_str()), (Some("generated.txt"), Some("fixture\n"), Some("completed")));
        }
        assert_eq!(view["runtimeRequests"][0]["status"], "resolved");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permissions_approval_grants_what_was_asked_for_the_turn_or_the_session() {
    for (decision, received) in [
        ("accept", json!({"permissions":{"fileSystem":{"write":["/tmp/fixture-output"]},"network":null},"scope":"turn"})),
        ("acceptForSession", json!({"permissions":{"fileSystem":{"write":["/tmp/fixture-output"]},"network":null},"scope":"session"})),
        ("decline", json!({"permissions":{},"scope":"turn"})),
    ] {
        let (view, request, answer) = ask("[permissions]", json!({"decision":decision})).await;
        assert_eq!(answer, received, "{decision}");
        // A request to write is a file-change request to the user.
        assert_eq!(request["kind"], "file-change");
        assert_eq!(view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap()["prompt"], "Reach outside the workspace");
        assert_eq!(view["runs"][0]["status"], "completed");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_to_read_is_a_file_read_request() {
    let (view, request, answer) = ask("[permissions-read]", json!({"decision":"accept"})).await;
    assert_eq!(request["kind"], "file-read");
    assert_eq!(answer["permissions"]["fileSystem"], json!({"read":["/tmp/fixture-input"]}));
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
    assert_eq!(item["requestKind"], "file-read");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_and_dynamic_tool_calls_become_dynamic_tool_items_with_their_presentation() {
    let mut harness = harness("full-access", "default").await;
    harness.send("[mcp-tool]", json!({})).await;
    let view = harness.settled(1).await;
    let tools: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "dynamic_tool").collect();
    assert_eq!(tools.len(), 2);
    let mcp = tools[0];
    assert_eq!((mcp["toolName"].as_str(), mcp["title"].as_str(), mcp["status"].as_str()), (Some("github_app.create_issue"), Some("create issue"), Some("completed")));
    assert_eq!(mcp["input"], json!({"title":"Fixture"}));
    assert_eq!(mcp["output"], json!([{"type":"text","text":"created"}]));
    assert_eq!(mcp["toolSource"], json!({"key":"mcp:github_app","name":"github app","kind":"integration"}));
    let dynamic = tools[1];
    assert_eq!((dynamic["toolName"].as_str(), dynamic["status"].as_str()), (Some("fixture.lookup"), Some("completed")));
    assert_eq!((dynamic["input"].clone(), dynamic["output"].clone()), (json!({"q":1}), json!([{"type":"inputText","text":"found"}])));
    assert!(dynamic.get("toolSource").is_none());
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_elicitation_names_its_app_offers_what_it_can_honour_and_answers_in_its_own_shape() {
    let (view, request, answer) = ask("[elicit]", json!({"decision":"acceptForSession"})).await;
    assert_eq!(request["kind"], "mcp-elicitation");
    assert_eq!(answer, json!({"action":"accept","_meta":{"persist":"session"},"content":{"decision":"allow_session"}}));
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
    assert_eq!((item["appName"].as_str(), item["prompt"].as_str()), (Some("Fixture"), Some("Allow ChatGPT to use Fixture?")));
    let decisions: Vec<&str> = item["options"].as_array().unwrap().iter().map(|option| option["decision"].as_str().unwrap()).collect();
    assert_eq!(decisions, ["cancel", "decline", "acceptForSession", "accept"]);
    let (_, _, accepted) = ask("[elicit]", json!({"decision":"accept"})).await;
    assert_eq!(accepted, json!({"action":"accept","content":{"decision":"allow_once"}}));
    let (_, _, declined) = ask("[elicit]", json!({"decision":"decline"})).await;
    assert_eq!(declined, json!({"action":"decline"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_legacy_exec_and_patch_approvals_are_answered_with_review_decisions() {
    for (tag, decision, received) in [
        ("[exec-legacy]", "accept", json!({"decision":"approved"})),
        ("[exec-legacy]", "acceptForSession", json!({"decision":"approved_for_session"})),
        ("[exec-legacy]", "decline", json!({"decision":{"denied":{"rejection":"User declined the request."}}})),
        ("[exec-legacy]", "cancel", json!({"decision":"abort"})),
        ("[patch-legacy]", "accept", json!({"decision":"approved"})),
    ] {
        let (view, request, answer) = ask(tag, json!({"decision":decision})).await;
        assert_eq!(answer, received, "{tag} {decision}");
        let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
        if tag == "[exec-legacy]" {
            assert_eq!((request["kind"].as_str(), item["prompt"].as_str()), (Some("command"), Some("ls -la")));
        } else {
            assert_eq!((request["kind"].as_str(), item["prompt"].as_str()), (Some("file-change"), Some("delete a.txt\nadd b.txt")));
        }
        assert_eq!(view["runs"][0]["status"], if decision == "cancel" { "interrupted" } else { "completed" });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_codex_withdraws_is_cancelled_and_the_turn_goes_on() {
    let mut harness = harness("approval-required", "default").await;
    harness.send("[withdraw]", json!({})).await;
    let view = harness.settled(1).await;
    assert_eq!(view["runs"][0]["status"], "completed");
    let request = &view["runtimeRequests"][0];
    assert_eq!((request["status"].as_str(), request["responseCapability"]["type"].as_str()), (Some("cancelled"), Some("not_resumable")));
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "approval_request").unwrap();
    assert_eq!(item["status"], "cancelled");
    // Nothing is left to answer: Codex never receives a response to the withdrawn request.
    assert!(harness.answers().is_empty());
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_mode_decides_what_codex_may_do_without_asking() {
    // Full access sends the approval policy `never`: Codex does the work and nobody is asked.
    let mut harness = harness("full-access", "default").await;
    harness.send("[file]", json!({})).await;
    let view = harness.settled(1).await;
    assert_eq!(view["runtimeRequests"].as_array().unwrap().len(), 0);
    assert!(view["turnItems"].as_array().unwrap().iter().all(|item| item["type"] != "approval_request"));
    assert_eq!(harness.item("file_change").unwrap()["status"], "completed");
    harness.finish().await;
    // Approval-required asks first, and the work waits for the answer.
    let mut harness = harness_ask().await;
    harness.send("[file]", json!({})).await;
    let request = harness.pending().await;
    assert!(harness.item("file_change").is_none());
    harness.respond(&request, json!({"decision":"accept"})).await;
    harness.settled(1).await;
    assert_eq!(harness.item("file_change").unwrap()["status"], "completed");
    harness.finish().await;
}
async fn harness_ask() -> Harness {
    harness("approval-required", "default").await
}

#[tokio::test(flavor = "multi_thread")]
async fn reasoning_streams_per_part_and_todo_usage_and_plan_updates_reach_the_projection() {
    let mut harness = harness("full-access", "default").await;
    harness.send("[think][todo][usage][hold]", json!({})).await;
    // The reasoning, plan and usage are persisted as they stream, before the turn is over.
    let view = harness.until(|view| {
        let items = view["turnItems"].as_array().unwrap();
        let ready = items.iter().filter(|item| item["type"] == "reasoning").count() == 2
            && items.iter().any(|item| item["type"] == "todo_list" && item["steps"][1]["status"] == "completed")
            && view["providerTurns"].get(0).is_some_and(|turn| turn["tokenUsage"]["usedTokens"] == 180);
        ready.then(|| view.clone())
    }).await;
    let reasoning: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "reasoning").collect();
    assert_eq!(reasoning.iter().map(|item| item["text"].as_str().unwrap()).collect::<Vec<_>>(), ["**Reading the task**", "**Planning the change**"]);
    let todo = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "todo_list").unwrap();
    assert_eq!(todo["steps"], json!([{"id":"step-1","text":"Read","status":"completed"},{"id":"step-2","text":"Write","status":"completed"}]));
    assert_eq!((todo["explanation"].as_str(), view["plans"][0]["kind"].as_str(), view["plans"][0]["status"].as_str()), (Some("Fixture steps"), Some("todo_list"), Some("active")));
    // Live context usage rides on the provider turn; the turn's own usage is recorded when it ends.
    let turn = &view["providerTurns"][0];
    assert_eq!((turn["tokenUsage"]["maxTokens"].as_u64(), turn["tokenUsage"]["inputTokens"].as_u64(), turn["tokenUsage"]["cachedInputTokens"].as_u64()), (Some(200000), Some(160), Some(50)));
    let run = view["runs"][0]["id"].clone();
    harness.command(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run})).await;
    let view = harness.settled(1).await;
    let usage = &view["providerTurns"][0]["turnTokenUsage"];
    assert_eq!((usage["usageStatus"].as_str(), usage["inputTokens"].as_u64(), usage["cachedInputTokens"].as_u64(), usage["outputTokens"].as_u64(), usage["reasoningTokens"].as_u64()), (Some("partial"), Some(260), Some(90), Some(30), Some(5)));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_plan_in_plan_mode_can_be_accepted_by_the_next_message() {
    let mut harness = harness("full-access", "plan").await;
    harness.send("[plan]", json!({})).await;
    let view = harness.settled(1).await;
    let plan = view["plans"].as_array().unwrap().iter().find(|plan| plan["kind"] == "proposed_plan").unwrap().clone();
    assert_eq!((plan["status"].as_str(), plan["markdown"].as_str()), (Some("active"), Some("1. Do the work.\n")));
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "proposed_plan").unwrap();
    assert_eq!((item["status"].as_str(), item["streaming"].clone(), item["planId"].clone()), (Some("completed"), json!(false), plan["id"].clone()));
    harness.send("Implement it", json!({"sourcePlanRef":{"threadId":harness.thread,"planId":plan["id"]}})).await;
    let view = harness.settled(2).await;
    assert_eq!(view["runs"][1]["sourcePlanRef"]["planId"], plan["id"]);
    assert_eq!(view["plans"].as_array().unwrap().iter().find(|candidate| candidate["id"] == plan["id"]).unwrap()["status"], "completed");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proposed_plan_text_streams_into_the_plan_before_the_item_completes() {
    let mut harness = harness("full-access", "plan").await;
    harness.send("[plan][plan-open][hold]", json!({})).await;
    let view = harness.until(|view| view["plans"].get(0).filter(|plan| plan["markdown"] == "1. Do the work.\n").map(|_| view.clone())).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "proposed_plan").unwrap();
    assert_eq!((item["status"].as_str(), item["streaming"].clone(), item["markdown"].as_str()), (Some("running"), json!(true), Some("1. Do the work.\n")));
    assert_eq!(view["plans"][0]["status"], "active");
    let run = view["runs"][0]["id"].clone();
    harness.command(json!({"type":"run.interrupt","commandId":"stop","threadId":harness.thread,"runId":run})).await;
    harness.settled(1).await;
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retried_errors_show_as_a_retry_that_recovers_and_a_usage_limit_stop_carries_its_reset() {
    let mut harness = harness("full-access", "default").await;
    harness.send("[retry]", json!({})).await;
    let view = harness.settled(1).await;
    let errors: Vec<&Value> = view["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "error").collect();
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0]["title"].as_str(), errors[0]["status"].as_str(), errors[0]["retry"]["attempt"].as_u64(), errors[0]["retry"]["maxAttempts"].as_u64()), (Some("Provider recovered"), Some("completed"), Some(2), Some(5)));
    harness.finish().await;

    let mut harness = harness_ask().await;
    harness.send("[usage-limit]", json!({})).await;
    // The run fails first; its error item is recorded right after.
    let view = harness.until(|view| view["turnItems"].as_array().unwrap().iter().any(|item| item["type"] == "error").then(|| view.clone())).await;
    assert_eq!(view["runs"][0]["status"], "failed");
    let error = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").unwrap();
    assert_eq!((error["title"].as_str(), error["failure"]["class"].as_str(), error["failure"]["code"].as_str()), (Some("Usage limit reached"), Some("usage_limit"), Some("usageLimitExceeded")));
    assert_eq!(error["failure"]["resetAt"], "2027-01-15T09:00:00.000Z");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_asks_codex_to_compact_and_the_run_adopts_the_turn_codex_starts() {
    let mut harness = harness("full-access", "default").await;
    harness.send("Hello", json!({})).await;
    harness.settled(1).await;
    harness.send(" /Compact ", json!({})).await;
    let view = harness.settled(2).await;
    assert_eq!(view["runs"][1]["status"], "completed");
    let compaction = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "compaction").unwrap();
    assert_eq!((compaction["status"].as_str(), compaction["title"].as_str(), compaction["driver"].as_str()), (Some("completed"), Some("Context compacted"), Some("codex")));
    // One prompt turn, and the compaction as its own native request rather than as prompt text.
    let methods = harness.methods();
    assert_eq!((methods.iter().filter(|method| *method == "turn/start").count(), methods.iter().filter(|method| *method == "thread/compact/start").count()), (1, 1));
    assert_eq!(view["providerTurns"].as_array().unwrap().len(), 2);
    harness.finish().await;
}
