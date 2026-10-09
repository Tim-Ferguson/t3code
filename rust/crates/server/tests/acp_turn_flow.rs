#![cfg(unix)]
//! The ACP turn path against a scripted agent (`fixtures/acp-turn-agent.py`): what a session update,
//! an agent question or a mode change becomes in the persisted thread, and what the agent is told
//! back. Tests wait on persisted events, never on sleeps.
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry, thread::ThreadService};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/acp-turn-agent.py");

struct Turn {
    _root: tempfile::TempDir,
    log: PathBuf,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    thread: String,
    next: usize,
}

impl Turn {
    async fn start(script: Value, runtime_mode: &str, interaction_mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let (path, log) = (cwd.join("script.json"), cwd.join("agent.jsonl"));
        std::fs::write(&path, script.to_string()).unwrap();
        let settings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "agent":{"driver":"acpRegistry","enabled":true,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[AGENT,path,log]}}}}))
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let store = Store::open(cwd.join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Fixture","workspaceRoot":cwd}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"ACP","modelSelection":{"instanceId":"agent","model":"default"},"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        Self { _root: root, log, store, execution, providers, thread, next: 0 }
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    fn send(&mut self, text: &str) -> usize {
        self.next += 1;
        let id = format!("message-{}", self.next);
        self.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn until<T>(&self, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a persisted event: {}", self.view()["turnItems"]))
    }
    async fn settled(&self, ordinal: usize) -> Value {
        self.until(|view| view["runs"].get(ordinal - 1).filter(|run| matches!(run["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled"))).cloned()).await
    }
    fn items(&self, kind: &str) -> Vec<Value> {
        self.view()["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == kind).cloned().collect()
    }
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["in"].clone()).collect()
    }
    /// A server restart: the actors end, and the next message starts the agent again from the saved session.
    async fn restart(&mut self) {
        self.execution.shutdown().await;
        self.execution = ExecutionService::start(self.store.clone(), self.providers.clone());
    }
    fn methods(&self) -> Vec<String> {
        self.received().iter().filter_map(|message| message["method"].as_str().map(ToOwned::to_owned)).filter(|method| method.starts_with("session/") && method != "session/prompt").collect()
    }
    fn dispatch_thread(&self, command: Value) {
        ThreadService::new(self.store.clone()).dispatch(&command, Utc::now()).unwrap();
    }
}

