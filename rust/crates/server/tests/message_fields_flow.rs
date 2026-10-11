#![cfg(unix)]
//! Message fields the planner used to refuse, end to end with the recording Codex fixture:
//! composer context reaches the provider as the original projects it, a queued notification is
//! kept on its message and refused from anyone else, and attachments ride along on a steering
//! message and on the answer to a question. Nothing outside the temporary directory is touched.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use serde_json::{Value, json};
use t3_server::{
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
};

struct Fixture {
    _directory: tempfile::TempDir,
    base: std::path::PathBuf,
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
    Fixture { _directory: directory, base, store, execution, record }
}

impl Fixture {
    fn dispatch(&self, command: Value) -> Result<t3_server::persistence::Receipt, t3_server::persistence::StoreError> {
        self.execution.dispatch(&command, Utc::now())
    }
    fn message(&self, id: &str, text: &str, mode: Value, extra: Value) -> Value {
        let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":"thread","messageId":id,"text":text,"attachments":[],"dispatchMode":mode});
        for (key, value) in extra.as_object().unwrap() {
            command[key] = value.clone();
        }
        command
    }
    fn provider_requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == method)
            .collect()
    }
    fn view(&self) -> Value {
        self.store.projection("thread", "thread").unwrap().unwrap()
    }
    /// A thread-owned image the way the claim step leaves one.
    fn stored_image(&self, id: &str, bytes: &[u8]) -> Value {
        let directory = self.base.join("attachments");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{id}.png")), bytes).unwrap();
        json!({"type":"image","id":id,"name":"shot.png","mimeType":"image/png","sizeBytes":bytes.len()})
    }
}

async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(HANG_BOUND, async {
        loop {
            let view = store.projection("thread", "thread").unwrap().unwrap();
            last = json!({"runs":view["runs"],"requests":view["runtimeRequests"]});
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
fn immediately() -> Value {
    json!({"type":"start_immediately"})
}

#[tokio::test]
async fn composer_context_is_kept_on_the_message_and_shown_to_the_provider_as_the_original_projects_it() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    let context = json!({"version":1,"records":[{"version":1,"contextId":"men_1","label":"README.md","kind":"mention","path":"docs/README.md"}]});
    let text = "read [README.md](t3-context://v1/mention/men_1) now";
    fixture.dispatch(fixture.message("one", text, immediately(), json!({"context":context}))).unwrap();
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["completed"]).await;
    assert_eq!(view["messages"][0]["context"], context);
    assert_eq!(view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_message").unwrap()["context"], context);
    assert_eq!(view["messages"][0]["text"], text);
    let shown = fixture.provider_requests("turn/start")[0]["params"]["input"][0]["text"].as_str().unwrap().to_owned();
    assert_eq!(
        shown,
        "read [Mention: README.md; ref=men_1] now\n\n<t3_context version=\"1\">\n<context kind=\"mention\" id=\"men_1\">\npath: docs/README.md\n</context>\n</t3_context>"
    );
    fixture.execution.shutdown().await;
}

#[tokio::test]
async fn a_notification_is_a_queued_message_from_the_server_or_the_provider_and_nothing_else() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    fixture.dispatch(fixture.message("first", "work [hold]", immediately(), json!({}))).unwrap();
    until(&fixture.store, &mut events, |view| statuses(view) == ["running"]).await;
    let notification = json!({"source":{"kind":"monitor"},"outcome":"completed","summary":"Monitor finished"});
    let by_user = fixture.dispatch(fixture.message("by-user", "ping", json!({"type":"queue_after_active"}), json!({"notification":notification}))).unwrap_err();
    assert_eq!(by_user.to_string(), "Invalid command: Notifications must be server- or provider-created queued messages.");
    let not_queued = fixture.dispatch(fixture.message("not-queued", "ping", immediately(), json!({"notification":notification,"createdBy":"agent","creationSource":"server"}))).unwrap_err();
    assert!(not_queued.to_string().ends_with("Notifications must be server- or provider-created queued messages."), "{not_queued}");
    fixture.dispatch(fixture.message("queued", "ping", json!({"type":"queue_after_active"}), json!({"notification":notification,"createdBy":"agent","creationSource":"server"}))).unwrap();
    let view = fixture.view();
    assert_eq!(statuses(&view), ["running", "queued"]);
    assert_eq!(view["messages"].as_array().unwrap().iter().find(|message| message["id"] == "queued").unwrap()["notification"], notification);
    fixture.execution.shutdown().await;
}

#[tokio::test]
async fn a_continuation_is_only_honoured_from_the_latest_interrupted_run() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    fixture.dispatch(fixture.message("first", "work [hold]", immediately(), json!({}))).unwrap();
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["running"]).await;
    let run = view["runs"][0]["id"].clone();
    // A run that is still running cannot be continued.
    let early = fixture.dispatch(fixture.message("early", "again", immediately(), json!({"manualContinuationOfRunId":run}))).unwrap_err();
    assert_eq!(early.to_string(), "Invalid command: This thread can no longer be resumed from that run.");
    fixture.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":"thread","runId":run})).unwrap();
    until(&fixture.store, &mut events, |view| statuses(view) == ["interrupted"]).await;
    let queued = fixture.dispatch(fixture.message("queued", "again", json!({"type":"queue_after_active"}), json!({"manualContinuationOfRunId":run}))).unwrap_err();
    assert_eq!(queued.to_string(), "Invalid command: This thread can no longer be resumed from that run.");
    fixture.dispatch(fixture.message("continue", "again", immediately(), json!({"manualContinuationOfRunId":run}))).unwrap();
    until(&fixture.store, &mut events, |view| statuses(view) == ["interrupted", "completed"]).await;
    // Only an interrupted (or usage-limited) run can be continued: the one that finished cannot.
    let finished = fixture.view()["runs"][1]["id"].clone();
    let stale = fixture.dispatch(fixture.message("stale", "again", immediately(), json!({"manualContinuationOfRunId":finished}))).unwrap_err();
    assert_eq!(stale.to_string(), "Invalid command: This thread can no longer be resumed from that run.");
    fixture.execution.shutdown().await;
}

