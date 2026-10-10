//! The read half of the `pullRequests.*` methods over a real socket. GitHub is a local server
//! that answers by what is asked (the document, the path), so a test sees every request the
//! server made and how many. No network, no real `gh`, no real credentials. This binary
//! installs one process-wide host before the first request, so everything runs in one test.
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
    pull_request_queries as queries,
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// ---- a GitHub that answers by what it is asked ------------------------------------------------

type Handler = Arc<dyn Fn(&Recorded) -> Step + Send + Sync>;

/// The next search fails, as a host that could not answer one question about every repository.
static SEARCH_FAILS_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// How many of the next searches say nothing at all about `acme/web`, as one the host does not index.
static SEARCHES_WITHOUT_THE_REPOSITORY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

struct Github {
    origin: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone)]
struct GithubState {
    recorded: Arc<Mutex<Vec<Recorded>>>,
    handler: Handler,
}

async fn serve(State(state): State<GithubState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
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
    let step = (state.handler)(&recorded);
    state.recorded.lock().unwrap().push(recorded);
    let mut response = Response::builder().status(StatusCode::from_u16(step.status).unwrap()).header("content-type", "application/json");
    for (name, value) in &step.headers {
        response = response.header(name.as_str(), value.as_str());
    }
    response.body(axum::body::Body::from(step.body.unwrap_or_default())).unwrap()
}

impl Github {
    async fn start(handler: Handler) -> Github {
        let recorded = Arc::new(Mutex::new(vec![]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().fallback(serve).with_state(GithubState { recorded: recorded.clone(), handler });
        tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Github { origin, recorded, _stop: stop }
    }
    fn resolver(&self) -> t3_server::source_control_host::GitHubApiResolver {
        let origin = self.origin.clone();
        Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")))
    }
    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }
    /// The GraphQL documents asked so far, as their first line.
    fn documents(&self) -> Vec<String> {
        self.requests().iter().filter(|request| request.path.starts_with("/graphql/")).map(document_name).collect()
    }
}

fn query_of(request: &Recorded) -> String {
    request.body.as_ref().and_then(|body| body["query"].as_str()).unwrap_or("").to_string()
}

fn number_of(request: &Recorded) -> u64 {
    request.body.as_ref().and_then(|body| body["variables"]["number"].as_u64()).or_else(|| summary_numbers(&query_of(request)).first().copied()).unwrap_or(0)
}

fn document_name(request: &Recorded) -> String {
    let query = query_of(request);
    if query.starts_with("query PullRequestSummaries") {
        return "summary".into();
    }
    let known = [
        (queries::CORE_GITHUB_COM, "core"),
        (queries::SUMMARY_SINGLE_WITH_STACKS, "summary"),
        (queries::PULL_REQUEST_PREVIEW_GRAPHQL_QUERY, "preview"),
        (queries::PULL_REQUEST_ACTIVITY_GRAPHQL_QUERY, "activity"),
        (queries::REVIEW_THREADS_GRAPHQL_QUERY, "threads"),
        (queries::REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY, "threadComments"),
        (queries::PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY, "filesViewed"),
        (queries::VIEWER_PERMISSIONS_GRAPHQL_QUERY, "viewerPermissions"),
        (queries::CHECK_CONTEXTS_GITHUB_COM, "checkContexts"),
    ];
    // The server asks for the rate-limit balance too, so a document is known by all but its last brace.
    known.iter().find(|(text, _)| query.starts_with(&text[..text.rfind('}').unwrap()])).map(|(_, name)| name.to_string()).unwrap_or_else(|| query.lines().next().unwrap_or("").to_string())
}

fn actor(login: &str) -> Value {
    json!({"__typename":"User","login":login,"avatarUrl":format!("https://avatars.example/{login}"),"name":login.to_uppercase(),"id":format!("ID_{login}")})
}

fn core_pull_request(number: u64) -> Value {
    json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":false,"rebaseMergeAllowed":true,"viewerPermission":"WRITE","pullRequest":{
        "number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"body":"Body","state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
        "reviewDecision":"REVIEW_REQUIRED","additions":10,"deletions":2,"changedFiles":3,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"closedAt":null,
        "headRefName":"feature","baseRefName":"main","headRefOid":"abc1234","isCrossRepository":false,"headRepositoryOwner":{"login":"acme"},"author":actor("alice"),"autoMergeRequest":null,
        "viewerCanUpdate":true,"viewerDidAuthor":false,"viewerCanUpdateBranch":true,"baseRef":{"compare":{"behindBy":2}},
        "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob","name":"Bob"}}]},"labels":{"nodes":[{"name":"bug","color":"ff0000"}]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://ci/1","checkSuite":{"workflowRun":{"workflow":{"name":"CI"}}}},
            {"__typename":"CheckRun","name":"lint","status":"IN_PROGRESS","conclusion":null,"checkSuite":null}
        ],"pageInfo":{"hasNextPage":false}}}}}]}
    }}}})
}

/// The pull request numbers a batched summary document asks for, in alias order.
fn summary_numbers(query: &str) -> Vec<u64> {
    query.split("pullRequest(number: ").skip(1).filter_map(|rest| rest.split(')').next()?.parse().ok()).collect()
}

/// A batched summary read: every alias answered the way the single read was.
fn summaries_of(query: &str) -> Value {
    let mut data = serde_json::Map::new();
    for (index, number) in summary_numbers(query).into_iter().enumerate() {
        data.insert(format!("s{index}"), summary_of(number)["data"]["s0"].clone());
    }
    json!({"data":data})
}

fn summary_of(number: u64) -> Value {
    json!({"data":{"s0":{"pullRequest":{"number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"state":"OPEN","isDraft":true,"mergeable":"MERGEABLE","reviewDecision":"APPROVED",
        "additions":5,"deletions":1,"changedFiles":2,"updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"closedAt":null,"headRefName":"feature","baseRefName":"main",
        "author":{"__typename":"User","login":"alice","avatarUrl":null,"name":"Alice"},"latestReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]},
        "stack":null}}}})
}

