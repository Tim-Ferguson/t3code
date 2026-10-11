#![cfg(unix)]
//! Text a model writes for `git.runStackedAction`, over a real socket: a commit message, a feature
//! branch name and a pull request title and description, written by the provider the settings
//! choose through a stand-in executable that records how it was started
//! (`fixtures/text-generation-provider.py`). GitHub is a local server and the remote a bare
//! repository; no provider CLI is run. A generation that fails fails the action at that step and
//! leaves everything before it done. This binary installs one process-wide host, so everything runs in
//! one test.
#![recursion_limit = "256"]
use t3_server::test_support::HANG_BOUND;
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{Method, StatusCode, Uri}, response::Response};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use source_control_support::*;
use std::{path::Path, sync::{Arc, Mutex}, time::Duration};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    provider_registry::ProviderRegistry,
    source_control_host::Host,
    text_generation_run::Options,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Github = Arc<Mutex<(Vec<Value>, Vec<Value>)>>;

/// Pull requests that exist, and the ones that were asked for.
async fn serve(State(github): State<Github>, method: Method, uri: Uri, body: Bytes) -> Response {
    let reply = |value: Value| Response::builder().status(StatusCode::OK).header("content-type", "application/json").body(axum::body::Body::from(value.to_string())).unwrap();
    let path = uri.path();
    if path.starts_with("/graphql/") {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let head = parsed["variables"]["h0"].as_str().unwrap_or("").to_string();
        let nodes: Vec<Value> = github.lock().unwrap().0.iter().filter(|pr| pr["headRefName"] == head.as_str()).cloned().collect();
        return reply(json!({"data":{"repository":{"h0":{"nodes":nodes}}}}));
    }
    if method == Method::POST && path.ends_with("/repos/acme/web/pulls") {
        let asked: Value = serde_json::from_slice(&body).unwrap();
        let head = asked["head"].as_str().unwrap().rsplit(':').next().unwrap().to_string();
        let mut state = github.lock().unwrap();
        state.1.push(asked.clone());
        state.0.push(json!({"number":42,"title":asked["title"],"url":"https://github.com/acme/web/pull/42","baseRefName":asked["base"],"headRefName":head,"headRefOid":"abc","state":"OPEN","isDraft":false,"mergedAt":null,"closedAt":null,"updatedAt":"2026-07-01T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}}));
        return reply(json!({"number":42,"html_url":"https://github.com/acme/web/pull/42"}));
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

async fn stream(socket: &mut Socket, id: u64, payload: Value) -> (Vec<Value>, Value) {
    let frame = json!({"_tag":"Request","id":id,"tag":"git.runStackedAction","payload":payload,"headers":[]});
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

/// The kinds of the events, leaving out git's own summary lines of a commit.
fn kinds(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["kind"].as_str().unwrap()).filter(|kind| *kind != "hook_output").collect()
}

fn error_of(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    &exit["cause"][0]["error"]
}

fn fake() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-generation-provider.py").to_string_lossy().into_owned()
}

/// Both built-in providers run the stand-in; each records into a directory of its own.
fn settings(codex: bool, claude: bool, codex_dir: &Path, claude_dir: &Path) -> t3_contracts::ServerSettings {
    let instance = |driver: &str, enabled: bool, delegate: &str, dir: &Path| {
        json!({"driver":driver,"enabled":enabled,"config":{"binaryPath":fake(),"homePath":dir.join("provider-home")},"environment":[{"name":"T3_TEXTGEN_DIR","value":dir},{"name":"T3_FAKE_DELEGATE","value":delegate}]})
    };
    serde_json::from_value(json!({"providerInstances":{"codex":instance("codex", codex, "codex", codex_dir),"claudeAgent":instance("claudeAgent", claude, "claude", claude_dir)}})).unwrap()
}

fn plan(dir: &Path, call: usize, plan: Value) {
    std::fs::write(dir.join(format!("{call:03}.plan.json")), plan.to_string()).unwrap();
}

fn capture(dir: &Path, call: usize) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{call:03}.capture.json"))).unwrap()).unwrap()
}

#[test]
fn text_written_for_stacked_actions_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let github: Github = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (stop_github, stopped_github) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn({
        let app = Router::new().fallback(serve).with_state(github.clone());
        async move { axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped_github.await; }).await.unwrap() }
    });
    let cli = FakeCliDir::new();
    let resolver: t3_server::source_control_host::GitHubApiResolver = Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")));
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(resolver), &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    // The checkout pushes to a local bare repository and knows GitHub only by name. The base branch
    // carries the repository's pull request template.
    let (remote, repo) = (base.join("remote.git"), base.join("repo"));
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(repo.join(".github")).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    git(&repo, &["init", "--initial-branch=main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join(".github/pull_request_template.md"), "## Template heading\n\n<!-- remove me -->\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "Initial commit"]);
    git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
    git(&repo, &["remote", "add", "upstream", "https://github.com/acme/web.git"]);
    git(&repo, &["push", "-u", "origin", "main"]);
    git(&repo, &["checkout", "-b", "feature/x"]);
    let cwd = repo.to_string_lossy().into_owned();

    let (codex_dir, claude_dir) = (base.join("codex-calls"), base.join("claude-calls"));
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::create_dir_all(&claude_dir).unwrap();
    let providers = ProviderRegistry::discover(&settings(true, true, &codex_dir, &claude_dir), &base).await.unwrap();
    providers.set_text_generation_options(Options { timeout: HANG_BOUND, retry_delay: Duration::ZERO });

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_tgs".into(), "loopback-browser".into()).unwrap();
    let writer = auth.issue_session("fixture", "bearer-access-token", vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"tgs-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: Some(providers.clone()), execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap() });
    let mut socket = connect(address, &writer).await;
    let action = |id: &str, extra: Value| {
        let mut payload = json!({"actionId":id,"cwd":cwd});
        payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        payload
    };

    // A commit with no message: the model writes it, from the staged files and patch.
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    plan(&codex_dir, 1, json!({"answer":{"subject":"Add a.txt.","body":"  - adds the file  "}}));
    let (events, exit) = stream(&mut socket, 1, action("a1", json!({"action":"commit"}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    assert_eq!(kinds(&events), ["action_started", "phase_started", "phase_started", "action_finished"], "{events:?}");
    assert_eq!(events[1]["label"], "Generating commit message...");
    assert_eq!(events[2]["label"], "Committing...");
    assert_eq!(events.last().unwrap()["result"]["commit"]["subject"], "Add a.txt");
    assert_eq!(git(&repo, &["log", "-1", "--format=%B"]).trim(), "Add a.txt\n\n- adds the file");
    // It was asked as the original asks: read-only, ephemeral, in the checkout, the prompt on stdin.
    let call = capture(&codex_dir, 1);
    let argv: Vec<&str> = call["argv"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(argv[0], "exec");
    for flag in ["--ephemeral", "--skip-git-repo-check", "--output-schema", "--output-last-message"] {
        assert!(argv.contains(&flag), "{flag} in {argv:?}");
    }
    assert!(argv.windows(2).any(|pair| pair == ["-s", "read-only"]) && argv.windows(2).any(|pair| pair == ["--model", "gpt-6-luna"]) && argv.last() == Some(&"-"), "{argv:?}");
    assert_eq!(Path::new(call["cwd"].as_str().unwrap()), std::fs::canonicalize(&repo).unwrap());
    let stdin = call["stdin"].as_str().unwrap();
    assert!(stdin.starts_with("You write concise git commit messages.\n") && stdin.contains("Branch: feature/x") && stdin.contains("Staged files:\nA\ta.txt") && stdin.contains("+a"), "{stdin}");
    assert!(call["schema"].as_str().unwrap().contains("\"required\":[\"subject\",\"body\"]"));

    // A model that fails fails the action at that step, and nothing is committed.
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    plan(&codex_dir, 2, json!({"stderr":"codex exploded","exit":1}));
    let (events, exit) = stream(&mut socket, 2, action("a2", json!({"action":"commit"}))).await;
    assert_eq!(kinds(&events), ["action_started", "phase_started", "action_failed"], "{events:?}");
    assert_eq!((events[2]["phase"].as_str(), events[2]["message"].as_str()), (Some("commit"), Some("Text generation failed in generateCommitMessage: Codex CLI command failed: codex exploded")));
    assert_eq!(error_of(&exit)["_tag"], "TextGenerationError");
    assert_eq!((error_of(&exit)["operation"].as_str(), error_of(&exit)["detail"].as_str()), (Some("generateCommitMessage"), Some("Codex CLI command failed: codex exploded")));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "?? b.txt");

    // Malformed and empty answers are failures too, not commits with made-up words.
    for (call, written, detail) in [(3, "not json", "Codex returned invalid structured output."), (4, "", "Codex returned invalid structured output.")] {
        plan(&codex_dir, call, json!({"outputFile":written}));
        let (_, exit) = stream(&mut socket, 10 + call as u64, action("bad", json!({"action":"commit"}))).await;
        assert_eq!(error_of(&exit)["detail"], detail);
    }
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);

    // A feature branch takes the model's name for it, made safe.
    plan(&codex_dir, 5, json!({"answer":{"subject":"Add b","body":"","branch":"Feature Add B!"}}));
    let (events, exit) = stream(&mut socket, 20, action("a3", json!({"action":"commit","featureBranch":true}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    assert_eq!(events.last().unwrap()["result"]["branch"], json!({"status":"created","name":"feature/feature-add-b"}));
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/feature-add-b");
    assert!(capture(&codex_dir, 5)["stdin"].as_str().unwrap().contains("Return a JSON object with keys: subject, body, branch."));
    git(&repo, &["checkout", "feature/x"]);
    git(&repo, &["branch", "-D", "feature/feature-add-b"]);

    // A pull request is created with the model's title and description, written to the repository's template.
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    plan(&codex_dir, 6, json!({"answer":{"title":"Add the c file\nsecond line","body":"## Template heading\n\n- done  "}}));
    let (events, exit) = stream(&mut socket, 21, action("a4", json!({"action":"commit_push_pr","commitMessage":"Add c"}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let result = &events.last().unwrap()["result"];
    assert_eq!((result["pr"]["status"].as_str(), result["pr"]["number"].as_u64(), result["pr"]["title"].as_str()), (Some("created"), Some(42), Some("Add the c file")));
    let asked = github.lock().unwrap().1.clone();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0]["title"].as_str(), asked[0]["body"].as_str(), asked[0]["base"].as_str()), (Some("Add the c file"), Some("## Template heading\n\n- done"), Some("main")));
    let stdin = capture(&codex_dir, 6)["stdin"].as_str().unwrap().to_string();
    assert!(stdin.starts_with("You write source control change request content.\n") && stdin.contains("Repository change request template:\n## Template heading\n\n<!-- remove me -->"), "{stdin}");
    assert!(stdin.contains("Base branch: main\nHead branch: feature/x") && stdin.contains("Commits:\n") && stdin.contains("Diff patch:\n"));
    assert!(events.iter().any(|event| event["label"] == "Generating PR content..."));

    // A model that never answers is given up on, and the child is killed.
    providers.set_text_generation_options(Options { timeout: Duration::from_millis(500), retry_delay: Duration::ZERO });
    std::fs::write(repo.join("d.txt"), "d\n").unwrap();
    plan(&codex_dir, 7, json!({"hang":true}));
    let (_, exit) = stream(&mut socket, 22, action("a5", json!({"action":"commit"}))).await;
    assert_eq!(error_of(&exit)["detail"], "Codex CLI request timed out.");
    providers.set_text_generation_options(Options { timeout: HANG_BOUND, retry_delay: Duration::ZERO });

    // With Codex disabled the first enabled provider writes, with its own default model.
    providers.reconfigure(&settings(false, true, &codex_dir, &claude_dir), &base).await.unwrap();
    plan(&claude_dir, 1, json!({"answer":{"subject":"Add d","body":""}}));
    let (_, exit) = stream(&mut socket, 23, action("a6", json!({"action":"commit"}))).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "Add d");
    let call = capture(&claude_dir, 1);
    let argv: Vec<&str> = call["argv"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    assert_eq!(argv[0], "-p");
    assert!(argv.windows(2).any(|pair| pair == ["--model", "claude-haiku-4-5"]) && argv.windows(2).any(|pair| pair == ["--tools", ""]) && argv.windows(2).any(|pair| pair == ["--permission-mode", "dontAsk"]), "{argv:?}");
    assert_eq!(std::fs::read_dir(&codex_dir).unwrap().filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().ends_with(".capture.json")).count(), 7, "Codex was not asked again");

    // With nothing enabled there is nobody to write, and the action says so.
    providers.reconfigure(&settings(false, false, &codex_dir, &claude_dir), &base).await.unwrap();
    std::fs::write(repo.join("e.txt"), "e\n").unwrap();
    let (events, exit) = stream(&mut socket, 24, action("a7", json!({"action":"commit"}))).await;
    assert_eq!(*kinds(&events).last().unwrap(), "action_failed");
    assert_eq!(error_of(&exit)["detail"], "Codex is disabled in T3 Code settings.");

    drop(socket);
    stop.send(()).unwrap();
    stop_github.send(()).unwrap();
    server.await.unwrap();
}