#[tokio::test]
async fn a_steering_message_delivers_its_attachments_to_the_running_turn() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    fixture.dispatch(fixture.message("first", "work [hold]", immediately(), json!({}))).unwrap();
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["running"]).await;
    let image = fixture.stored_image("thread-00000000-0000-4000-8000-000000000001", b"png-bytes");
    let run = view["runs"][0]["id"].clone();
    fixture.dispatch(fixture.message("steer", "look at this", json!({"type":"steer_active","targetRunId":run}), json!({"attachments":[image]}))).unwrap();
    // The fixture finishes the turn when it takes the steering message, so completion is the milestone.
    until(&fixture.store, &mut events, |view| statuses(view) == ["completed"]).await;
    let steer = fixture.provider_requests("turn/steer").into_iter().next().expect("the steering message never reached the provider");
    let input = steer["params"]["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "{input:?}");
    assert!(input[0]["text"].as_str().unwrap().starts_with("look at this"), "{input:?}");
    use base64::Engine;
    assert_eq!(input[1]["url"], format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(b"png-bytes")));
    assert_eq!(fixture.view()["messages"].as_array().unwrap().iter().find(|message| message["id"] == "steer").unwrap()["attachments"][0]["id"], "thread-00000000-0000-4000-8000-000000000001");
    fixture.execution.shutdown().await;
}

#[tokio::test]
async fn an_answer_with_attachments_names_each_file_in_the_answer_and_keeps_them_on_the_question() {
    let fixture = fixture().await;
    let mut events = fixture.store.subscribe();
    fixture.dispatch(fixture.message("first", "ask [question]", immediately(), json!({}))).unwrap();
    let view = until(&fixture.store, &mut events, |view| view["runtimeRequests"].as_array().unwrap().iter().any(|request| request["status"] == "pending")).await;
    let request = view["runtimeRequests"].as_array().unwrap().iter().find(|request| request["status"] == "pending").unwrap().clone();
    let image = fixture.stored_image("thread-00000000-0000-4000-8000-000000000002", b"answer-bytes");
    let path = fixture.base.join("attachments").join("thread-00000000-0000-4000-8000-000000000002.png");
    let command = json!({"type":"runtime-request.respond","commandId":"answer","threadId":"thread","requestId":request["id"],"answers":{"choice":"One"},"attachmentsByQuestionId":{"choice":[image]}});
    let files = t3_server::attachment_service::AttachmentService::for_store(&fixture.store);
    let receipt = t3_server::answer_attachments::respond_with_claims(files.as_ref(), &fixture.store, &command, |command| fixture.execution.dispatch(command, Utc::now()));
    assert_eq!(receipt.ok().expect("the answer was refused").status, "accepted");
    let view = until(&fixture.store, &mut events, |view| statuses(view) == ["completed"]).await;
    let item = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "user_input_request").unwrap().clone();
    let sent = format!("One\n\nAttached image \"shot.png\": {}", serde_json::to_string(path.to_str().unwrap()).unwrap());
    assert_eq!(item["questionAnswer"]["answers"]["choice"], sent);
    assert_eq!(item["questionAnswer"]["attachmentsByQuestionId"]["choice"][0]["id"], "thread-00000000-0000-4000-8000-000000000002");
    assert_eq!(item["questionAnswer"]["questionTextById"]["choice"], "Choose a fixture response");
    fixture.execution.shutdown().await;
}
