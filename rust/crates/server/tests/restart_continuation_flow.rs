#![cfg(unix)]
//! Continuing a turn a server restart cut, against the scripted fake Codex app-server: with
//! `continueThreadsAfterServerUpdate` on, the restarted server resumes the turn that was running, once;
//! with it off, or after the user stopped the turn, it does not. A restart is the execution service shut
//! down and started again over the same store. Tests wait on persisted events, never on sleeps.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use serde_json::{Value, json};
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    providers: ProviderRegistry,
    execution: ExecutionService,
    thread: String,
    requests: std::path::PathBuf,
}

async fn harness(continue_after_update: bool) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let requests = cwd.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"continueThreadsAfterServerUpdate":continue_after_update,"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":requests}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    Harness { _directory: directory, store, providers, execution, thread, requests }
}

impl Harness {
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}; runs {}", self.view()["runs"]))
    }
    fn send(&self, id: &str, text: &str) {
        self.execution.dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    }
    /// The server stops and starts again over the same store.
    async fn restart(&mut self) {
        self.execution.shutdown().await;
        self.execution = ExecutionService::start(self.store.clone(), self.providers.clone());
    }
    fn turn_starts(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.requests).unwrap_or_default().lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|record| record["method"] == "turn/start").map(|record| record["params"].clone()).collect()
    }
    async fn finish(self) {
        self.execution.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_resumes_the_turn_it_cut_once_when_the_setting_is_on() {
    let mut h = harness(true).await;
    h.send("work", "work [hold]");
    h.until("the turn to run", |view| (view["runs"][0]["status"] == "running" && view["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running"))).then_some(())).await;
    h.restart().await;
    let view = h.until("the continuation to finish", |view| (view["runs"].as_array().unwrap().len() == 2 && view["runs"][1]["status"] == "completed").then(|| view.clone())).await;
    assert_eq!(view["runs"][1]["restartContinuationOfRunId"], view["runs"][0]["id"]);
    assert!(matches!(view["runs"][0]["status"].as_str(), Some("cancelled" | "interrupted")));
    let message = view["messages"].as_array().unwrap().iter().find(|message| message["id"] == format!("message:restart-continuation:{}", view["runs"][0]["id"].as_str().unwrap())).unwrap();
    assert_eq!((message["text"].as_str(), message["createdBy"].as_str(), message["creationSource"].as_str()), (Some("Continue where you left off."), Some("agent"), Some("server")));
    // Codex resumes the cut turn: the continuation's start carries no input.
    let starts = h.turn_starts();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[1]["input"], json!([]));
    // Another restart finds nothing to continue.
    h.restart().await;
    h.execution.drained(&h.thread).await;
    assert_eq!(h.view()["runs"].as_array().unwrap().len(), 2);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_leaves_the_cut_turn_alone_when_the_setting_is_off_or_the_user_stopped_it() {
    let mut off = harness(false).await;
    off.send("work", "work [hold]");
    off.until("the turn to run", |view| (view["runs"][0]["status"] == "running" && view["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running"))).then_some(())).await;
    off.restart().await;
    off.execution.drained(&off.thread).await;
    assert_eq!(off.view()["runs"].as_array().unwrap().len(), 1);
    off.finish().await;

    let mut stopped = harness(true).await;
    stopped.send("work", "work [hold]");
    let run = stopped.until("the turn to run", |view| (view["runs"][0]["status"] == "running" && view["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running"))).then(|| view["runs"][0]["id"].clone())).await;
    stopped.execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop","threadId":stopped.thread,"runId":run}), Utc::now()).unwrap();
    stopped.until("the run to end", |view| (view["runs"][0]["status"] == "interrupted").then_some(())).await;
    stopped.restart().await;
    stopped.execution.drained(&stopped.thread).await;
    assert_eq!(stopped.view()["runs"].as_array().unwrap().len(), 1);
    stopped.finish().await;
}
