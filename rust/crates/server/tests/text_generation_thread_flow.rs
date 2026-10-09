#![cfg(unix)]
//! Thread titles over a real socket: the first message of a thread is titled by the provider the
//! settings choose (the message's own words stand in meanwhile), a rename can be asked for, and a
//! model that fails or cannot be reached leaves the title as it was. The provider is a stand-in
//! executable that records how it was started and answers as scripted
//! (`fixtures/text-generation-provider.py`; for the thread's own turn it hands over to the Codex
//! fixture), so no provider CLI is run. Nothing waits on a timer: the test follows committed events.
#![recursion_limit = "256"]
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}, time::Duration};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::Store,
    project::ProjectService,
    provider_registry::ProviderRegistry,
    text_generation_run::Options,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn fake() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-generation-provider.py")
}

/// Codex runs the stand-in; an ACP Registry agent runs threads but cannot write text.
fn settings(codex: bool, calls: &Path) -> t3_contracts::ServerSettings {
    let acp = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
    serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "local-agent":{"driver":"acpRegistry","enabled":true,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[acp,"2","normal"]}},
        "codex":{"driver":"codex","enabled":codex,"config":{"binaryPath":fake(),"homePath":calls.join("codex-home")},"environment":[{"name":"T3_TEXTGEN_DIR","value":calls},{"name":"T3_FAKE_DELEGATE","value":"codex"}]}}})).unwrap()
}

fn plan(calls: &Path, number: usize, plan: Value) {
    std::fs::write(calls.join(format!("{number:03}.plan.json")), plan.to_string()).unwrap();
}

fn capture(calls: &Path, number: usize) -> Value {
    serde_json::from_str(&std::fs::read_to_string(calls.join(format!("{number:03}.capture.json"))).unwrap()).unwrap()
}

fn captures(calls: &Path) -> usize {
    std::fs::read_dir(calls).unwrap().filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().ends_with(".capture.json")).count()
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
    loop {
        let value: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// Waits for a committed event after which the thread satisfies `done`. The limit is only a guard
/// against a test that can never finish.
async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<t3_server::persistence::StoredEvent>>, thread: &str, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view = store.projection("thread", thread).unwrap().unwrap();
            if done(&view) {
                return view;
            }
            last = json!({"thread":view["thread"],"runs":view["runs"],"messages":view["messages"]});
            events.recv().await.unwrap();
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("the thread never reached the expected state: {last}"))
}

fn message(thread: &str, id: &str, seed: Option<&str>) -> Value {
    let mut command = json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":format!("message:{id}"),"text":"Fix the login bug","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}});
    if let Some(seed) = seed {
        command["titleSeed"] = json!(seed);
    }
    command
}

