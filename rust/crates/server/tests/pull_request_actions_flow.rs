//! The pull request actions over a real socket: merge, close, update-branch, approving a fork's
//! workflows, asking for reviewers and changing labels. GitHub is a local server that answers by
//! what is asked, so a test sees every request the server made. Each call is checked for what it
//! sends, for what it refuses before sending, and for what it makes the server forget. No network,
//! no real `gh`, no real credentials. This binary installs one process-wide host before the first
//! request, so everything runs in one test.
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
    pull_request_service::PullRequestService,
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Handler = Arc<dyn Fn(&Recorded) -> Step + Send + Sync>;

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
    /// What was sent since the last call, as a short description of each request.
    fn since(&self, seen: &mut usize) -> Vec<Recorded> {
        let all = self.requests();
        let fresh = all[*seen..].to_vec();
        *seen = all.len();
        fresh
    }
}

fn query_of(request: &Recorded) -> String {
    request.body.as_ref().and_then(|body| body["query"].as_str()).unwrap_or("").to_string()
}

fn variables_of(request: &Recorded) -> Value {
    request.body.as_ref().map(|body| body["variables"].clone()).unwrap_or(Value::Null)
}

fn number_of(request: &Recorded) -> u64 {
    variables_of(request)["number"].as_u64().or_else(|| query_of(request).split("pullRequest(number: ").nth(1).and_then(|rest| rest.split(')').next()?.parse().ok())).unwrap_or(0)
}

/// What the request is, in a word a test can compare.
fn kind_of(request: &Recorded) -> String {
    if !request.path.starts_with("/graphql/") {
        return format!("{} {}", request.method, request.path.trim_start_matches("/rest/github.com/"));
    }
    let query = query_of(request);
    let mutations = [
        ("closePullRequest", "close"),
        ("reopenPullRequest", "reopen"),
        ("markPullRequestReadyForReview", "ready"),
        ("convertPullRequestToDraft", "draft"),
        ("mergePullRequest", "merge"),
        ("enablePullRequestAutoMerge", "autoMerge"),
        ("disablePullRequestAutoMerge", "disableAutoMerge"),
        ("updatePullRequestBranch", "updateBranch"),
        ("revertPullRequest", "revert"),
    ];
    if query.starts_with("mutation") {
        return mutations.iter().find(|(needle, _)| query.contains(needle)).map_or("mutation", |(_, kind)| *kind).to_string();
    }
    if query.contains("query PullRequestActionState") {
        return "state".into();
    }
    if query.starts_with("query PullRequestSummaries") {
        return "summary".into();
    }
    let known = [
        (queries::VIEWER_PERMISSIONS_GRAPHQL_QUERY, "viewerPermissions"),
        (queries::PULL_REQUEST_NODE_ID_GRAPHQL_QUERY, "nodeId"),
        (queries::REVIEWER_CANDIDATES_GRAPHQL_QUERY, "reviewerCandidates"),
        (queries::LABEL_CANDIDATES_GRAPHQL_QUERY, "labelCandidates"),
        (queries::PULL_REQUEST_HEADS_GRAPHQL_QUERY, "heads"),
        (queries::CORE_GITHUB_COM, "core"),
    ];
    // The server asks for the rate-limit balance too, so a document is known by all but its last brace.
    known.iter().find(|(text, _)| query.starts_with(&text[..text.rfind('}').unwrap()])).map(|(_, name)| name.to_string()).unwrap_or_else(|| query.lines().find(|line| !line.is_empty()).unwrap_or("").to_string())
}

fn actor(login: &str) -> Value {
    json!({"__typename":"User","login":login,"avatarUrl":format!("https://avatars.example/{login}"),"name":login.to_uppercase(),"id":format!("ID_{login}")})
}

/// 99 is somebody with read access, 70 a fork's pull request, 71 a fork whose runs cannot be read.
fn core_pull_request(number: u64) -> Value {
    let fork = number == 70 || number == 71;
    let (permission, author) = if number == 99 { ("READ", false) } else { ("WRITE", false) };
    json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":true,"viewerPermission":permission,"pullRequest":{
        "number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"body":"Body","state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
        "reviewDecision":"REVIEW_REQUIRED","additions":10,"deletions":2,"changedFiles":3,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","mergedAt":null,"closedAt":null,
        "headRefName":"feature","baseRefName":"main","headRefOid":"abc1234","isCrossRepository":fork,"headRepositoryOwner":{"login":if fork {"Fork"} else {"acme"}},"author":actor("alice"),"autoMergeRequest":null,
        "viewerCanUpdate":permission == "WRITE" || author,"viewerDidAuthor":author,"viewerCanUpdateBranch":true,"baseRef":{"compare":{"behindBy":2}},
        "reviewRequests":{"nodes":[]},"labels":{"nodes":[{"name":"bug","color":"ff0000"}]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://ci/1","checkSuite":{"workflowRun":{"workflow":{"name":"CI"}}}}
        ],"pageInfo":{"hasNextPage":false}}}}}]}
    }}}})
}