fn update(value: Value) -> Value {
    json!({"update": value})
}
fn text(text: &str) -> Value {
    update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}))
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_session_info_and_unknown_updates_reach_the_thread_and_the_timeline() {
    let script = json!({"protocol":2,"prompts":[[
        update(json!({"sessionUpdate":"usage_update","used":120,"size":1000,"cost":{"amount":0.5,"currency":"USD"}})),
        update(json!({"sessionUpdate":"session_info_update","title":"  Named by the agent ","updatedAt":"2026-01-01T00:00:00Z"})),
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":"build","title":"Build","kind":"execute","status":"in_progress","rawInput":{"command":"make"}})),
        update(json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":"build","content":{"type":"content","content":{"type":"text","text":"compiling "}}})),
        update(json!({"sessionUpdate":"tool_call_content_chunk","toolCallId":"build","content":{"type":"content","content":{"type":"text","text":"done"}}})),
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":"build","status":"completed"})),
        update(json!({"sessionUpdate":"compaction_summary_chunk","compactionId":"c1","content":{"type":"text","text":"Summary A. "}})),
        update(json!({"sessionUpdate":"compaction_update","compactionId":"c1","status":"completed","summary":[{"type":"text","text":"Summary final"}]})),
        update(json!({"sessionUpdate":"state_update","state":"requires_action"})),
        update(json!({"sessionUpdate":"state_update","state":"running"})),
        update(json!({"sessionUpdate":"timeline_update","entries":[]})),
        text("all done"),
        {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("go");
    turn.settled(run).await;
    let view = turn.view();
    let thread = &view["providerThreads"][0];
    assert_eq!(thread["contextUsage"], json!({"usedTokens":120,"maxTokens":1000,"cost":{"amount":0.5,"currency":"USD"}}));
    assert_eq!((thread["nativeMetadata"]["title"].as_str(), thread["nativeMetadata"]["updatedAt"].as_str()), (Some("Named by the agent"), Some("2026-01-01T00:00:00Z")));
    let build = turn.items("command_execution").into_iter().find(|item| item["input"] == "make").unwrap();
    assert_eq!(build["output"], "compiling done", "{build}");
    let compaction = &turn.items("compaction")[0];
    assert_eq!((compaction["summary"].as_str(), compaction["status"].as_str(), compaction["driver"].as_str()), (Some("Summary final"), Some("completed"), Some("acpRegistry")));
    let titles: Vec<String> = view["turnItems"].as_array().unwrap().iter().filter_map(|item| item["title"].as_str().map(ToOwned::to_owned)).collect();
    assert!(titles.contains(&"Unsupported ACP update: timeline_update".to_string()), "{titles:?} {}", view["turnItems"].as_array().unwrap().iter().map(|item| format!("{}/{}/{}", item["type"], item["title"], item["toolName"])).collect::<Vec<_>>().join(", "));
    assert!(titles.contains(&"Action required".to_string()), "{titles:?}");
    let action = view["turnItems"].as_array().unwrap().iter().find(|item| item["title"] == "Action required").unwrap();
    assert_eq!(action["status"], "completed", "a later non-requiring state completes the action item");
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_with_ids_and_reasoning_split_into_the_items_the_agent_meant() {
    let chunk = |kind: &str, id: Option<&str>, body: &str| {
        let mut value = json!({"sessionUpdate":kind,"content":{"type":"text","text":body}});
        if let Some(id) = id {
            value["messageId"] = json!(id);
        }
        update(value)
    };
    let script = json!({"protocol":2,"prompts":[[
        chunk("agent_message_chunk", Some("m1"), "first "),
        update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t","title":"Read a","kind":"read","status":"in_progress","rawInput":{"path":"a"}})),
        chunk("agent_message_chunk", Some("m1"), "second"),
        chunk("agent_thought_chunk", None, "thinking"),
        chunk("agent_message_chunk", Some("m2"), "other message"),
        chunk("agent_message_chunk", Some("m2"), " continues"),
        update(json!({"sessionUpdate":"agent_message","messageId":"m2","content":[{"type":"text","text":"replaced m2"}]})),
        {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("go");
    turn.settled(run).await;
    let assistant: Vec<String> = turn.items("assistant_message").iter().map(|item| item["text"].as_str().unwrap().to_owned()).collect();
    // m1 keeps streaming across the tool and the thought ends it; m2 is replaced in place.
    assert_eq!(assistant, ["first second", "replaced m2"], "{assistant:?}");
    assert_eq!(turn.items("reasoning").len(), 1);
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_form_elicitation_is_a_user_question_and_only_asked_keys_are_answered() {
    let script = json!({"prompts":[[
        {"request":{"method":"elicitation/create","params":{"mode":"form","message":"Pick","requestedSchema":{"type":"object","properties":{
            "color":{"type":"string","title":"Color","description":"Which color?","enum":["red","blue"]},
            "sure":{"type":"boolean","title":"Sure"}},"required":["color"]}}}},
        text("thanks"),
        {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("ask");
    let request = turn.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending" && request["kind"] == "user_input").cloned()).await;
    let item = &turn.items("user_input_request")[0];
    assert_eq!(item["questions"][1], json!({"id":"sure","header":"Sure","question":"Pick","options":[{"label":"true","description":"Yes"},{"label":"false","description":"No"}]}));
    turn.execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"answer","threadId":turn.thread,"requestId":request["id"],"answers":{"color":"blue","sure":"true","invented":"x"}}), Utc::now()).unwrap();
    turn.settled(run).await;
    let answer = turn.received().into_iter().find(|message| message["id"] == 101).unwrap();
    assert_eq!(answer["result"], json!({"action":"accept","content":{"color":"blue","sure":"true"}}));
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_turns_switch_the_agent_mode_and_build_turns_put_it_back() {
    let script = json!({"setup":{"modes":{"currentModeId":"code","availableModes":[{"id":"code","name":"Code"},{"id":"plan","name":"Plan"}]}},
        "prompts":[[text("planned"), {"end":"end_turn"}],[text("built"), {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "plan").await;
    let run = turn.send("plan it");
    turn.settled(run).await;
    turn.dispatch_thread(json!({"type":"thread.interaction-mode.set","commandId":"build","threadId":turn.thread,"interactionMode":"default"}));
    let run = turn.send("now build");
    turn.settled(run).await;
    let modes: Vec<String> = turn.received().iter().filter(|message| message["method"] == "session/set_mode").map(|message| message["params"]["modeId"].as_str().unwrap().to_owned()).collect();
    assert_eq!(modes, ["plan", "code"]);
    assert_eq!(turn.view()["runs"].as_array().unwrap().iter().map(|run| run["status"].clone()).collect::<Vec<_>>(), [json!("completed"), json!("completed")]);
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_session_is_resumed_when_the_agent_cannot_load_it() {
    let script = json!({"capabilities":{"loadSession":false,"sessionCapabilities":{"resume":{}}},"prompts":[[text("one"),{"end":"end_turn"}],[text("two"),{"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("first");
    turn.settled(run).await;
    turn.restart().await;
    let run = turn.send("second");
    turn.settled(run).await;
    // The first session/new is discovery's probe, the second the first turn's.
    assert_eq!(turn.methods(), ["session/new", "session/new", "session/resume"]);
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_session_the_agent_refuses_to_load_starts_a_fresh_one() {
    let script = json!({"loadFails":true,"prompts":[[text("one"),{"end":"end_turn"}],[text("two"),{"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("first");
    turn.settled(run).await;
    turn.restart().await;
    let run = turn.send("second");
    let second = turn.settled(run).await;
    assert_eq!(second["status"], "completed");
    assert_eq!(turn.methods(), ["session/new", "session/new", "session/load", "session/new"]);
    assert_eq!(turn.view()["providerThreads"][0]["nativeMetadata"]["itemIdentityVersion"], 2);
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_the_agent_cancelled_on_its_own_is_cancelled_not_interrupted() {
    let script = json!({"prompts":[[text("starting"),{"end":"cancelled"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("go");
    assert_eq!(turn.settled(run).await["status"], "cancelled");
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_message_of_an_imported_thread_loads_the_native_session() {
    let script = json!({"prompts":[[text("continuing"),{"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    // The imported thread, beside the one the harness launched.
    let imported = "imported-thread";
    turn.dispatch_thread(json!({"type":"thread.create","commandId":"import","threadId":imported,"projectId":"project","title":"Imported","modelSelection":{"instanceId":"agent","model":"default"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,
        "importedNativeThread":{"ref":{"driver":"acpRegistry","nativeId":"turn-session","strength":"strong"},"metadata":{"itemIdentityVersion":2,"title":"Imported"}}}));
    turn.thread = imported.to_owned();
    let run = turn.send("pick this up");
    turn.settled(run).await;
    let loads: Vec<Value> = turn.received().into_iter().filter(|message| message["method"] == "session/load").collect();
    assert_eq!(loads.len(), 1, "{:?}", turn.methods());
    assert_eq!(loads[0]["params"]["sessionId"], "turn-session");
    let view = turn.view();
    assert_eq!(view["providerThreads"].as_array().unwrap().len(), 1, "the imported provider thread is used, not a second one");
    assert_eq!(view["providerThreads"][0]["nativeThreadRef"]["nativeId"], "turn-session");
    assert_eq!(view["providerThreads"][0]["nativeMetadata"]["title"], "Imported");
    assert_eq!(turn.items("assistant_message")[0]["text"], "continuing");
    turn.execution.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_tool_approval_form_is_answered_by_the_policy_unless_it_asks() {
    let approval = || json!({"request":{"method":"elicitation/create","params":{"mode":"form","message":"Allow the tool?","_meta":{"codex_approval_kind":"mcp_tool_call"},"requestedSchema":{"type":"object","properties":{}}}}});
    let script = json!({"prompts":[[approval(), text("ran"), {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "full-access", "default").await;
    let run = turn.send("use the tool");
    turn.settled(run).await;
    let answer = turn.received().into_iter().find(|message| message["id"] == 101).unwrap();
    assert_eq!(answer["result"], json!({"action":"accept","content":{}}));
    assert!(turn.items("user_input_request").is_empty(), "the policy allowed it without asking");
    turn.execution.shutdown().await;

    // Approval-required mode puts the question to the user.
    let script = json!({"prompts":[[approval(), text("ran"), {"end":"end_turn"}]]});
    let mut turn = Turn::start(script, "approval-required", "default").await;
    let run = turn.send("use the tool");
    let request = turn.until(|view| view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").cloned()).await;
    assert_eq!(request["kind"], "user_input");
    // The question has no field to fill in, so the user's only answer is to stop the run.
    turn.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":turn.thread,"runId":turn.view()["runs"][0]["id"]}), Utc::now()).unwrap();
    assert_eq!(turn.settled(run).await["status"], "interrupted");
    turn.execution.shutdown().await;
}
