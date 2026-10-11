#![cfg(unix)]
#![recursion_limit = "512"]
//! The pull request sync end to end: threads on the real store and execution service with the scripted fake Codex, linked to pull
//! requests on a GitHub that is a local server answering from a scenario the test edits, and the manual clock the host reads time
//! from. A pass is `sweep()` after the clock advances, so nothing waits on a sleep. The timer `main.rs` starts is shown to sweep at
//! once. No network, no real `gh`, no real credentials. This binary installs one process-wide host, so everything runs in one test.
mod mcp_support;
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::Response};
use mcp_support::Harness;
use serde_json::{Value, json};
use source_control_support::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_server::pull_request_queries as queries;
use t3_server::{
    project::ProjectService,
    pull_request_service::{Env, PullRequestService},
    pull_request_sync::PullRequestSync,
    source_control_host::Host,
    thread::ThreadService,
};
use t3_vcs::Clock as _;

/// What the fake GitHub says right now, per pull request number.
#[derive(Clone)]
struct Remote {
    /// The host answers 502 to every request about it.
    failing: bool,
    title: &'static str,
    head: String,
    state: &'static str,
    mergeable: &'static str,
    /// (name, status, conclusion)
    checks: Vec<(&'static str, &'static str, Option<&'static str>)>,
    /// (id, author, createdAt, body)
    comments: Vec<(&'static str, &'static str, &'static str, &'static str)>,
    /// A 429 with this retry-after, for every GraphQL request.
    limited: Option<u32>,
    /// The balance every GraphQL answer reports.
    remaining: u32,
}

impl Default for Remote {
    fn default() -> Self {
        Remote { failing: false, title: "Fix it", head: "aaa1111".into(), state: "OPEN", mergeable: "MERGEABLE", checks: vec![("lint", "IN_PROGRESS", None)], comments: vec![], limited: None, remaining: 4_000 }
    }
}

type Remotes = Arc<Mutex<std::collections::HashMap<u64, Remote>>>;

struct Github {
    origin: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone)]
struct GithubState {
    recorded: Arc<Mutex<Vec<Recorded>>>,
    remotes: Remotes,
}

fn actor(login: &str) -> Value {
    json!({"__typename":"User","login":login,"avatarUrl":format!("https://avatars.example/{login}"),"name":login,"id":format!("ID_{login}")})
}