fn summary_of(number: u64) -> Value {
    // 55 is a pull request that is merged by the time the merge is confirmed.
    let (state, merged_at) = if number == 55 { ("MERGED", json!("2026-01-03T00:00:00Z")) } else { ("OPEN", Value::Null) };
    json!({"data":{"s0":{"pullRequest":{"number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"state":state,"isDraft":false,"mergeable":"MERGEABLE","reviewDecision":"APPROVED",
        "additions":5,"deletions":1,"changedFiles":2,"updatedAt":"2026-01-02T00:00:00Z","mergedAt":merged_at,"closedAt":null,"headRefName":"feature","baseRefName":"main",
        "author":{"__typename":"User","login":"alice","avatarUrl":null,"name":"Alice"},"latestReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]},
        "stack":null}}}})
}

fn handler() -> Handler {
    Arc::new(|request| {
        let number = number_of(request);
        let json = |body: Value| Step::json(200, body);
        if request.path == "/rest/github.com/user" {
            return json(json!({"id":4242,"login":"octo"}));
        }
        if request.path.starts_with("/graphql/") {
            return match kind_of(request).as_str() {
                "viewerPermissions" => {
                    let (permission, author) = if number == 99 { ("READ", false) } else { ("WRITE", false) };
                    json(json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":true,"viewerPermission":permission,"pullRequest":{"viewerCanUpdate":permission == "WRITE" || author,"viewerDidAuthor":author}}}}))
                }
                // 44 is a pull request whose merge queue is on; 45 one already up to date.
                "state" => json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node","headRefOid":"def4567","isMergeQueueEnabled":number == 44,"mergeStateStatus":"BLOCKED","baseRef":{"compare":{"behindBy":if number == 45 { 0 } else { 2 }}}}}}})),
                "nodeId" => json(json!({"data":{"repository":{"pullRequest":{"id":"PR_node"}}}})),
                "core" => json(core_pull_request(number)),
                "summary" => {
                    let numbers: Vec<u64> = query_of(request).split("pullRequest(number: ").skip(1).filter_map(|rest| rest.split(')').next()?.parse().ok()).collect();
                    json(json!({"data":numbers.iter().enumerate().map(|(index, number)| (format!("s{index}"), summary_of(*number)["data"]["s0"].clone())).collect::<serde_json::Map<_, _>>()}))
                }
                "reviewerCandidates" => json(json!({"data":{"repository":{
                    "assignableUsers":{"pageInfo":{"hasNextPage":false},"nodes":[{"login":"alice","name":"Alice","avatarUrl":null},{"login":"bob","name":"Bob","avatarUrl":null},{"login":"carol","name":null,"avatarUrl":null}]},
                    "pullRequest":{"author":{"login":"alice"},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob","name":"Bob","avatarUrl":null}},{"requestedReviewer":{"slug":"core","name":"Core","avatarUrl":null}}]}}}}})),
                "labelCandidates" => json(json!({"data":{"repository":{
                    "labels":{"pageInfo":{"hasNextPage":false},"nodes":[{"name":"bug","color":"ff0000","description":"Broken"},{"name":"docs","color":null,"description":null}]},
                    "pullRequest":{"labels":{"nodes":[{"name":"bug"}]}}}}})),
                "heads" => {
                    // 70 is the fork's only open pull request for its head; 71 shares it with another.
                    let owner = json!({"login":"Fork"});
                    let target = CURRENT.lock().unwrap().clone();
                    let mut nodes = vec![json!({"number":target,"headRefOid":"abc1234","isCrossRepository":true,"headRepositoryOwner":owner})];
                    if target == 71 {
                        nodes.push(json!({"number":72,"headRefOid":"abc1234","isCrossRepository":true,"headRepositoryOwner":owner}));
                    }
                    json(json!({"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":nodes}}}}))
                }
                kind if mutation_kinds().contains(&kind) => json(json!({"data":{}})),
                other => Step::json(500, json!({"message":format!("unscripted document {other}")})),
            };
        }
        if request.path.contains("/actions/runs") && request.method == "GET" {
            return json(json!({"workflow_runs":[{"id":901,"name":"CI","html_url":"https://github.com/acme/web/actions/runs/901"}]}));
        }
        if request.method == "POST" && request.path.ends_with("/approve") {
            return json(json!({}));
        }
        if request.path.ends_with("/requested_reviewers") || request.path.contains("/labels") {
            return json(json!([]));
        }
        Step::json(500, json!({"message":format!("unscripted {} {}", request.method, request.path)}))
    })
}

fn mutation_kinds() -> [&'static str; 9] {
    ["close", "reopen", "ready", "draft", "merge", "autoMerge", "disableAutoMerge", "updateBranch", "revert"]
}

/// The pull request a head query is made for is the one the test last asked about; the head query
/// itself does not name it, so the handler reads it from here.
static CURRENT: Mutex<u64> = Mutex::new(0);

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

fn rpc<'a>(socket: &'a mut Socket, id: u64, tag: &'a str, payload: Value) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        if let Some(number) = payload["number"].as_u64() {
            *CURRENT.lock().unwrap() = number;
        }
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
fn pull_request_actions_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let github = Github::start(handler()).await;
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(github.resolver()), &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", "https://github.com/acme/web.git")]);
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Project web","workspaceRoot":web_path.to_string_lossy()}), Utc::now())
        .unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_pra".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"pra-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    let mut reader_socket = connect(address, &reader).await;
    let mut socket = connect(address, &writer).await;
    let reference = |number: u64| json!({"projectId":"web","repository":"acme/web","number":number});
    let with = |mut base: Value, extra: Value| {
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    let mut seen = 0;
    let mut merges = PullRequestService::shared().subscribe_merges();
    let mut id = 100;
    let mut call = |tag: &'static str, payload: Value| {
        id += 1;
        (id, tag, payload)
    };

    // -- a read-only connection is refused before anything is decided ------------------------------
    for (tag, extra) in [
        ("pullRequests.runAction", json!({"action":"close"})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"bob","kind":"user"}],"requested":true})),
        ("pullRequests.setLabels", json!({"labels":["bug"],"applied":true})),
    ] {
        let (n, tag, payload) = call(tag, with(reference(7), extra));
        let exit = rpc(&mut reader_socket, n, tag, payload).await;
        assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&exit)["requiredPermission"], "source-control:write");
    }
    assert!(github.since(&mut seen).is_empty(), "a refused write reaches nothing");

    // -- close, reopen, ready, draft: one node id, one mutation ----------------------------------------
    for (action, mutation) in [("close", "close"), ("reopen", "reopen"), ("ready", "ready"), ("draft", "draft"), ("disable-auto-merge", "disableAutoMerge"), ("revert", "revert")] {
        let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":action})));
        success(&rpc(&mut socket, n, tag, payload).await);
        let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
        assert_eq!(sent[0], "viewerPermissions", "{action}: the account's standing is read first");
        assert!(sent.contains(&mutation.to_string()), "{action}: {sent:?}");
    }

    // -- an account without the standing is turned down before any mutation --------------------------
    for (action, sentence) in [
        ("close", "You need write access on this repository, or to have opened this change request, to close it."),
        ("merge", "You need write access on this repository to merge."),
        ("revert", "You need write access on this repository to open a revert pull request."),
        ("approve-workflows", "You need write access on this repository to approve workflows from a fork pull request."),
    ] {
        let (n, tag, payload) = call("pullRequests.runAction", with(reference(99), json!({"action":action})));
        let exit = rpc(&mut socket, n, tag, payload).await;
        assert_eq!(failure(&exit)["detail"], sentence, "{action}");
        assert_eq!(failure(&exit)["operation"], "runAction");
        let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
        assert_eq!(sent, vec!["viewerPermissions"], "{action}: only the standing was read");
    }

    // -- what the host does not offer is refused without a request -----------------------------------------
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"merge","mergeMethod":"merge","stackNumber":3,"expectedStackHeads":[{"number":7,"headSha":"abc"}]})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "This stack action is not supported or has no expected head revision.");
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"merge","mergeMethod":"fast-forward"})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die", "a strategy outside the contract is a decode failure");
    assert!(github.since(&mut seen).is_empty(), "nothing was sent for a stack or a strategy the host does not have");

    // -- merge: the state is read fresh, the strategy named, the merge confirmed afterwards ------------
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"merge","mergeMethod":"squash"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent = github.since(&mut seen);
    assert_eq!(sent.iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions", "state", "merge", "summary"], "the merge is confirmed against the host");
    assert_eq!(variables_of(&sent[2]), json!({"input":{"pullRequestId":"PR_node","mergeMethod":"SQUASH"}}));
    assert!(merges.try_recv().is_err(), "a merge GitHub only accepted is not announced");
    // a merge queue takes it through auto-merge
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(44), json!({"action":"merge"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
    assert_eq!(sent, vec!["viewerPermissions", "state", "autoMerge", "summary"]);
    // arming auto-merge on a blocked pull request
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"enable-auto-merge","mergeMethod":"rebase"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent = github.since(&mut seen);
    assert_eq!(sent.iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions", "state", "autoMerge"], "arming is not confirmed as a merge");
    assert_eq!(variables_of(&sent[2]), json!({"input":{"pullRequestId":"PR_node","mergeMethod":"REBASE"}}));
    // a merge the host reports as done is announced to whoever settles the threads
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(55), json!({"action":"merge"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    github.since(&mut seen);
    let event = merges.try_recv().expect("the confirmed merge is announced");
    assert_eq!((event.project_id.as_str(), event.repository.as_str(), event.number), ("web", "acme/web", 55));

    // -- update-branch asks the heavier question, and does nothing for a branch that is current -------------
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"update-branch","updateMethod":"rebase"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent = github.since(&mut seen);
    assert_eq!(sent.iter().map(kind_of).collect::<Vec<_>>(), vec!["core", "state", "updateBranch"], "the comparison comes with the detail read");
    assert_eq!(variables_of(&sent[2]), json!({"pullRequestId":"PR_node","expectedHeadOid":"def4567","updateMethod":"REBASE"}));
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(45), json!({"action":"update-branch"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
    assert_eq!(sent, vec!["core", "state"], "already current is done");

    // -- a fork's workflows are approved one run at a time, and only for the head that was read ---------------
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(7), json!({"action":"approve-workflows"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
    assert_eq!(sent, vec!["viewerPermissions", "core"], "a pull request from the same repository has nothing to approve");
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(70), json!({"action":"approve-workflows"})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
    assert_eq!(
        sent,
        vec!["viewerPermissions", "core", "heads", "GET repos/acme/web/actions/runs", "core", "heads", "GET repos/acme/web/actions/runs", "POST repos/acme/web/actions/runs/901/approve"]
    );
    let (n, tag, payload) = call("pullRequests.runAction", with(reference(71), json!({"action":"approve-workflows"})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "The head revision matched 2 pull requests instead of uniquely matching #71.");
    let sent: Vec<String> = github.since(&mut seen).iter().map(kind_of).collect();
    assert!(!sent.iter().any(|kind| kind.starts_with("POST")), "an ambiguous head approves nothing: {sent:?}");

    // -- the fork's pending workflows are a check, and a count ---------------------------------------------------
    let (n, tag, payload) = call("pullRequests.detail", with(reference(70), json!({"allowStale":false})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    let detail = success(&exit);
    assert_eq!(detail["workflowApprovalsRequired"], 1);
    assert_eq!(detail["checks"][1], json!({"name":"CI","status":"action-required","description":"A maintainer must approve this workflow before it can run.","url":"https://github.com/acme/web/actions/runs/901"}));
    assert!(detail["viewerPermissions"]["actions"].as_array().unwrap().contains(&json!("approve-workflows")));
    assert!(detail["capabilities"]["actions"].as_array().unwrap().contains(&json!("approve-workflows")));
    let (n, tag, payload) = call("pullRequests.checks", with(reference(70), json!({"allowStale":false})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(success(&exit)["checks"].as_array().unwrap().len(), 2);
    github.since(&mut seen);

    // -- reviewers: the menu and the request share one gate ---------------------------------------------------------
    let (n, tag, payload) = call("pullRequests.reviewerCandidates", reference(7));
    let exit = rpc(&mut reader_socket, n, tag, payload).await;
    let list = success(&exit);
    assert_eq!(list["truncated"], false);
    assert_eq!(list["candidates"].as_array().unwrap().iter().map(|candidate| (candidate["id"].as_str().unwrap(), candidate["kind"].as_str().unwrap(), candidate["isRequested"].as_bool().unwrap())).collect::<Vec<_>>(), vec![("bob", "user", true), ("core", "team", true), ("carol", "user", false)], "the author is not a candidate");
    let (n, tag, payload) = call("pullRequests.reviewerCandidates", reference(99));
    let exit = rpc(&mut reader_socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "You need write access on this repository to ask for a review.");
    github.since(&mut seen);
    let (n, tag, payload) = call("pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"carol","kind":"user"},{"id":"core","kind":"team"}],"requested":true})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent = github.since(&mut seen);
    assert_eq!(sent.iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions", "POST repos/acme/web/pulls/7/requested_reviewers"]);
    assert_eq!(sent[1].body.clone().unwrap(), json!({"reviewers":["carol"],"team_reviewers":["core"]}));
    let (n, tag, payload) = call("pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"carol","kind":"user"}],"requested":false})));
    success(&rpc(&mut socket, n, tag, payload).await);
    assert_eq!(github.since(&mut seen)[1].method, "DELETE", "a request is taken back by the same call turned around");
    let (n, tag, payload) = call("pullRequests.requestReviewers", with(reference(99), json!({"reviewers":[{"id":"carol","kind":"user"}],"requested":true})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "You need write access on this repository to ask for a review.");
    assert_eq!(github.since(&mut seen).iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions"]);
    let (n, tag, payload) = call("pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[],"requested":true})));
    assert_eq!(rpc(&mut socket, n, tag, payload).await["cause"][0]["_tag"], "Die", "asking nobody is a decode failure");

    // -- labels --------------------------------------------------------------------------------------------------------------
    let (n, tag, payload) = call("pullRequests.labelCandidates", reference(7));
    let exit = rpc(&mut reader_socket, n, tag, payload).await;
    assert_eq!(success(&exit)["candidates"], json!([{"name":"bug","color":"ff0000","description":"Broken","isApplied":true},{"name":"docs","color":null,"description":null,"isApplied":false}]));
    let (n, tag, payload) = call("pullRequests.labelCandidates", reference(99));
    let exit = rpc(&mut reader_socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "You need triage access on this repository to change its labels.");
    github.since(&mut seen);
    let (n, tag, payload) = call("pullRequests.setLabels", with(reference(7), json!({"labels":["docs"],"applied":true})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let sent = github.since(&mut seen);
    assert_eq!((kind_of(&sent[1]), sent[1].body.clone().unwrap()), ("POST repos/acme/web/issues/7/labels".to_string(), json!({"labels":["docs"]})));
    let (n, tag, payload) = call("pullRequests.setLabels", with(reference(7), json!({"labels":["needs triage","bug"],"applied":false})));
    success(&rpc(&mut socket, n, tag, payload).await);
    assert_eq!(github.since(&mut seen).iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions", "DELETE repos/acme/web/issues/7/labels/needs%20triage", "DELETE repos/acme/web/issues/7/labels/bug"]);
    let (n, tag, payload) = call("pullRequests.setLabels", with(reference(99), json!({"labels":["bug"],"applied":true})));
    let exit = rpc(&mut socket, n, tag, payload).await;
    assert_eq!(failure(&exit)["detail"], "You need triage access on this repository to change its labels.");
    assert_eq!(github.since(&mut seen).iter().map(kind_of).collect::<Vec<_>>(), vec!["viewerPermissions"]);

    // -- a write makes the server forget what it held, and tells the readers -------------------------------------
    let (n, tag, payload) = call("pullRequests.detail", with(reference(7), json!({"allowStale":false})));
    success(&rpc(&mut socket, n, tag, payload).await);
    let before = github.since(&mut seen).len();
    assert!(before > 0);
    let (n, tag, payload) = call("pullRequests.detail", with(reference(7), json!({"allowStale":false})));
    success(&rpc(&mut socket, n, tag, payload).await);
    assert!(github.since(&mut seen).is_empty(), "a detail read is held");
    let (n, tag, payload) = call("pullRequests.setLabels", with(reference(7), json!({"labels":["docs"],"applied":true})));
    success(&rpc(&mut socket, n, tag, payload).await);
    github.since(&mut seen);
    let (n, tag, payload) = call("pullRequests.detail", with(reference(7), json!({"allowStale":false})));
    success(&rpc(&mut socket, n, tag, payload).await);
    assert!(!github.since(&mut seen).is_empty(), "the labels changed, so the detail is read again");

    // GraphQL documents the fixtures above rely on are the original's.
    assert!(queries::REVIEWER_CANDIDATES_GRAPHQL_QUERY.contains("assignableUsers"));

    drop(socket);
    drop(reader_socket);
    stop.send(()).unwrap();
    server.await.unwrap();
}