#[test]
fn thread_titles_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let calls = base.join("calls");
    std::fs::create_dir(&calls).unwrap();
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let providers = ProviderRegistry::discover(&settings(true, &calls), &base).await.unwrap();
    providers.set_text_generation_options(Options { timeout: Duration::from_secs(60), retry_delay: Duration::ZERO });
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let launch_on = |id: &str, instance: &str| {
        ThreadLaunchService::new(store.clone())
            .launch(json!({"commandId":id,"projectId":"project","title":"Thread","modelSelection":{"instanceId":instance,"model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let launch = |id: &str| launch_on(id, "codex");
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_title".into(), "loopback-browser".into()).unwrap();
    let operator = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"title-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: Some(providers.clone()), execution: Some(execution.clone()), workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap() });
    let mut socket = connect(address, &operator).await;
    let mut shell = connect(address, &operator).await;
    let mut events = store.subscribe();

    // A client watching the shell sees the thread named for its first message, then renamed.
    shell.send(Message::Text(json!({"_tag":"Request","id":1,"tag":"orchestration.subscribeShell","payload":{"requestCompletionMarker":true},"headers":[]}).to_string().into())).await.unwrap();
    loop {
        let value: Value = serde_json::from_str(shell.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if value["_tag"] == "Chunk" {
            shell.send(Message::Text(json!({"_tag":"Ack","requestId":1}).to_string().into())).await.unwrap();
            if value["values"][0]["kind"] == "synchronized" {
                break;
            }
        }
    }

    // The first message: the seed is the title at once, the model's title replaces it.
    let first = launch("launch-1");
    plan(&calls, 1, json!({"answer":{"title":"\"Login Redirect Fix\"","needsRefinement":false}}));
    let exit = rpc(&mut socket, 1, "orchestration.dispatchCommand", message(&first, "send-1", Some("Fix the login bug"))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &first, |view| view["thread"]["title"] == "Login Redirect Fix" && view["thread"]["titleRegeneration"].is_null()).await;
    assert_eq!(view["thread"]["title"], "Login Redirect Fix");
    // The shell is told of the thread as it stands when it is told, so a client that is slow may
    // only ever see the final title; the committed events say what happened: first the seed with
    // the request armed, then the title with the request cleared.
    let mut told = false;
    while !told {
        let value: Value = serde_json::from_str(shell.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if value["_tag"] != "Chunk" {
            continue;
        }
        shell.send(Message::Text(json!({"_tag":"Ack","requestId":1}).to_string().into())).await.unwrap();
        told = value["values"].as_array().unwrap().iter().any(|update| update["kind"] == "thread.updated" && update["thread"]["id"] == first.as_str() && update["thread"]["title"] == "Login Redirect Fix");
    }
    let titled: Vec<Value> = store
        .read(|connection| Ok(connection.prepare("SELECT event_json FROM rust_application_events WHERE aggregate_id=?1 AND event_type='thread.metadata-updated' ORDER BY sequence")?.query_map([&first], |row| row.get::<_, String>(0))?.map(|json| Ok(serde_json::from_str::<Value>(&json?).unwrap())).collect::<rusqlite::Result<_>>()?))
        .unwrap();
    assert_eq!(titled.len(), 2, "one event arms the title, one lands it");
    let (armed, landed) = (&titled[0]["payload"], &titled[1]["payload"]);
    assert_eq!((armed["title"].as_str(), armed["titleRegeneration"]["requestId"].as_str()), (Some("Fix the login bug"), Some("send-1")));
    assert_eq!((landed["title"].as_str(), landed["titleRegeneration"].is_null()), (Some("Login Redirect Fix"), true));
    let call = capture(&calls, 1);
    let stdin = call["stdin"].as_str().unwrap();
    assert!(stdin.starts_with("Generate a title that will help the user recognize this T3 Code thread weeks later.\n") && stdin.ends_with("\n\nUser message:\nFix the login bug"), "{stdin}");
    assert_eq!(Path::new(call["cwd"].as_str().unwrap()), workspace);
    assert!(call["argv"].as_array().unwrap().iter().any(|arg| arg == "read-only"));

    // A rename from the thread's history keeps what the previous title said, in the original's words.
    until(&store, &mut events, &first, |view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed") && view["messages"].as_array().unwrap().iter().any(|message| message["role"] == "assistant" && message["streaming"] == false)).await;
    plan(&calls, 2, json!({"answer":{"title":"Auth Redirect Loop"}}));
    let exit = rpc(&mut socket, 2, "orchestration.dispatchCommand", json!({"type":"thread.metadata.update","commandId":"rename-1","threadId":first,"regenerateTitle":true})).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &first, |view| view["thread"]["title"] == "Auth Redirect Loop" && view["thread"]["titleRegeneration"].is_null()).await;
    assert_eq!(view["thread"]["title"], "Auth Redirect Loop");
    let stdin = capture(&calls, 2)["stdin"].as_str().unwrap().to_string();
    assert!(stdin.starts_with("Regenerate the title for an existing T3 Code thread so the user can recognize it weeks later.\nThe previous title was \"Login Redirect Fix\".\n"), "{stdin}");
    assert!(stdin.contains("\n\nThread contents:\nUSER:\nFix the login bug\n\nASSISTANT:\n"), "{stdin}");

    // A rename that fails is tried once, and the title stays.
    plan(&calls, 3, json!({"stderr":"model unavailable","exit":1}));
    let exit = rpc(&mut socket, 3, "orchestration.dispatchCommand", json!({"type":"thread.metadata.update","commandId":"rename-2","threadId":first,"regenerateTitle":true})).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &first, |view| view["thread"]["titleRegeneration"].is_null() && captures(&calls) == 3).await;
    assert_eq!(view["thread"]["title"], "Auth Redirect Loop");
    // An answer that is the same title, or none worth having, changes nothing either.
    for (number, answer) in [(4, json!({"title":" Auth Redirect Loop "})), (5, json!({"title":"   "}))] {
        plan(&calls, number, json!({"answer":answer}));
        let exit = rpc(&mut socket, 10 + number as u64, "orchestration.dispatchCommand", json!({"type":"thread.metadata.update","commandId":format!("rename-{number}"),"threadId":first,"regenerateTitle":true})).await;
        assert_eq!(exit["_tag"], "Success", "{exit}");
        let view = until(&store, &mut events, &first, |view| view["thread"]["titleRegeneration"].is_null() && captures(&calls) == number).await;
        assert_eq!(view["thread"]["title"], "Auth Redirect Loop", "answer {answer}");
    }

    // A first title is tried three times before the seed is kept.
    let second = launch("launch-2");
    plan(&calls, 6, json!({"exit":2,"stderr":"one"}));
    plan(&calls, 7, json!({"outputFile":"not json"}));
    plan(&calls, 8, json!({"answer":{"title":"Third Time Lucky"}}));
    let exit = rpc(&mut socket, 20, "orchestration.dispatchCommand", message(&second, "send-2", Some("Retry me"))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &second, |view| view["thread"]["titleRegeneration"].is_null() && view["thread"]["title"] != "Thread" && view["thread"]["title"] != "Retry me").await;
    assert_eq!(view["thread"]["title"], "Third Time Lucky");
    assert_eq!(captures(&calls), 8);

    let third = launch("launch-3");
    for number in 9..=11 {
        plan(&calls, number, json!({"exit":1,"stderr":"still down"}));
    }
    let exit = rpc(&mut socket, 21, "orchestration.dispatchCommand", message(&third, "send-3", Some("Never titled"))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &third, |view| view["thread"]["titleRegeneration"].is_null() && captures(&calls) == 11).await;
    assert_eq!(view["thread"]["title"], "Never titled", "the seed stays when no title could be written");
    assert_eq!(captures(&calls), 11, "three attempts and no more");

    // With every provider that can write text disabled there is nobody to ask: the seed stays, and
    // nothing is started (the thread runs on an ACP Registry agent, which cannot write text).
    providers.reconfigure(&settings(false, &calls), &base).await.unwrap();
    let fourth = launch_on("launch-4", "local-agent");
    let exit = rpc(&mut socket, 22, "orchestration.dispatchCommand", message(&fourth, "send-4", Some("No provider"))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &fourth, |view| view["thread"]["titleRegeneration"].is_null() && view["thread"]["title"] == "No provider").await;
    assert_eq!(view["thread"]["title"], "No provider");
    assert_eq!(captures(&calls), 11, "a disabled provider is never started");

    // A message without a seed does not title the thread.
    providers.reconfigure(&settings(true, &calls), &base).await.unwrap();
    let fifth = launch("launch-5");
    let exit = rpc(&mut socket, 23, "orchestration.dispatchCommand", message(&fifth, "send-5", None)).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let view = until(&store, &mut events, &fifth, |view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed") && !view["runs"].as_array().unwrap().is_empty()).await;
    assert_eq!((view["thread"]["title"].as_str(), view["thread"]["titleRegeneration"].is_null()), (Some("Thread"), true));
    assert_eq!(captures(&calls), 11);

    drop(socket);
    drop(shell);
    stop.send(()).unwrap();
    server.await.unwrap();
    execution.shutdown().await;
}

#[test]
fn a_launch_that_asks_for_a_generated_title_names_the_thread_after_its_first_message() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(async {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = base.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let calls = base.join("calls");
        std::fs::create_dir(&calls).unwrap();
        let store = Store::open(base.join("rust-state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
        let providers = ProviderRegistry::discover(&settings(true, &calls), &base).await.unwrap();
        providers.set_text_generation_options(Options { timeout: Duration::from_secs(60), retry_delay: Duration::ZERO });
        let execution = ExecutionService::start(store.clone(), providers.clone());
        plan(&calls, 1, json!({"answer":{"title":"\"Login Redirect Fix\"","needsRefinement":false}}));
        let mut events = store.subscribe();
        let launch = |id: &str, generate: bool| {
            ThreadLaunchService::with_providers(store.clone(), providers.clone())
                .launch(json!({"commandId":id,"threadId":id,"projectId":"project","title":"Fix the login bug","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"},"generateTitle":generate,"initialMessage":{"text":"Fix the login bug","attachments":[]}}), Utc::now())
                .unwrap()
        };
        // Without the request the launch title stays and nothing is asked of a model.
        launch("plain", false);
        until(&store, &mut events, "plain", |view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed") && !view["runs"].as_array().unwrap().is_empty()).await;
        assert_eq!(captures(&calls), 0);
        let launched = launch("generated", true);
        assert_eq!(launched["projection"]["thread"]["title"], "Fix the login bug");
        let view = until(&store, &mut events, "generated", |view| view["thread"]["title"] == "Login Redirect Fix" && view["thread"]["titleRegeneration"].is_null()).await;
        assert_eq!(view["thread"]["title"], "Login Redirect Fix");
        // The same two events as a first message sent later: the launch title armed the request, the model's title landed.
        let titled: Vec<Value> = store
            .read(|connection| Ok(connection.prepare("SELECT event_json FROM rust_application_events WHERE aggregate_id='generated' AND event_type='thread.metadata-updated' ORDER BY sequence")?.query_map([], |row| row.get::<_, String>(0))?.map(|json| Ok(serde_json::from_str::<Value>(&json?).unwrap())).collect::<rusqlite::Result<_>>()?))
            .unwrap();
        assert_eq!(titled.len(), 2);
        assert_eq!(titled[0]["payload"]["titleRegeneration"]["requestId"], "generated:initial-message");
        assert!(capture(&calls, 1)["stdin"].as_str().unwrap().ends_with("\n\nUser message:\nFix the login bug"));
        execution.shutdown().await;
    }));
}