fn activity_page() -> Value {
    json!({"data":{"repository":{"pullRequest":{"author":actor("alice"),
        "commits":{"nodes":[{"commit":{"oid":"c1","messageHeadline":"one","committedDate":"2026-01-01T00:00:00Z","authors":{"nodes":[{"name":"Al","email":"a@x","user":{"login":"alice"}}]}}}]},
        "comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"IC_1","body":"hi","createdAt":"2026-01-02T00:00:00Z","lastEditedAt":null,"url":"https://c/1","author":actor("bob")}]},
        "reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"R_1","body":"","state":"APPROVED","submittedAt":"2026-01-02T01:00:00Z","url":"https://r/1","author":actor("carol")}]}}}}})
}

fn threads_page() -> Value {
    json!({"data":{"viewer":{"login":"octo"},"repository":{"pullRequest":{
        "reviewThreads":{"totalCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"T_1","isResolved":false,"isOutdated":false,"path":"src/a.ts","line":12,"diffSide":"RIGHT",
            "comments":{"totalCount":1,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"RC_1","author":actor("bob"),"body":"nit","createdAt":"2026-01-02T00:30:00Z","url":"https://t/1","reactionGroups":[{"content":"THUMBS_UP","viewerHasReacted":true,"reactors":{"totalCount":2,"nodes":[{"login":"octo"},{"login":"zed"}]}}]}]}}]},
        "viewerCanUpdate":true,"viewerDidAuthor":false,"author":actor("alice"),"reactionGroups":[],"comments":{"nodes":[{"id":"IC_1","lastEditedAt":"2026-01-03T00:00:00Z","author":actor("bob"),"reactionGroups":[]}]},
        "reviews":{"nodes":[]},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob","name":"Bob","avatarUrl":"https://avatars.example/bob"}}]},"latestReviews":{"nodes":[]},
        "reviewDismissals":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},"commits":{"nodes":[{"commit":{"oid":"c1","messageHeadline":"one","committedDate":"2026-01-01T00:00:00Z","additions":5,"deletions":1,"parents":{"totalCount":1},"authors":{"nodes":[{"name":"Al","avatarUrl":"https://a","user":{"login":"alice"}}]}}}]}
    }}}})
}

