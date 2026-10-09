//! `git.runStackedAction` over a real socket: a stream of progress events and then the outcome.
//! Checks the write scope, what a client receives for a plain commit, what a failure part-way
//! leaves behind, the failure where a model has to write text and the server has no provider to do it
//! with (`text_generation_flow.rs` covers the writing itself), and what
//! the server does after a success (links the pull request to its thread, forgets what it held
//! for pull requests). GitHub is a local server and the remote a bare repository. This binary
//! installs one process-wide host, so everything runs in one test.
#![recursion_limit = "256"]
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{Method, StatusCode, Uri}, response::Response};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use source_control_support::*;
use std::sync::{Arc, Mutex};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    project::ProjectService,
    pull_request_service::PullRequestService,
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Prs = Arc<Mutex<Vec<Value>>>;

async fn serve(State(prs): State<Prs>, method: Method, uri: Uri, _body: Bytes) -> Response {
    let reply = |value: Value| Response::builder().status(StatusCode::OK).header("content-type", "application/json").body(axum::body::Body::from(value.to_string())).unwrap();
    let path = uri.path();
    if path.starts_with("/graphql/") {
        let nodes: Vec<Value> = prs.lock().unwrap().clone();
        return reply(json!({"data":{"repository":{"h0":{"nodes":nodes}}}}));
    }
    if method == Method::GET && path.ends_with("/repos/acme/web") {
        return reply(json!({"full_name":"acme/web","html_url":"https://github.com/acme/web","ssh_url":"git@github.com:acme/web.git","default_branch":"main"}));
    }
    Response::builder().status(StatusCode::NOT_FOUND).body(axum::body::Body::from("{}")).unwrap()
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// A request whose answer is a stream: every chunk is acknowledged, and the events and the exit
/// are returned.
async fn stream(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> (Vec<Value>, Value) {
    let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
    socket.send(Message::Text(frame.to_string().into())).await.unwrap();
    let mut events = vec![];
    loop {
        let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        match message["_tag"].as_str() {
            Some("Chunk") if message["requestId"] == id => {
                events.extend(message["values"].as_array().unwrap().iter().cloned());
                socket.send(Message::Text(json!({"_tag":"Ack","requestId":id}).to_string().into())).await.unwrap();
            }
            Some("Exit") if message["requestId"] == id => return (events, message["exit"].clone()),
            _ => {}
        }
    }
}

async fn unary(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
    socket.send(Message::Text(frame.to_string().into())).await.unwrap();
    loop {
        let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if message["_tag"] == "Exit" && message["requestId"] == id {
            return message["exit"].clone();
        }
    }
}

fn error_of(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["kind"].as_str().unwrap()).collect()
}

#[test]
fn stacked_actions_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let prs: Prs = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (stop_github, stopped_github) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn({
        let app = Router::new().fallback(serve).with_state(prs.clone());
        async move { axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped_github.await; }).await.unwrap() }
    });
    let cli = FakeCliDir::new();
    let resolver: t3_server::source_control_host::GitHubApiResolver = Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")));
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(resolver), &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    // The checkout pushes to a local bare repository and knows GitHub only by name.
    let remote = base.join("remote.git");
    let repo = base.join("repo");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    git(&repo, &["init", "--initial-branch=main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-m", "Initial commit"]);
    git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(&repo, &["remote", "add", "upstream", "https://github.com/acme/web.git"]);
    git(&repo, &["push", "-u", "origin", "main"]);
    git(&repo, &["checkout", "-b", "feature/x"]);

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Project web","workspaceRoot":repo.to_string_lossy()}), Utc::now())
        .unwrap();
    let threads = t3_server::thread::ThreadService::new(store.clone());
    let receipt = threads
        .dispatch(&json!({"type":"thread.create","commandId":"create-thread","threadId":"thread-1","projectId":"web","title":"Fix it","modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), Utc::now())
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_gsa".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"gsa-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap() });
    let mut read_only = connect(address, &reader).await;
    let mut socket = connect(address, &writer).await;
    let cwd = repo.to_string_lossy().into_owned();
    let action = |id: &str, extra: Value| {
        let mut payload = json!({"actionId":id,"cwd":cwd});
        payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        payload
    };

    // A connection that may not write is turned away before anything starts.
    let (events, exit) = stream(&mut read_only, 1, "git.runStackedAction", action("a1", json!({"action":"commit","commitMessage":"x"}))).await;
    assert!(events.is_empty());
    assert_eq!(error_of(&exit)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write");

    // A malformed request fails before it streams anything.
    for payload in [action("a2", json!({"action":"merge"})), action("a3", json!({"action":"commit","filePaths":[]})), action("   ", json!({"action":"commit"})), action("a4", json!({"action":"commit","commitMessage":"x".repeat(10_001)}))] {
        let (events, exit) = stream(&mut socket, 2, "git.runStackedAction", payload).await;
        assert!(events.is_empty());
        assert_eq!(exit["cause"][0]["_tag"], "Die", "a malformed request is a decode failure: {exit}");
    }

    // A commit with the caller's words: events in order, then the outcome, and the commit is there.
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    let (events, exit) = stream(&mut socket, 3, "git.runStackedAction", action("a5", json!({"action":"commit","commitMessage":"Add a\n\nBecause."}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    assert_eq!(kinds(&events)[..2], ["action_started", "phase_started"]);
    assert_eq!(*kinds(&events).last().unwrap(), "action_finished");
    assert!(events.iter().all(|event| event["actionId"] == "a5" && event["cwd"] == cwd.as_str() && event["action"] == "commit"));
    assert_eq!(events.last().unwrap()["result"]["commit"]["subject"], "Add a");
    assert_eq!(git(&repo, &["log", "-1", "--format=%B"]).trim(), "Add a\n\nBecause.");

    // Where a model has to write the message and this server has no provider to do it, the action
    // fails with a text generation error, and changes nothing.
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let (events, exit) = stream(&mut socket, 4, "git.runStackedAction", action("a6", json!({"action":"commit"}))).await;
    assert_eq!(kinds(&events), ["action_started", "phase_started", "action_failed"]);
    assert_eq!(events[1]["label"], "Generating commit message...");
    assert_eq!(events[2]["phase"], "commit");
    assert_eq!(events[2]["message"], "Text generation failed in generateCommitMessage: No provider is configured to write text.");
    assert_eq!(error_of(&exit)["_tag"], "TextGenerationError");
    assert_eq!(error_of(&exit)["operation"], "generateCommitMessage");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before, "nothing was committed");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "?? b.txt");

    // A push that lands, for a branch that already has a pull request: the thread learns of it, and
    // what the server holds about pull requests is dropped.
    prs.lock().unwrap().push(json!({"number":7,"title":"Existing 7","url":"https://github.com/acme/web/pull/7","baseRefName":"main","headRefName":"feature/x","headRefOid":"abc","state":"OPEN","isDraft":false,"mergedAt":null,"closedAt":null,"updatedAt":"2026-07-01T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}}));
    let epoch = PullRequestService::shared().listings_epoch();
    let (events, exit) = stream(&mut socket, 5, "git.runStackedAction", action("a7", json!({"action":"commit_push_pr","commitMessage":"Add b","threadId":"thread-1"}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let result = &events.last().unwrap()["result"];
    assert_eq!(result["pr"], json!({"status":"opened_existing","url":"https://github.com/acme/web/pull/7","number":7,"baseBranch":"main","headBranch":"feature/x","title":"Existing 7"}));
    assert_eq!(result["push"]["status"], "pushed");
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/feature/x"]), git(&repo, &["rev-parse", "HEAD"]), "the branch is on the remote");
    assert!(PullRequestService::shared().listings_epoch() > epoch, "a push drops what is held for pull requests");
    let linked = unary(&mut socket, 6, "pullRequests.linkedThreads", json!({"projectId":"web","repository":"acme/web","number":7})).await;
    assert_eq!(linked["value"]["threads"][0]["id"], "thread-1", "the pull request is linked to the thread it ran beside: {linked}");

    // A pull request that has to be written by a model nobody can run: everything before it stands.
    git(&repo, &["checkout", "-b", "feature/y"]);
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    let (events, exit) = stream(&mut socket, 7, "git.runStackedAction", action("a8", json!({"action":"commit_push_pr","commitMessage":"Add c"}))).await;
    let sent = kinds(&events);
    assert_eq!(sent[..3], ["action_started", "phase_started", "hook_output"], "{sent:?}");
    let failed = events.last().unwrap();
    assert_eq!((failed["kind"].as_str(), failed["phase"].as_str()), (Some("action_failed"), Some("pr")));
    assert_eq!(failed["message"], "Text generation failed in generatePrContent: No provider is configured to write text.");
    assert_eq!(error_of(&exit)["_tag"], "TextGenerationError");
    assert_eq!(error_of(&exit)["operation"], "generatePrContent");
    assert_eq!(git(&remote, &["log", "-1", "--format=%s", "refs/heads/feature/y"]), "Add c", "the commit and the push happened before the failure");

    drop(socket);
    drop(read_only);
    stop.send(()).unwrap();
    stop_github.send(()).unwrap();
    server.await.unwrap();
}