fn answer(remotes: &Remotes, request: &Recorded) -> Step {
    let json = |body: Value| Step::json(200, body);
    if request.path == "/rest/github.com/user" {
        return json(json!({"id":4242,"login":"octo"}));
    }
    let query = request.body.as_ref().and_then(|body| body["query"].as_str()).unwrap_or("").to_owned();
    let number = request.body.as_ref().and_then(|body| body["variables"]["number"].as_u64()).unwrap_or(0);
    let remote = remotes.lock().unwrap().get(&number).cloned().unwrap_or_default();
    if remote.failing {
        return Step::json(502, json!({"message":"bad gateway"}));
    }
    if let Some(retry) = remote.limited {
        return Step { status: 429, headers: vec![("retry-after".into(), retry.to_string())], body: Some("{}".into()) };
    }
    let rate_limit = json!({"cost":1,"limit":5000,"remaining":remote.remaining,"resetAt":"2026-10-05T18:00:00Z"});
    let with_limit = |mut body: Value| {
        body["data"]["rateLimit"] = rate_limit.clone();
        json(body)
    };
    // The server appends a rate-limit selection to each document, so one is known by all but its last brace.
    let is = |document: &str| query.starts_with(&document[..document.rfind('}').unwrap()]);
    if is(queries::SUMMARY_SINGLE_WITH_STACKS) {
        let merged = remote.state == "MERGED";
        return with_limit(json!({"data":{"s0":{"repository":null,"pullRequest":{"number":number,"title":remote.title,"url":format!("https://github.com/acme/web/pull/{number}"),"state":remote.state,"isDraft":false,"mergeable":remote.mergeable,"reviewDecision":"REVIEW_REQUIRED",
            "additions":1,"deletions":1,"changedFiles":1,"updatedAt":if merged { "2026-10-05T13:00:00Z" } else { "2026-10-05T12:00:00Z" },"mergedAt":if merged { json!("2026-10-05T13:00:00Z") } else { Value::Null },"closedAt":Value::Null,"headRefName":"feature","baseRefName":"main",
            "author":actor("octo"),"latestReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"PENDING"}}}]},"stack":null}}}}));
    }
    if is(queries::CORE_GITHUB_COM) {
        let contexts: Vec<Value> = remote
            .checks
            .iter()
            .map(|(name, status, conclusion)| json!({"__typename":"CheckRun","name":name,"status":status,"conclusion":conclusion,"detailsUrl":format!("https://ci.example/{name}"),"checkSuite":null}))
            .collect();
        return with_limit(json!({"data":{"repository":{"mergeCommitAllowed":true,"squashMergeAllowed":true,"rebaseMergeAllowed":true,"viewerPermission":"WRITE","pullRequest":{
            "number":number,"title":"Fix it","url":format!("https://github.com/acme/web/pull/{number}"),"body":"Body","state":remote.state,"isDraft":false,"mergeable":remote.mergeable,
            "reviewDecision":"REVIEW_REQUIRED","additions":1,"deletions":1,"changedFiles":1,"createdAt":"2026-10-01T00:00:00Z","updatedAt":"2026-10-05T12:00:00Z","mergedAt":null,"closedAt":null,
            "headRefName":"feature","baseRefName":"main","headRefOid":remote.head,"isCrossRepository":false,"headRepositoryOwner":{"login":"acme"},"author":actor("octo"),"autoMergeRequest":null,
            "viewerCanUpdate":true,"viewerDidAuthor":true,"viewerCanUpdateBranch":true,"baseRef":{"compare":{"behindBy":0}},"reviewRequests":{"nodes":[]},"labels":{"nodes":[]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":contexts,"pageInfo":{"hasNextPage":false}}}}}]}}}}}));
    }
    if is(queries::PULL_REQUEST_ACTIVITY_GRAPHQL_QUERY) {
        let nodes: Vec<Value> = remote.comments.iter().map(|(id, author, at, body)| json!({"id":id,"body":body,"createdAt":at,"lastEditedAt":null,"url":format!("https://c/{id}"),"author":actor(author)})).collect();
        return with_limit(json!({"data":{"repository":{"pullRequest":{"author":actor("octo"),"commits":{"nodes":[]},
            "comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":nodes},"reviews":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}}));
    }
    if is(queries::REVIEW_THREADS_GRAPHQL_QUERY) {
        return with_limit(json!({"data":{"viewer":{"login":"octo"},"repository":{"pullRequest":{"reviewThreads":{"totalCount":0,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},
            "viewerCanUpdate":true,"viewerDidAuthor":true,"author":actor("octo"),"reactionGroups":[],"comments":{"nodes":[]},"reviews":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[]},
            "reviewDismissals":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]},"commits":{"nodes":[]}}}}}));
    }
    Step::json(500, json!({"message":format!("unscripted document: {}", query.lines().next().unwrap_or(""))}))
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
    let step = answer(&state.remotes, &recorded);
    state.recorded.lock().unwrap().push(recorded);
    let mut response = Response::builder().status(StatusCode::from_u16(step.status).unwrap()).header("content-type", "application/json");
    for (name, value) in &step.headers {
        response = response.header(name.as_str(), value.as_str());
    }
    response.body(axum::body::Body::from(step.body.unwrap_or_default())).unwrap()
}

impl Github {
    async fn start(remotes: Remotes) -> Github {
        let recorded = Arc::new(Mutex::new(vec![]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().fallback(serve).with_state(GithubState { recorded: recorded.clone(), remotes });
        tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Github { origin, recorded, _stop: stop }
    }
    fn resolver(&self) -> t3_server::source_control_host::GitHubApiResolver {
        let origin = self.origin.clone();
        Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")))
    }
    /// GraphQL requests so far, by the pull request number they name.
    fn graphql(&self, number: u64) -> usize {
        self.recorded.lock().unwrap().iter().filter(|request| request.path.starts_with("/graphql/") && request.body.as_ref().and_then(|body| body["variables"]["number"].as_u64()) == Some(number)).count()
    }
    fn graphql_total(&self) -> usize {
        self.recorded.lock().unwrap().iter().filter(|request| request.path.starts_with("/graphql/")).count()
    }
}


/// The launch command of a thread whose first turn runs to completion on the scripted Codex.
#[allow(non_snake_case)]
fn IDLE_LAUNCH(id: &str) -> Value {
    json!({"commandId":format!("launch-{id}"),"threadId":id,"projectId":"web","title":id,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"},"initialMessage":{"messageId":format!("first-{id}"),"text":"finish this","attachments":[]}})
}

async fn settled(h: &Harness, id: &str) {
    h.until(id, |view| (view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed")).then_some(())).await
}

fn link_of(view: &Value, number: u64) -> Value {
    view["thread"]["pullRequests"].as_array().unwrap().iter().find(|link| link["number"] == number).cloned().unwrap_or(Value::Null)
}

#[test]
fn every_linked_pull_request_snapshot_is_kept_current() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let remotes: Remotes = Arc::default();
    let github = Github::start(remotes.clone()).await;
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(github.resolver()), &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");
    let (_repo, repo_path) = repo_with_remotes(&[("origin", "https://github.com/acme/web.git")]);

    let h = Harness::new().await;
    ProjectService::new(h.store.clone()).mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Web","workspaceRoot":repo_path.to_string_lossy(),"defaultModelSelection":{"instanceId":"codex","model":"fixture-model"}}), clock.utc_now()).unwrap();
    let threads = ThreadService::new(h.store.clone());
    // Built as `main.rs` builds it: the execution service, the host's environment and the shared pull request service.
    let sync = PullRequestSync::new(h.execution.clone(), Env { host: test.host.clone(), store: h.store.clone() }, PullRequestService::shared().clone());

    let idle_thread = |id: &str| {
        t3_server::launch::ThreadLaunchService::with_providers(h.store.clone(), h.providers.clone())
            .launch(IDLE_LAUNCH(id), clock.utc_now())
            .unwrap();
    };
    let link = |id: &str, number: u64| {
        let receipt = threads
            .dispatch(&json!({"type":"thread.pull-request.link","commandId":format!("link-{id}-{number}"),"threadId":id,"host":"github.com","repository":"acme/web","number":number,"url":format!("https://github.com/acme/web/pull/{number}"),"source":"agent"}), clock.utc_now())
            .unwrap();
        assert_eq!(receipt.status, "accepted", "{receipt:?}");
    };
    let after = |minutes: u64| clock.advance(Duration::from_secs(minutes * 60));
    let snapshot = |id: &str, number: u64| link_of(&h.view(id), number)["snapshot"].clone();

    // -- a link with no snapshot is read, and what the host says is written back ---------------------------
    remotes.lock().unwrap().insert(7, Remote::default());
    idle_thread("t1");
    settled(&h, "t1").await;
    link("t1", 7);
    assert_eq!(snapshot("t1", 7), Value::Null);
    sync.sweep().await;
    let first = snapshot("t1", 7);
    assert_eq!((first["state"].clone(), first["title"].clone(), first["headBranch"].clone(), first["baseBranch"].clone(), first["isDraft"].clone()), (json!("open"), json!("Fix it"), json!("feature"), json!("main"), json!(false)), "{first}");
    assert!(first["syncedAt"].is_string() && first["updatedAt"].is_string(), "{first}");

    // -- an open pull request is read again, and when it says nothing new nothing is written -----------------
    let (reads, sequence) = (github.graphql(7), h.store.latest_sequence().unwrap());
    after(10);
    sync.sweep().await;
    assert!(github.graphql(7) > reads, "an open pull request is due at every pass");
    assert_eq!(h.store.latest_sequence().unwrap(), sequence, "the host said nothing new, so no command was committed");
    assert_eq!(snapshot("t1", 7)["syncedAt"], first["syncedAt"]);

    // -- the host's change is written back, and a merged pull request is not read again --------------------------
    remotes.lock().unwrap().get_mut(&7).unwrap().state = "MERGED";
    remotes.lock().unwrap().get_mut(&7).unwrap().title = "Fix it for good";
    after(10);
    sync.sweep().await;
    let merged = snapshot("t1", 7);
    assert_eq!((merged["state"].clone(), merged["title"].clone(), merged["mergedAt"].clone()), (json!("merged"), json!("Fix it for good"), json!("2026-10-05T13:00:00Z")), "{merged}");
    assert!(h.store.latest_sequence().unwrap() > sequence);
    let reads = github.graphql_total();
    after(10);
    sync.sweep().await;
    assert_eq!(github.graphql_total(), reads, "a merged pull request is not read again");

    // -- two threads on one pull request share the read, and both take the snapshot --------------------------------
    remotes.lock().unwrap().insert(9, Remote::default());
    idle_thread("t2");
    idle_thread("t3");
    settled(&h, "t2").await;
    settled(&h, "t3").await;
    link("t2", 9);
    link("t3", 9);
    let before = github.graphql(9);
    sync.sweep().await;
    assert_eq!(github.graphql(9) - before, github.graphql(9) - before.min(github.graphql(9)), "counted");
    let one_read = github.graphql(9) - before;
    assert!(one_read >= 1);
    assert_eq!((snapshot("t2", 9)["state"].clone(), snapshot("t3", 9)["state"].clone()), (json!("open"), json!("open")));
    after(10);
    let before = github.graphql(9);
    sync.sweep().await;
    assert_eq!(github.graphql(9) - before, one_read, "however many threads share it, a pull request costs one read per pass");

    // -- a settled thread stops watching: its pull request is no longer read for it ------------------------------------
    remotes.lock().unwrap().insert(10, Remote::default());
    idle_thread("t4");
    settled(&h, "t4").await;
    link("t4", 10);
    sync.sweep().await;
    assert_eq!(snapshot("t4", 10)["state"], "open");
    // Settling is a command the port refuses while a provider session is up, so the thread is settled in its projection.
    h.store
        .transaction(|transaction| {
            let mut projection = t3_server::persistence::read_projection(transaction, "thread", "t4")?.unwrap();
            projection["thread"]["settledOverride"] = json!("settled");
            projection["thread"]["settledAt"] = json!("2026-10-05T14:00:00.000Z");
            t3_server::persistence::write_projection(transaction, "thread", "t4", &projection)
        })
        .unwrap();
    remotes.lock().unwrap().get_mut(&10).unwrap().state = "CLOSED";
    let (reads, sequence) = (github.graphql(10), h.store.latest_sequence().unwrap());
    after(20);
    sync.sweep().await;
    assert_eq!((github.graphql(10), h.store.latest_sequence().unwrap()), (reads, sequence));
    assert_eq!(snapshot("t4", 10)["state"], "open", "the settled thread's snapshot waits for the thread to be unsettled");

    // -- an archived thread's links are not read either -------------------------------------------------------------------
    remotes.lock().unwrap().insert(11, Remote::default());
    idle_thread("t5");
    settled(&h, "t5").await;
    link("t5", 11);
    let receipt = threads.dispatch(&json!({"type":"thread.archive","commandId":"archive-t5","threadId":"t5"}), clock.utc_now()).unwrap();
    assert_eq!(receipt.status, "accepted", "{receipt:?}");
    let reads = github.graphql(11);
    sync.sweep().await;
    assert_eq!(github.graphql(11), reads);

    // -- the timer main.rs starts sweeps at once, and a host that fails leaves the link as it was -----------------------------
    remotes.lock().unwrap().insert(12, Remote { failing: true, ..Remote::default() });
    idle_thread("t6");
    settled(&h, "t6").await;
    link("t6", 12);
    sync.sweep().await;
    assert_eq!(snapshot("t6", 12), Value::Null, "a pull request the host fails on is skipped, not failed");
    remotes.lock().unwrap().insert(13, Remote::default());
    idle_thread("t7");
    settled(&h, "t7").await;
    link("t7", 13);
    sync.spawn_timer();
    h.until("t7", |view| link_of(view, 13)["snapshot"]["state"].as_str().map(|_| ())).await;

    h.finish().await;
}
