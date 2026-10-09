#![cfg(unix)]
//! Archiving a thread that has runs and a live provider session, end to end: the executing run is
//! ended by detaching its session, the runs still queued are cancelled, the thread moves to the
//! archived shell and a later message after unarchiving starts a fresh turn. A real recording
//! Codex fixture; nothing outside the temporary directory is touched.
use chrono::Utc;
use serde_json::{Value, json};
use t3_server::{
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
    thread::ThreadService,
};

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    execution: ExecutionService,
    record: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now())
        .unwrap();
    let record = base.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":base.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
    ThreadLaunchService::with_providers(store.clone(), providers.clone())
        .launch(json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap();
    let execution = ExecutionService::start(store.clone(), providers);
    Fixture { _directory: directory, store, execution, record }
}

impl Fixture {
    fn send(&self, id: &str, text: &str, mode: Value) {
        self.execution
            .dispatch(&json!({"type":"message.dispatch","commandId":id,"threadId":"thread","messageId":id,"text":text,"attachments":[],"dispatchMode":mode}), Utc::now())
            .unwrap();
    }
    fn thread_command(&self, kind: &str, id: &str) -> t3_server::persistence::Receipt {
        ThreadService::new(self.store.clone()).dispatch(&json!({"type":kind,"commandId":id,"threadId":"thread"}), Utc::now()).unwrap()
    }
    fn provider_requests(&self, method: &str) -> usize {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter(|line| serde_json::from_str::<Value>(line).unwrap()["method"] == method)
            .count()
    }
    fn view(&self) -> Value {
        self.store.projection("thread", "thread").unwrap().unwrap()
    }
}

async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let view = store.projection("thread", "thread").unwrap().unwrap();
            last = json!({"thread":view["thread"],"runs":view["runs"],"sessions":view["providerSessions"]});
            if done(&view) {
                return view;
            }
            events.recv().await.unwrap();
        }
    })
    .await;
    outcome.unwrap_or_else(|_| panic!("the thread never reached the wanted state: {last}"))
}
fn statuses(view: &Value) -> Vec<String> {
    view["runs"].as_array().unwrap().iter().map(|run| run["status"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn archiving_ends_the_executing_run_cancels_the_queued_one_and_detaches_the_session() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    fixture.send("first", "work [hold]", json!({"type":"start_immediately"}));
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["running"] && view["providerSessions"].as_array().is_some_and(|sessions| !sessions.is_empty())).await;
    let session = view["providerSessions"][0]["id"].clone();
    fixture.send("second", "later", json!({"type":"queue_after_active"}));
    until(&fixture.store, &mut events, |view| statuses(view) == ["running", "queued"]).await;

    let receipt = fixture.thread_command("thread.archive", "archive");
    assert_eq!(receipt.status, "accepted");
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["interrupted", "cancelled"]).await;
    // The same events the original decides: the queued run is cancelled in the command's own
    // commit, the session detaches from the thread and the executing run is ended by the detach.
    assert!(!view["thread"]["archivedAt"].is_null());
    assert!(view["providerSessions"].as_array().unwrap().iter().all(|candidate| candidate["id"] != session));
    assert!(view["runs"][1]["queuePosition"].is_null());
    assert_eq!(view["attempts"][1]["status"], "cancelled");
    assert_eq!(fixture.provider_requests("turn/interrupt"), 1);
    // The queued message never reached the provider.
    assert_eq!(fixture.provider_requests("turn/start"), 1);

    // The archived shell lists the thread and the active shell does not.
    let shell = ThreadService::new(fixture.store.clone()).shell_snapshot().unwrap();
    assert!(shell["threads"].as_array().unwrap().is_empty());
    assert_eq!(shell["archivedThreads"][0]["id"], "thread");
    assert!(!shell["archivedThreads"][0]["archivedAt"].is_null());

    // Unarchiving brings the thread back; the next message starts a fresh turn.
    assert_eq!(fixture.thread_command("thread.unarchive", "unarchive").status, "accepted");
    fixture.send("third", "after", json!({"type":"start_immediately"}));
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["interrupted", "cancelled", "completed"]).await;
    assert!(view["thread"]["archivedAt"].is_null());
    assert_eq!(fixture.provider_requests("turn/start"), 2);
    fixture.execution.shutdown().await;
}

#[tokio::test]
async fn an_archived_thread_takes_no_messages_and_cannot_be_archived_again() {
    let fixture = fixture().await;
    assert_eq!(fixture.thread_command("thread.archive", "archive").status, "accepted");
    let again = fixture.thread_command("thread.archive", "archive-again");
    assert_eq!(again.status, "rejected");
    assert_eq!(again.error.unwrap()["cause"], "Thread thread is already archived.");
    let refused = fixture
        .execution
        .dispatch(&json!({"type":"message.dispatch","commandId":"late","threadId":"thread","messageId":"late","text":"hi","attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now())
        .unwrap_err();
    assert_eq!(refused.to_string(), "Invalid command: Thread is deleted or archived.");
    assert_eq!(fixture.view()["runs"].as_array().unwrap().len(), 0);
    fixture.execution.shutdown().await;
}
