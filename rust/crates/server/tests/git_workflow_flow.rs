//! `git.resolvePullRequest` and `git.preparePullRequestThread` over a real socket. The host's
//! API is a local server; the repository, its fork remote and the pull request's head ref are
//! local bare repositories, so nothing touches a network. This binary installs one process-wide
//! host before the first request, so everything runs in one test.
#![recursion_limit = "512"]
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::Response};
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
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Api {
    recorded: Arc<Mutex<Vec<Recorded>>>,
    head_sha: String,
    fork_path: String,
}

async fn serve(State(api): State<Api>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
    let raw_body = String::from_utf8_lossy(&body).into_owned();
    let recorded = Recorded {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        authorization: header("authorization"),
        accept: header("accept"),
        if_none_match: header("if-none-match"),
        body: serde_json::from_str(&raw_body).ok(),
        raw_body,
    };
    let query = recorded.body.as_ref().and_then(|body| body["query"].as_str()).unwrap_or("").to_string();
    let number = recorded.body.as_ref().and_then(|body| body["variables"]["number"].as_u64()).unwrap_or(0);
    api.recorded.lock().unwrap().push(recorded.clone());
    let step = if recorded.path.starts_with("/graphql/") && query.starts_with("query PullRequestByNumber") {
        if number == 404 {
            Step::json(200, json!({"data":{"repository":{"pullRequest":null}},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a PullRequest with the number of 404."}]}))
        } else {
            Step::json(200, json!({"data":{"repository":{"pullRequest":{
                "number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"baseRefName":"main","headRefName":"feature","headRefOid":api.head_sha,
                "state":"OPEN","isDraft":false,"mergedAt":null,"closedAt":null,"updatedAt":"2026-01-02T00:00:00Z","isCrossRepository":false,
                "headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}}}}}))
        }
    } else if recorded.path == "/rest/github.com/repos/acme/web" {
        Step::json(200, json!({"full_name":"acme/web","html_url":api.fork_path,"ssh_url":api.fork_path,"default_branch":"main"}))
    } else {
        Step::json(500, json!({"message":format!("unscripted {} {}", recorded.method, recorded.path)}))
    };
    let mut response = Response::builder().status(StatusCode::from_u16(step.status).unwrap()).header("content-type", "application/json");
    for (name, value) in &step.headers {
        response = response.header(name.as_str(), value.as_str());
    }
    response.body(axum::body::Body::from(step.body.unwrap_or_default())).unwrap()
}

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

fn rpc<'a>(socket: &'a mut Socket, id: u64, tag: &'a str, payload: Value) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}).to_string().into())).await.unwrap();
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            if value["_tag"] == "Exit" && value["requestId"] == id {
                return value["exit"].clone();
            }
        }
    })
}

fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}

fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}

#[test]
fn resolves_a_pull_request_and_prepares_a_worktree_for_it() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

    // The repository's remote: a bare repository holding main, the pull request's head branch and
    // its `refs/pull/7/head`. The checkout also knows the host's URL, which is how it is detected.
    let origin = base.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--bare", "-q"]);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["push", "-q", &path(&origin), "HEAD:refs/heads/main"]);
    git(&seed, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "feature\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "feature work"]);
    let head_sha = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["push", "-q", &path(&origin), "HEAD:refs/heads/feature", &format!("{head_sha}:refs/pull/7/head")]);
    let checkout = base.join("checkout");
    git(&base, &["clone", "-q", &path(&origin), &path(&checkout)]);
    git(&checkout, &["remote", "add", "github", "https://github.com/acme/web.git"]);

    let api_state = Api { recorded: Arc::default(), head_sha: head_sha.clone(), fork_path: path(&origin) };
    let recorded = api_state.recorded.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().fallback(serve).with_state(api_state)).await.unwrap() });
    let resolver: t3_server::source_control_host::GitHubApiResolver =
        Arc::new(move |host| (format!("{origin_url}/rest/{host}"), format!("{origin_url}/graphql/{host}")));
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(resolver), &base);
    assert!(Host::install(test.host.clone()));

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"create","projectId":"web","title":"Web","workspaceRoot":path(&checkout)}), Utc::now())
        .unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_gw".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"gw-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let mut socket = connect(address, &writer).await;

    // -- resolving: `#7` means `7`, and the answer is the host's -----------------------------------
    let cwd = path(&checkout);
    let exit = rpc(&mut socket, 1, "git.resolvePullRequest", json!({"cwd":cwd,"reference":"#7"})).await;
    assert_eq!(
        success(&exit),
        &json!({"pullRequest":{"number":7,"title":"Fix it","url":"https://github.com/acme/web/pull/7","baseBranch":"main","headBranch":"feature","state":"open"}})
    );
    let exit = rpc(&mut socket, 2, "git.resolvePullRequest", json!({"cwd":cwd,"reference":"404"})).await;
    assert_eq!(failure(&exit)["_tag"], "SourceControlProviderError");
    assert_eq!(failure(&exit)["operation"], "getChangeRequest");
    let exit = rpc(&mut socket, 3, "git.resolvePullRequest", json!({"cwd":path(&base.join("not-a-repo")),"reference":"7"})).await;
    assert_eq!(failure(&exit)["_tag"], "GitManagerError", "{exit}");
    let exit = rpc(&mut socket, 4, "git.resolvePullRequest", json!({"cwd":cwd,"reference":"  "})).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die", "an empty reference is a decode failure");

    // -- preparing a worktree thread: the head becomes a branch in a worktree of its own -----------
    let exit = rpc(&mut socket, 5, "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    let prepared = success(&exit).clone();
    assert_eq!(prepared["branch"], "feature");
    assert_eq!(prepared["isOnPullRequestHead"], true);
    assert_eq!(prepared["pullRequest"]["number"], 7);
    let worktree = prepared["worktreePath"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&worktree).join("feature.txt").exists(), "the worktree holds the pull request's files");
    assert_eq!(git(std::path::Path::new(&worktree), &["rev-parse", "HEAD"]), head_sha);
    assert_eq!(git(std::path::Path::new(&worktree), &["rev-parse", "--abbrev-ref", "feature@{upstream}"]), "origin/feature", "tracking is set to the remote that holds the head (the fork URL is the one origin already has)");
    assert!(git(&checkout, &["worktree", "list", "--porcelain"]).contains(&worktree));
    assert_eq!(git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]), "main", "the main checkout is untouched");

    // The same request again reuses the worktree rather than making another.
    let exit = rpc(&mut socket, 6, "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    assert_eq!(success(&exit)["worktreePath"], worktree.as_str());
    assert_eq!(success(&exit)["isOnPullRequestHead"], true);
    assert_eq!(git(&checkout, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 2);

    // A pull request the host does not know creates nothing.
    let exit = rpc(&mut socket, 7, "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"404","mode":"worktree"})).await;
    assert_eq!(failure(&exit)["_tag"], "SourceControlProviderError");
    assert_eq!(git(&checkout, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 2);

    // -- the read-only connection may resolve, and may not prepare ----------------------------------
    let mut read_only = connect(address, &reader).await;
    success(&rpc(&mut read_only, 10, "git.resolvePullRequest", json!({"cwd":cwd,"reference":"7"})).await);
    let before = recorded.lock().unwrap().len();
    let exit = rpc(&mut read_only, 11, "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(failure(&exit)["requiredPermission"], "source-control:write");
    assert_eq!(recorded.lock().unwrap().len(), before, "a refused request reaches nothing");
}