fn handler() -> Handler {
    Arc::new(|request| {
        let number = number_of(request);
        let json = |body: Value| Step::json(200, body);
        if request.path == "/rest/github.com/user" {
            return json(json!({"id":4242,"login":"octo"}));
        }
        if request.path.starts_with("/graphql/") {
            if number == 404 {
                return json(json!({"data":{"repository":{"pullRequest":null}},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a PullRequest with the number of 404."}]}));
            }
            if number == 429 {
                return Step { status: 429, headers: vec![("retry-after".into(), "60".into())], body: Some("{}".into()) };
            }
            let query = query_of(request);
            if query.starts_with("query($owner: String!, $name: String!, $number: Int!) {\n  repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } }") {
                return json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node"}}}}));
            }
            if query.starts_with("query($owner: String!, $name: String!, $number: Int!) {\n  repository(owner: $owner, name: $name) {\n    mergeCommitAllowed") {
                // 99 is somebody with read access; 98 is somebody who opened the pull request.
                let (permission, author) = match number { 99 => ("READ", false), 98 => ("READ", true), _ => ("WRITE", false) };
                return json(json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":false,"viewerPermission":permission,"pullRequest":{"viewerCanUpdate":permission == "WRITE" || author,"viewerDidAuthor":author}}}}));
            }
            if query.starts_with("query($owner: String!, $name: String!, $number: Int!, $subjectId: ID!)") {
                let subject = request.body.as_ref().unwrap()["variables"]["subjectId"].as_str().unwrap_or("").to_string();
                let owner = if subject.contains("FOREIGN") { "PR_OTHER" } else { "PR_node" };
                return json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node"}},"node":{"id":subject,"pullRequest":{"id":owner}}}}));
            }
            if query.starts_with("mutation") {
                return json(json!({"data":{}}));
            }
            if query.starts_with("query($q: String!, $after: String)") {
                let q = request.body.as_ref().unwrap()["variables"]["q"].as_str().unwrap_or("").to_string();
                let row = |number: u64, updated: &str, repository: &str| {
                    json!({"number":number,"title":format!("PR {number}"),"url":format!("https://github.com/{repository}/pull/{number}"),"author":{"__typename":"User","login":"alice","avatarUrl":"https://avatars.example/alice","name":"Alice"},
                        "headRefName":"h","baseRefName":"main","state":"OPEN","isDraft":false,"mergeable":"MERGEABLE","reviewDecision":"APPROVED","latestReviews":{"nodes":[]},
                        "createdAt":"2026-01-01T00:00:00Z","updatedAt":updated,"mergedAt":null,"repository":{"nameWithOwner":repository},
                        "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"octo"}}]},"labels":{"nodes":[{"name":"bug","color":"f00"}]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}})
                };
                use std::sync::atomic::Ordering::SeqCst;
                if SEARCH_FAILS_ONCE.swap(false, SeqCst) {
                    return Step::json(500, json!({"message":"search is down"}));
                }
                let silent = SEARCHES_WITHOUT_THE_REPOSITORY.fetch_update(SeqCst, SeqCst, |left| left.checked_sub(1)).is_ok();
                let nodes = if silent {
                    vec![row(1, "2026-01-02T00:00:00Z", "acme/elsewhere")]
                } else if q.contains("updated:<=") {
                    vec![row(7, "2026-01-05T00:00:00Z", "acme/web"), row(6, "2026-01-03T00:00:00Z", "acme/web")]
                } else {
                    vec![row(7, "2026-01-05T00:00:00Z", "acme/web"), row(6, "2026-01-03T00:00:00Z", "acme/web"), row(1, "2026-01-02T00:00:00Z", "acme/elsewhere")]
                };
                return json(json!({"data":{"search":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":nodes}}}));
            }
            if query_of(request).starts_with("query {\n  s0: repository") {
                return json(json!({"data":{"s0":{"pullRequest":{"additions":5,"deletions":1}}}}));
            }
            if query_of(request).starts_with("query PullRequestsByHead") {
                let head = request.body.as_ref().unwrap()["variables"]["h0"].as_str().unwrap_or("").to_string();
                let nodes = if head == "feature" {
                    json!([{"number":7,"title":"Fix it","url":"https://github.com/acme/web/pull/7","baseRefName":"main","headRefName":"feature","headRefOid":"abc1234","state":"OPEN","isDraft":true,
                        "mergedAt":null,"closedAt":null,"updatedAt":"2026-01-02T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}}])
                } else {
                    json!([])
                };
                return json(json!({"data":{"repository":{"h0":{"nodes":nodes}}}}));
            }
            return match document_name(request).as_str() {
                "core" => json(core_pull_request(number)),
                "summary" if query_of(request).starts_with("query PullRequestSummaries") => json(summaries_of(&query_of(request))),
                "summary" => json(summary_of(number)),
                "preview" => json(json!({"data":{"repository":{"pullRequest":{"number":number,"title":"Fix it","url":"u","state":"OPEN","isDraft":false,"createdAt":"2026-01-01T00:00:00Z","author":{"__typename":"User","login":"alice","avatarUrl":null,"name":"Alice"}}}}})),
                "activity" => json(activity_page()),
                "threads" => json(threads_page()),
                "threadComments" => {
                    let same = request.body.as_ref().unwrap()["variables"]["threadId"] == "T_1";
                    json(json!({"data":{"viewer":{"login":"octo"},"repository":{"pullRequest":{"id":"PR_1"}},"node":{"pullRequest":{"id":if same {"PR_1"} else {"PR_OTHER"}},"comments":{"pageInfo":{"hasNextPage":false,"endCursor":"e"},"nodes":[{"id":"RC_5","author":actor("bob"),"body":"later","createdAt":"2026-01-03T00:00:00Z","url":null,"reactionGroups":[]}]}}}}))
                }
                "filesViewed" => json(json!({"data":{"repository":{"pullRequest":{"files":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"path":"a.ts","viewerViewedState":"VIEWED"},{"path":"b.ts","viewerViewedState":"UNVIEWED"}]}}}}})),
                other => Step::json(500, json!({"message":format!("unscripted document {other}")})),
            };
        }
        if request.method == "POST" && request.path.ends_with("/reviews") {
            return json(json!({"id":1}));
        }
        if request.path.ends_with("/pulls/7") && request.accept.as_deref() == Some("application/vnd.github.diff") {
            return Step { status: 200, headers: vec![], body: Some("diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n".into()) };
        }
        if request.path.ends_with("/pulls/9/files") || request.path.ends_with("/pulls/7/files") {
            return json(json!([{"filename":"a.ts","status":"modified","patch":"@@ -1 +1 @@\n-a\n+b","additions":1,"deletions":1}]));
        }
        if request.path.ends_with("/pulls/8") && request.accept.as_deref() == Some("application/vnd.github.diff") {
            // GitHub answers 406 past 300 changed files; the files API serves them instead.
            return Step { status: 406, headers: vec![], body: Some(json!({"message":"Sorry, the diff exceeded the maximum number of lines"}).to_string()) };
        }
        if request.path.ends_with("/pulls/8/files") {
            return json(json!([{"filename":"big.ts","status":"added","patch":"@@ -0,0 +1 @@\n+x","additions":1,"deletions":0}]));
        }
        if request.path.ends_with("/pulls/7") {
            return json(json!({"base":{"sha":"1111111"},"head":{"sha":"2222222"}}));
        }
        if request.path.contains("/contents/") {
            let revision = request.query.clone().unwrap_or_default();
            return Step { status: 200, headers: vec![], body: Some(format!("contents at {revision}\n")) };
        }
        if request.path.contains("/stacks") {
            return Step { status: 404, headers: vec![], body: Some("{}".into()) };
        }
        Step::json(500, json!({"message":format!("unscripted {} {}", request.method, request.path)}))
    })
}

// ---- the socket --------------------------------------------------------------------------------

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

fn rpc<'a>(socket: &'a mut Socket, id: u64, tag: &'a str, payload: Value) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
        socket.send(Message::Text(frame.to_string().into())).await.unwrap();
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
fn reads_pull_requests_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let github = Github::start(handler()).await;
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(github.resolver()), &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", "https://github.com/acme/web.git")]);
    let (_other, other_path) = repo_with_remotes(&[("origin", "https://github.example.com/acme/other.git")]);
    let (_gitlab, gitlab_path) = repo_with_remotes(&[("origin", "https://git.example.org/acme/lab.git")]);
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    for (id, path) in [("web", &web_path), ("other", &other_path), ("lab", &gitlab_path)] {
        ProjectService::new(store.clone())
            .mutate(json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":format!("Project {id}"),"workspaceRoot":path.to_string_lossy()}), Utc::now())
            .unwrap();
    }
    let threads = t3_server::thread::ThreadService::new(store.clone());
    let store_view = store.clone();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_pr".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"pr-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    let mut socket = connect(address, &reader).await;
    let reference = |number: u64| json!({"projectId":"web","repository":"acme/web","number":number,"allowStale":false});

    // -- summary: one GraphQL read, then held ----------------------------------------------------
    let exit = rpc(&mut socket, 1, "pullRequests.summary", reference(7)).await;
    let summary = success(&exit);
    assert_eq!(summary["provider"], "github");
    assert_eq!(summary["state"], "open");
    assert_eq!(summary["isDraft"], true);
    assert_eq!(summary["reviewDecision"], "approved");
    assert_eq!(summary["checksState"], "pending");
    assert_eq!(summary["author"]["avatarUrl"], "https://github.com/alice.png?size=80", "an actor with no picture gets the login-shaped one");
    assert_eq!(summary["stack"], Value::Null);
    assert!(summary["observedAt"].as_i64().unwrap() >= NOW_MS);
    assert_eq!(github.documents(), vec!["summary"]);
    let authorization = github.requests()[0].authorization.clone().unwrap();
    assert_eq!(authorization, "Bearer ghp_test_token_value");
    success(&rpc(&mut socket, 2, "pullRequests.summary", reference(7)).await);
    assert_eq!(github.documents(), vec!["summary"], "a pull request already read is not asked for again");

    // -- detail: the core read and the viewer, with the permissions that read yields -------------
    let exit = rpc(&mut socket, 3, "pullRequests.detail", reference(7)).await;
    let detail = success(&exit);
    assert_eq!(detail["title"], "Fix it");
    assert_eq!(detail["headBranch"], "feature");
    assert_eq!(detail["headSha"], "abc1234");
    assert_eq!(detail["viewer"], "octo");
    assert_eq!(detail["baseComparison"], "behind");
    assert_eq!(detail["behindBy"], 2);
    assert_eq!(detail["mergeCapabilities"], json!({"merge":true,"squash":false,"rebase":true}));
    assert_eq!(detail["reviewers"], json!([{"login":"bob","name":null,"avatarUrl":null}]));
    assert_eq!(detail["checks"], json!([
        {"name":"build","status":"success","description":null,"url":"https://ci/1"},
        {"name":"lint","status":"pending","description":null,"url":null}
    ]));
    assert_eq!(detail["viewerPermissions"]["actions"], json!(["merge","enable-auto-merge","disable-auto-merge","revert","approve-workflows","ready","draft","close","reopen","update-branch"]));
    assert_eq!(detail["viewerPermissions"]["updateMethods"], json!(["merge","rebase"]));
    assert_eq!(detail["capabilities"]["viewedFiles"], "host");
    assert!(github.documents().contains(&"core".to_string()));
    let requests_after_detail = github.requests().len();
    success(&rpc(&mut socket, 4, "pullRequests.detail", reference(7)).await);
    assert_eq!(github.requests().len(), requests_after_detail, "a detail read inside its minute is shared");

    // -- checks and preview ----------------------------------------------------------------------
    let exit = rpc(&mut socket, 5, "pullRequests.checks", reference(7)).await;
    assert_eq!(success(&exit)["state"], "open");
    assert_eq!(success(&exit)["checks"].as_array().unwrap().len(), 2);
    let exit = rpc(&mut socket, 6, "pullRequests.preview", reference(7)).await;
    assert_eq!(success(&exit)["title"], "Fix it", "{exit}");

    // -- activity: comments, reviews, threads, commits and the viewer's reactions ----------------
    let exit = rpc(&mut socket, 7, "pullRequests.activity", reference(7)).await;
    let activity = success(&exit);
    let ids: Vec<&str> = activity["comments"].as_array().unwrap().iter().map(|comment| comment["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["IC_1", "RC_1", "R_1"], "issue comments, line comments and reviews in time order");
    assert_eq!(activity["comments"][0]["editedAt"], "2026-01-03T00:00:00Z", "the edit time comes from the thread read");
    assert_eq!(activity["commentCount"], 3, "the issue comment and the review plus the host's count of line comments");
    assert_eq!(activity["commentsTruncated"], false);
    assert_eq!(activity["reviewThreads"][0]["path"], "src/a.ts");
    assert_eq!(activity["reviewThreads"][0]["commentCount"], 1);
    assert_eq!(activity["reviewThreads"][0]["comments"][0]["reactions"], json!([{"content":"thumbs-up","count":2,"actors":["zed"],"viewerHasReacted":true}]));
    assert_eq!(activity["commits"][0]["additions"], 5);
    assert_eq!(activity["commits"][0]["authors"][0]["avatarUrl"], "https://a", "the review read's picture wins over the login-shaped one");
    assert_eq!(activity["reviewers"][0]["login"], "bob");

    // -- a thread's remaining comments, refused for a thread of another pull request -------------
    let thread_input = |thread: &str| json!({"projectId":"web","repository":"acme/web","number":7,"threadId":thread,"cursor":"abc"});
    let exit = rpc(&mut socket, 8, "pullRequests.threadComments", thread_input("T_1")).await;
    assert_eq!(success(&exit)["comments"][0]["id"], "RC_5");
    assert_eq!(success(&exit)["nextCursor"], Value::Null);
    let exit = rpc(&mut socket, 9, "pullRequests.threadComments", thread_input("T_OTHER")).await;
    assert_eq!(failure(&exit)["_tag"], "PullRequestOperationError");
    assert_eq!(failure(&exit)["detail"], "The named subject did not belong to the named pull request.");

    // -- the diff over HTTP, files and a file's two sides ----------------------------------------
    let http = reqwest::Client::new();
    let diff = |number: u64, extra: Value| {
        let mut body = reference(number);
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        http.post(format!("http://{address}/api/pull-requests/diff")).bearer_auth(&reader).json(&body).send()
    };
    let response = diff(7, json!({})).await.unwrap();
    assert_eq!(response.status(), 200);
    let patch: Value = response.json().await.unwrap();
    assert_eq!(patch["truncated"], false);
    assert_eq!(patch["nextCursor"], Value::Null);
    assert!(patch["patch"].as_str().unwrap().starts_with("diff --git a/a.ts b/a.ts"));
    let response = diff(8, json!({})).await.unwrap();
    let paged: Value = response.json().await.unwrap();
    assert!(paged["patch"].as_str().unwrap().contains("diff --git a/big.ts b/big.ts\nnew file mode 100644"), "a diff GitHub refuses is built from the files API: {paged}");
    let response = diff(7, json!({"cursor":"nope"})).await.unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(response.json::<Value>().await.unwrap()["detail"], "The diff cursor was not one this pull request handed out.");
    let response = diff(7, json!({"commit":"not a sha"})).await.unwrap();
    assert_eq!(response.json::<Value>().await.unwrap()["detail"], "The named commit was not a commit sha.");
    let unauthenticated = http.post(format!("http://{address}/api/pull-requests/diff")).json(&reference(7)).send().await.unwrap();
    assert_eq!(unauthenticated.status(), 401);

    let contents = json!({"projectId":"web","repository":"acme/web","number":7,"changeType":"change","oldPath":"src/a b.ts","newPath":"src/a b.ts"});
    let exit = rpc(&mut socket, 10, "pullRequests.diffFileContents", contents).await;
    assert_eq!(success(&exit), &json!({"oldContents":"contents at ref=1111111\n","newContents":"contents at ref=2222222\n"}));
    assert!(github.requests().iter().any(|request| request.path.ends_with("/contents/src/a%20b.ts")), "path segments are percent-encoded");

    // -- files viewed -----------------------------------------------------------------------------
    let exit = rpc(&mut socket, 11, "pullRequests.filesViewed", reference(7)).await;
    assert_eq!(success(&exit), &json!({"files":[{"path":"a.ts","state":"viewed"},{"path":"b.ts","state":"unviewed"}],"truncated":false}));

    // -- stack: a host without the preview has none ----------------------------------------------
    let exit = rpc(&mut socket, 12, "pullRequests.stack", reference(7)).await;
    assert_eq!(success(&exit), &Value::Null);

    // -- routing identity ------------------------------------------------------------------------
    let exit = rpc(&mut socket, 13, "pullRequests.routingIdentity", json!({"host":"github.com"})).await;
    assert_eq!(success(&exit), &json!({"accountId":"4242","host":"github.com","provider":"github","viewer":"octo"}));
    let exit = rpc(&mut socket, 14, "pullRequests.routing", reference(7)).await;
    assert_eq!(success(&exit)["projectTitle"], "Project web");
    assert_eq!(success(&exit)["workspaceRoot"], web_path.to_string_lossy().as_ref());
    let mut routed = reference(7);
    routed["expectedAccountId"] = json!("1");
    routed["host"] = json!("github.com");
    let exit = rpc(&mut socket, 15, "pullRequests.summary", routed.clone()).await;
    assert_eq!(failure(&exit)["detail"], "The GitHub account could not be verified before starting the operation.");
    routed["expectedAccountId"] = json!("4242");
    success(&rpc(&mut socket, 16, "pullRequests.summary", routed).await);

    // -- failures keep their envelopes -----------------------------------------------------------
    let exit = rpc(&mut socket, 20, "pullRequests.summary", reference(404)).await;
    assert_eq!(failure(&exit)["_tag"], "PullRequestOperationError");
    assert_eq!(failure(&exit)["reason"], "not-found");
    let exit = rpc(&mut socket, 21, "pullRequests.summary", reference(429)).await;
    assert_eq!(failure(&exit)["_tag"], "PullRequestOperationError");
    // The batch is refused and recorded as a pause, so the read it falls back to is turned away by
    // the pause rather than sent: the answer names the pause, as the original's does.
    assert!(failure(&exit)["detail"].as_str().unwrap().contains("paused until the rate limit resets"), "{exit}");
    let again = github.requests().len();
    let exit = rpc(&mut socket, 22, "pullRequests.summary", reference(430)).await;
    assert!(failure(&exit)["detail"].as_str().unwrap().contains("paused"), "a rate-limited host is not asked again: {exit}");
    assert_eq!(github.requests().len(), again);
    clock.advance(std::time::Duration::from_secs(61));
    success(&rpc(&mut socket, 23, "pullRequests.summary", reference(431)).await);

    let exit = rpc(&mut socket, 24, "pullRequests.summary", json!({"projectId":"other","repository":"acme/other","number":1})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-missing","provider":"github"}), "no credential for that host");
    let exit = rpc(&mut socket, 25, "pullRequests.summary", json!({"projectId":"lab","repository":"acme/lab","number":1})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"provider-unsupported"}), "a host this server cannot read");
    let exit = rpc(&mut socket, 26, "pullRequests.summary", json!({"projectId":"web","repository":"someone/else","number":1})).await;
    assert_eq!(failure(&exit)["detail"], "The change request does not belong to the selected project.");
    let exit = rpc(&mut socket, 27, "pullRequests.summary", json!({"projectId":"web","repository":"acme/web","number":0})).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die", "a zero number is a decode failure: {exit}");

    // -- invalidation strands what is held and tells the readers ---------------------------------
    let mut refreshes = connect(address, &reader).await;
    refreshes.send(Message::Text(json!({"_tag":"Request","id":40,"tag":"pullRequests.subscribeRefreshes","payload":{},"headers":[]}).to_string().into())).await.unwrap();
    let before = github.documents().iter().filter(|name| *name == "activity").count();
    success(&rpc(&mut socket, 41, "pullRequests.invalidate", json!({"reference":reference(7)})).await);
    let notice = loop {
        let message: Value = serde_json::from_str(refreshes.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_ne!(message["_tag"], "Exit", "the stream ended: {message}");
        if message["_tag"] == "Chunk" {
            break message;
        }
    };
    assert!(notice["values"][0].as_u64().unwrap() > 0, "{notice}");
    success(&rpc(&mut socket, 42, "pullRequests.activity", reference(7)).await);
    assert_eq!(github.documents().iter().filter(|name| *name == "activity").count(), before + 1, "an invalidated read is made again");
    drop(refreshes);

    // -- listing: one search per host, narrowed by what was asked, carried on by a cursor ---------
    let search_requests = || github.requests().into_iter().filter(|request| query_of(request).starts_with("query($q: String!, $after: String)")).collect::<Vec<_>>();
    let exit = rpc(&mut socket, 80, "pullRequests.list", json!({"state":"open","involvement":"reviewing","limit":1,"filters":{"draft":"hide","labels":[["bug"]],"author":"alice"},"query":"fix \"it\""})).await;
    let page = success(&exit).clone();
    assert_eq!(page["viewers"], json!({"github.com":"octo"}));
    assert_eq!(page["entries"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["entries"][0]["number"], 7);
    assert_eq!(page["entries"][0]["repository"], "acme/web");
    assert_eq!(page["entries"][0]["viewerReviewRequested"], true);
    assert_eq!(page["entries"][0]["checksState"], "passing");
    assert_eq!(page["truncated"], true);
    assert_eq!(page["nextCursors"], json!({"github.com acme/web":"2026-01-05T00:00:00Z|1|7"}));
    assert_eq!(page["errors"], json!([{"projectId":"other","projectTitle":"Project other","message":"acme/other could not be read."}]), "the project on a host with no credential is explained, not dropped");
    let providers = page["providers"].as_array().unwrap();
    assert!(providers.iter().any(|provider| provider["host"] == "github.com" && provider["configured"] == true && provider["searchesOnHost"] == true));
    assert!(providers.iter().any(|provider| provider["host"] == "github.example.com" && provider["configured"] == false && provider["detail"].as_str().unwrap().contains("GitHub")));
    assert!(providers.iter().any(|provider| provider["host"] == "git.example.org" && provider["detail"] == "This host cannot be browsed here yet."));
    let search = search_requests().last().unwrap().body.clone().unwrap();
    let q = search["variables"]["q"].as_str().unwrap();
    for part in ["is:pr", "is:open", "review-requested:octo", "\"fix \\\"it\\\"\"", "label:\"bug\"", "author:\"alice\"", "draft:false", "sort:updated-desc", "repo:acme/web"] {
        assert!(q.contains(part), "{part} in {q}");
    }
    assert!(search["query"].as_str().unwrap().contains("first: 2"), "one row over the page size probes for a further page");
    let asked = search_requests().len();
    success(&rpc(&mut socket, 81, "pullRequests.list", json!({"state":"open","involvement":"reviewing","limit":1,"filters":{"draft":"hide","labels":[["bug"]],"author":"alice"},"query":"fix \"it\""})).await);
    assert_eq!(search_requests().len(), asked, "an identical listing is shared");
    // The next page names the repository to carry on, and the instant to carry on from.
    let exit = rpc(&mut socket, 82, "pullRequests.list", json!({"state":"open","limit":2,"cursors":{"github.com acme/web":"2026-01-05T00:00:00Z|1|7"}})).await;
    let second = success(&exit);
    assert_eq!(second["entries"].as_array().unwrap().iter().map(|entry| entry["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![6], "the row already sent at that instant is not sent again");
    assert!(search_requests().last().unwrap().body.as_ref().unwrap()["variables"]["q"].as_str().unwrap().contains("updated:<=2026-01-05T00:00:00Z"));
    let exit = rpc(&mut socket, 83, "pullRequests.list", json!({"state":"open","cursors":{"github.com acme/web":"nonsense"}})).await;
    assert_eq!(failure(&exit)["detail"], "The list could not be carried on from where it left off.");
    let exit = rpc(&mut socket, 84, "pullRequests.list", json!({"state":"open","limit":501})).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die");

    // A search that fails is read the long way: each repository on its own, so one question the host
    // could not answer does not report every repository as unreadable.
    use std::sync::atomic::Ordering::SeqCst;
    let asked = search_requests().len();
    SEARCH_FAILS_ONCE.store(true, SeqCst);
    let exit = rpc(&mut socket, 90, "pullRequests.list", json!({"state":"open","limit":3})).await;
    let page = success(&exit);
    assert_eq!(page["entries"].as_array().unwrap().iter().map(|entry| entry["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![7, 6], "{page}");
    assert_eq!(page["errors"].as_array().unwrap().len(), 1, "only the project with no credential is unreadable: {page}");
    assert_eq!(search_requests().len(), asked + 2, "the search, then the repository on its own");
    // A repository the search says nothing about is read on its own before the silence is believed:
    // once the last time a search named it is long ago.
    clock.advance(std::time::Duration::from_secs(11 * 60));
    let asked = search_requests().len();
    SEARCHES_WITHOUT_THE_REPOSITORY.store(1, SeqCst);
    let exit = rpc(&mut socket, 91, "pullRequests.list", json!({"state":"open","limit":4})).await;
    assert_eq!(success(&exit)["entries"].as_array().unwrap().iter().map(|entry| entry["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![7, 6], "{exit}");
    assert_eq!(search_requests().len(), asked + 2, "the search, then the repository it did not name");
    // Once a search has named it, its silence is believed: an empty answer costs no second read.
    success(&rpc(&mut socket, 92, "pullRequests.list", json!({"state":"open","limit":5})).await);
    let asked = search_requests().len();
    SEARCHES_WITHOUT_THE_REPOSITORY.store(1, SeqCst);
    let exit = rpc(&mut socket, 93, "pullRequests.list", json!({"state":"open","limit":6})).await;
    assert_eq!(success(&exit)["entries"].as_array().unwrap().iter().map(|entry| entry["number"].as_u64().unwrap()).collect::<Vec<_>>(), Vec::<u64>::new(), "{exit}");
    assert_eq!(search_requests().len(), asked + 1, "a repository known to be indexed is not asked for again");

    // Line counts arrive after the rows, in one aliased read, and are held for a minute.
    let stats_input = json!({"refs":[{"projectId":"web","repository":"acme/web","number":7},{"projectId":"web","repository":"someone/else","number":1}]});
    let exit = rpc(&mut socket, 85, "pullRequests.listStats", stats_input.clone()).await;
    assert_eq!(success(&exit), &json!({"stats":[{"projectId":"web","repository":"acme/web","number":7,"additions":5,"deletions":1}]}));
    let stats_requests = || github.requests().into_iter().filter(|request| query_of(request).starts_with("query {\n  s0: repository")).count();
    assert_eq!(stats_requests(), 1);
    success(&rpc(&mut socket, 86, "pullRequests.listStats", stats_input.clone()).await);
    assert_eq!(stats_requests(), 1);
    clock.advance(std::time::Duration::from_secs(61));
    success(&rpc(&mut socket, 87, "pullRequests.listStats", stats_input).await);
    assert_eq!(stats_requests(), 2, "counts older than a minute are read again");

    // -- the status of a checkout carries the pull request of its branch -------------------------
    git(&web_path, &["checkout", "-q", "-b", "feature"]);
    let lookups = || github.requests().iter().filter(|request| query_of(request).starts_with("query PullRequestsByHead")).count();
    let exit = rpc(&mut socket, 70, "vcs.refreshStatus", json!({"cwd":web_path.to_string_lossy()})).await;
    assert_eq!(
        success(&exit)["pr"],
        json!({"number":7,"title":"Fix it","url":"https://github.com/acme/web/pull/7","baseRef":"main","headRef":"feature","state":"open","isDraft":true,"updatedAt":"2026-01-02T00:00:00.000Z"})
    );
    assert_eq!(lookups(), 1, "the head is asked about once under its one selector");
    git(&web_path, &["checkout", "-q", "main"]);
    let exit = rpc(&mut socket, 71, "vcs.refreshStatus", json!({"cwd":web_path.to_string_lossy()})).await;
    assert_eq!(success(&exit)["pr"], Value::Null, "main has no pull request");

    // -- linked threads --------------------------------------------------------------------------
    let exit = rpc(&mut socket, 50, "pullRequests.linkedThreads", reference(7)).await;
    assert_eq!(success(&exit), &json!({"threads":[]}));
    let dispatch = |command: Value| {
        let receipt = threads.dispatch(&command, Utc::now()).unwrap();
        assert_eq!(receipt.status, "accepted", "{command}: {:?}", receipt.error);
    };
    dispatch(json!({"type":"thread.create","commandId":"create-thread","threadId":"thread-1","projectId":"web","title":"Fix it","modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}));
    // A link made the legacy way names the pull request by project, repository and number.
    dispatch(json!({"type":"thread.metadata.update","commandId":"link-legacy","threadId":"thread-1","linkedPullRequest":{"projectId":"web","repository":"acme/web","number":7,"url":"https://github.com/acme/web/pull/7"}}));
    let exit = rpc(&mut socket, 51, "pullRequests.linkedThreads", reference(7)).await;
    assert_eq!(success(&exit), &json!({"threads":[{"id":"thread-1","projectId":"web","title":"Fix it","archivedAt":null}]}));
    let projection = store_view.projection("thread", "thread-1").unwrap().unwrap();
    assert_eq!(projection["thread"]["pullRequests"][0]["host"], "github.com");
    assert_eq!(projection["thread"]["pullRequests"][0]["source"], "manual");
    // Linking what the thread already holds changes nothing; unlinking takes it out again.
    dispatch(json!({"type":"thread.pull-request.link","commandId":"link-again","threadId":"thread-1","host":"github.com","repository":"acme/web","number":7,"url":"https://github.com/acme/web/pull/7","source":"created"}));
    let projection = store_view.projection("thread", "thread-1").unwrap().unwrap();
    assert_eq!(projection["thread"]["pullRequests"].as_array().unwrap().len(), 1);
    assert_eq!(projection["thread"]["pullRequests"][0]["source"], "manual", "the thread already knew it");
    dispatch(json!({"type":"thread.pull-request.link","commandId":"link-created","threadId":"thread-1","host":"github.com","repository":"acme/web","number":8,"url":"https://github.com/acme/web/pull/8","source":"created"}));
    let exit = rpc(&mut socket, 52, "pullRequests.linkedThreads", reference(8)).await;
    assert_eq!(success(&exit)["threads"][0]["id"], "thread-1");
    dispatch(json!({"type":"thread.pull-request.unlink","commandId":"unlink-8","threadId":"thread-1","host":"github.com","repository":"acme/web","number":8}));
    let exit = rpc(&mut socket, 53, "pullRequests.linkedThreads", reference(8)).await;
    assert_eq!(success(&exit), &json!({"threads":[]}));
    dispatch(json!({"type":"thread.metadata.update","commandId":"unlink-legacy","threadId":"thread-1","linkedPullRequest":null}));
    let exit = rpc(&mut socket, 54, "pullRequests.linkedThreads", reference(7)).await;
    assert_eq!(success(&exit), &json!({"threads":[]}));

    // -- writes: each is gated by the host, by the account's standing, and by the write scope -----
    let mut write_socket = connect(address, &writer).await;
    let before_writes = github.requests().len();
    let mutations = || github.requests().into_iter().filter(|request| query_of(request).starts_with("mutation")).collect::<Vec<_>>();
    let writes = |number: u64| json!({"projectId":"web","repository":"acme/web","number":number});
    let with = |mut base: Value, extra: Value| {
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    // Every write is refused for a read-only connection before anything reaches the host.
    for (tag, extra) in [
        ("pullRequests.comment", json!({"body":"hi"})),
        ("pullRequests.update", json!({"title":"New"})),
        ("pullRequests.updateComment", json!({"commentId":"IC_1","kind":"issue-comment","body":"edited"})),
        ("pullRequests.submitReview", json!({"verdict":"comment","body":"ok","comments":[]})),
        ("pullRequests.replyToThread", json!({"threadId":"T_1","body":"reply"})),
        ("pullRequests.setThreadResolution", json!({"threadId":"T_1","resolved":true})),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true})),
        ("pullRequests.setFilesViewed", json!({"files":[{"path":"a.ts","viewed":true}]})),
        ("pullRequests.runAction", json!({"action":"merge"})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"bob","kind":"user"}],"requested":true})),
        ("pullRequests.setLabels", json!({"labels":["bug"],"applied":true})),
    ] {
        let exit = rpc(&mut socket, 200, tag, with(writes(7), extra)).await;
        assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}: {exit}");
        assert_eq!(failure(&exit)["requiredPermission"], "source-control:write", "{tag}");
    }
    assert_eq!(github.requests().len(), before_writes, "a refused write reaches nothing");

    // A remark is posted to the pull request's own node, after the account's standing is read.
    success(&rpc(&mut write_socket, 201, "pullRequests.comment", with(writes(7), json!({"body":"  Looks good.  "}))).await);
    let comment = mutations().pop().unwrap().body.unwrap();
    assert_eq!(comment["variables"], json!({"subjectId":"PR_node","body":"  Looks good.  "}), "markdown is sent as written");
    let exit = rpc(&mut write_socket, 202, "pullRequests.comment", with(writes(7), json!({"body":"   "}))).await;
    assert_eq!(failure(&exit)["detail"], "A comment cannot be empty.");
    let exit = rpc(&mut write_socket, 203, "pullRequests.comment", with(writes(7), json!({"body":"x".repeat(65_537)}))).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die", "an oversized body is a decode failure");
    // Writing changed the pull request: what was held for it is read again, and listings too.
    let activity_before = github.documents().iter().filter(|name| *name == "activity").count();
    success(&rpc(&mut write_socket, 204, "pullRequests.activity", reference(7)).await);
    assert_eq!(github.documents().iter().filter(|name| *name == "activity").count(), activity_before + 1);

    // Replying and resolving follow the standing of the account.
    success(&rpc(&mut write_socket, 205, "pullRequests.replyToThread", with(writes(7), json!({"threadId":"T_1","body":"Done."}))).await);
    assert_eq!(mutations().pop().unwrap().body.unwrap()["variables"], json!({"threadId":"T_1","body":"Done."}));
    let exit = rpc(&mut write_socket, 206, "pullRequests.replyToThread", with(writes(7), json!({"threadId":"T_1","body":""}))).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die");
    success(&rpc(&mut write_socket, 207, "pullRequests.setThreadResolution", with(writes(7), json!({"threadId":"T_1","resolved":true}))).await);
    assert!(query_of(&mutations().pop().unwrap()).contains("resolveReviewThread"));
    success(&rpc(&mut write_socket, 208, "pullRequests.setThreadResolution", with(writes(7), json!({"threadId":"T_1","resolved":false}))).await);
    assert!(query_of(&mutations().pop().unwrap()).contains("unresolveReviewThread"), "a resolution can be taken back");
    let asked = mutations().len();
    let exit = rpc(&mut write_socket, 209, "pullRequests.setThreadResolution", with(writes(99), json!({"threadId":"T_1","resolved":true}))).await;
    assert_eq!(failure(&exit)["detail"], "You need write access on this repository, or to have opened this change request, to resolve a review conversation.");
    success(&rpc(&mut write_socket, 210, "pullRequests.setThreadResolution", with(writes(98), json!({"threadId":"T_1","resolved":true}))).await);
    assert_eq!(mutations().len(), asked + 1, "the author may resolve, the stranger may not");

    // A review goes as one request; an author may not approve their own change.
    let exit = rpc(&mut write_socket, 211, "pullRequests.submitReview", with(writes(7), json!({"verdict":"request-changes","body":" ","comments":[]}))).await;
    assert_eq!(failure(&exit)["detail"], "A review needs a summary or at least one comment.");
    let exit = rpc(&mut write_socket, 212, "pullRequests.submitReview", with(writes(98), json!({"verdict":"approve","body":"","comments":[]}))).await;
    assert_eq!(failure(&exit)["detail"], "You need write access on this repository to approve a change request.");
    let review = json!({"verdict":"request-changes","body":"Please fix","comments":[
        {"path":"src/a.ts","position":{"kind":"added","newLine":3},"body":"here"},
        {"path":"src/a.ts","position":{"kind":"deleted","oldLine":9},"body":"gone"},
        {"path":"src/b.ts","position":{"kind":"context","oldLine":4,"newLine":6,"side":"left"},"body":"left side"}]});
    success(&rpc(&mut write_socket, 213, "pullRequests.submitReview", with(writes(7), review)).await);
    let posted = github.requests().into_iter().rev().find(|request| request.method == "POST" && request.path.ends_with("/pulls/7/reviews")).unwrap();
    assert_eq!(
        posted.body.unwrap(),
        json!({"event":"REQUEST_CHANGES","body":"Please fix","comments":[
            {"path":"src/a.ts","line":3,"side":"RIGHT","body":"here"},
            {"path":"src/a.ts","line":9,"side":"LEFT","body":"gone"},
            {"path":"src/b.ts","line":4,"side":"LEFT","body":"left side"}]})
    );

    // A subject the client names must belong to this pull request.
    let asked = mutations().len();
    let exit = rpc(&mut write_socket, 214, "pullRequests.updateComment", with(writes(7), json!({"commentId":"IC_FOREIGN","kind":"issue-comment","body":"hijack"}))).await;
    assert_eq!(failure(&exit)["detail"], "The named subject did not belong to the named pull request.");
    let exit = rpc(&mut write_socket, 215, "pullRequests.setReaction", with(writes(7), json!({"subjectId":"IC_FOREIGN","content":"rocket","reacted":true}))).await;
    assert_eq!(failure(&exit)["detail"], "The named subject did not belong to the named pull request.");
    assert_eq!(mutations().len(), asked, "nothing was written for a foreign subject");
    success(&rpc(&mut write_socket, 216, "pullRequests.updateComment", with(writes(7), json!({"commentId":"IC_1","kind":"review-comment","body":"edited"}))).await);
    let edit = mutations().pop().unwrap();
    assert!(query_of(&edit).contains("updatePullRequestReviewComment"));
    assert_eq!(edit.body.unwrap()["variables"], json!({"commentId":"IC_1","body":"edited"}));
    success(&rpc(&mut write_socket, 217, "pullRequests.setReaction", with(writes(7), json!({"subjectId":"IC_1","content":"thumbs-up","reacted":false}))).await);
    let reaction = mutations().pop().unwrap();
    assert!(query_of(&reaction).contains("removeReaction"));
    assert_eq!(reaction.body.unwrap()["variables"], json!({"subjectId":"IC_1","content":"THUMBS_UP"}));

    // The description is rewritten field by field; naming nothing is refused.
    success(&rpc(&mut write_socket, 218, "pullRequests.update", with(writes(7), json!({"title":"Better title"}))).await);
    assert_eq!(mutations().pop().unwrap().body.unwrap()["variables"], json!({"pullRequestId":"PR_node","title":"Better title"}), "a field not named is left out entirely");
    let exit = rpc(&mut write_socket, 219, "pullRequests.update", with(writes(7), json!({}))).await;
    assert_eq!(failure(&exit)["detail"], "Nothing was changed.");

    // Ticking files off is one aliased mutation, and forgets only the viewed marks.
    let core_reads = github.documents().iter().filter(|name| *name == "core").count();
    let viewed_reads = || github.documents().iter().filter(|name| *name == "filesViewed").count();
    let marks_before = viewed_reads();
    success(&rpc(&mut write_socket, 220, "pullRequests.filesViewed", reference(7)).await);
    success(&rpc(&mut write_socket, 221, "pullRequests.setFilesViewed", with(writes(7), json!({"files":[{"path":"a.ts","viewed":true},{"path":"b.ts","viewed":false}]}))).await);
    let marks = mutations().pop().unwrap().body.unwrap();
    assert!(marks["query"].as_str().unwrap().contains("f0: markFileAsViewed") && marks["query"].as_str().unwrap().contains("f1: unmarkFileAsViewed"));
    assert_eq!(marks["variables"], json!({"pullRequestId":"PR_node","path0":"a.ts","path1":"b.ts"}));
    success(&rpc(&mut write_socket, 222, "pullRequests.filesViewed", reference(7)).await);
    assert!(viewed_reads() >= marks_before + 1);
    assert_eq!(github.documents().iter().filter(|name| *name == "core").count(), core_reads, "ticking a file does not drop the detail");
    let exit = rpc(&mut write_socket, 223, "pullRequests.setFilesViewed", with(writes(7), json!({"files":(0..501).map(|n| json!({"path":format!("f{n}"),"viewed":true})).collect::<Vec<_>>()}))).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die");

    // -- a read-only connection is refused before anything is decided ------------------------------
    let exit = rpc(&mut socket, 60, "pullRequests.runAction", json!({"projectId":"web","repository":"acme/web","number":7,"action":"close"})).await;
    assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(failure(&exit)["requiredPermission"], "source-control:write");

    drop(socket);
    drop(write_socket);
    stop.send(()).unwrap();
    server.await.unwrap();
}
