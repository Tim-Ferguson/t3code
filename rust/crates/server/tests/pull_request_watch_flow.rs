#![cfg(unix)]
#![recursion_limit = "512"]
//! The pull request watcher end to end: threads on the real store and execution service with the scripted
//! fake Codex, a GitHub that is a local server answering from a scenario the test edits, and a manual
//! clock. A pass is `tick()` after the clock advances, so nothing waits on a sleep: a wake is committed
//! when `tick` returns, and every count of requests is the fake's own. No network, no real `gh`, no real
//! credentials. This binary installs one process-wide host, so everything runs in one test.
mod mcp_support;
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::Response};
use chrono::Utc;
use mcp_support::Harness;
use serde_json::{Value, json};
use source_control_support::*;
use t3_vcs::Clock as _;
use t3_server::pull_request_queries as queries;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_server::{
    project::ProjectService,
    pull_request_service::{Env, PullRequestService},
    pull_request_watcher::PullRequestWatcher,
    source_control_host::Host,
    thread::ThreadService,
};

/// What the fake GitHub says right now, per pull request number.
#[derive(Clone)]
struct Remote {
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
        Remote { head: "aaa1111".into(), state: "OPEN", mergeable: "MERGEABLE", checks: vec![("lint", "IN_PROGRESS", None)], comments: vec![], limited: None, remaining: 4_000 }
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

fn wakes(view: &Value) -> Vec<Value> {
    view["messages"].as_array().unwrap().iter().filter(|message| message["notification"]["source"]["kind"] == "monitor").cloned().collect()
}

fn watch_of(view: &Value, number: u64) -> Value {
    view["thread"]["pullRequests"].as_array().unwrap().iter().find(|link| link["number"] == number).map_or(Value::Null, |link| link["watch"].clone())
}

#[test]
fn watched_pull_requests_wake_their_agents() {
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
    let watcher = PullRequestWatcher::new(h.execution.clone(), Env { host: test.host.clone(), store: h.store.clone() }, PullRequestService::shared().clone(), clock.clone());

    // A thread of the project whose first turn has finished, so a wake starts a run at once.
    let idle_thread = |id: &str| {
        t3_server::launch::ThreadLaunchService::with_providers(h.store.clone(), h.providers.clone())
            .launch(json!({"commandId":format!("launch-{id}"),"threadId":id,"projectId":"web","title":id,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"},"initialMessage":{"messageId":format!("first-{id}"),"text":"finish this","attachments":[]}}), Utc::now())
            .unwrap();
    };
    let settled = |id: &str| {
        let id = id.to_owned();
        let h = &h;
        async move { h.until(&id, |view| (view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed")).then_some(())).await }
    };
    let watch = |id: &str, number: u64, source: &str| {
        let receipt = threads
            .dispatch(&json!({"type":"thread.pull-request.watch","commandId":format!("watch-{id}-{number}"),"threadId":id,"host":"github.com","repository":"acme/web","number":number,"watching":true,"link":{"url":format!("https://github.com/acme/web/pull/{number}"),"source":source}}), clock.utc_now())
            .unwrap();
        assert_eq!(receipt.status, "accepted", "{receipt:?}");
    };
    let after = |minutes: u64| clock.advance(Duration::from_secs(minutes * 60));

    // -- one timer, armed only while something is watched -----------------------------------------------
    let timer = PullRequestWatcher::new(h.execution.clone(), Env { host: test.host.clone(), store: h.store.clone() }, PullRequestService::shared().clone(), clock.clone());
    timer.spawn_timer();
    assert_eq!(timer.next_pass_in(), None, "with nothing watched there is no timer to wake");

    // -- a check failing wakes the agent once, with the original's text -----------------------------
    remotes.lock().unwrap().insert(7, Remote::default());
    idle_thread("t1");
    settled("t1").await;
    watch("t1", 7, "agent");
    timer.wait_armed().await;
    assert_eq!(timer.next_pass_in(), Some(Duration::from_secs(120)), "the watch arms one pass, two minutes on");
    timer.shutdown().await;
    assert!(watcher.tick().await, "the first pass is due at once");
    assert!(!watcher.tick().await, "the next is not due for two minutes");
    // A read is the detail, the conversation and its review threads: three documents.
    assert_eq!((github.graphql(7), wakes(&h.view("t1")).len()), (3, 0), "a first look reads the detail and the activity, and a running check is not news");
    assert_eq!(watch_of(&h.view("t1"), 7)["headSha"], "aaa1111");

    after(2);
    remotes.lock().unwrap().get_mut(&7).unwrap().checks = vec![("lint", "COMPLETED", Some("FAILURE"))];
    assert!(watcher.tick().await);
    let told = wakes(&h.view("t1"));
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["text"], "Update on pull request #7 (https://github.com/acme/web/pull/7), which T3 Code is watching for you:\n- Checks failed on aaa1111:\n  - lint https://ci.example/lint\n\nLook into each item and act on it as your task requires. T3 Code keeps watching and wakes you on the next change, so end your turn when you are done. When you hand the work back to the user, call unwatch_pull_request first so the thread returns to their inbox.");
    assert_eq!(told[0]["notification"], json!({"source":{"kind":"monitor"},"outcome":"failed","summary":"#7: checks failed"}));
    assert_eq!((told[0]["createdBy"].as_str(), told[0]["creationSource"].as_str()), (Some("agent"), Some("server")));
    assert_eq!(watch_of(&h.view("t1"), 7)["failedChecks"], json!(["lint"]));

    // -- a second pass with nothing new reads nothing, and wakes nobody ------------------------------
    let reads = github.graphql(7);
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql(7), reads, "nothing is in flight and the snapshot did not move: no read");
    after(10);
    assert!(watcher.tick().await);
    assert!(github.graphql(7) > reads, "a quiet pull request is read again after ten minutes");
    assert_eq!(wakes(&h.view("t1")).len(), 1, "the same failing check is not told twice");

    // -- someone else's comment wakes the agent; the agent's own account does not --------------------
    remotes.lock().unwrap().get_mut(&7).unwrap().comments = vec![("IC_1", "octo", "2026-10-05T12:30:00Z", "my own reply"), ("IC_2", "bob", "2026-10-05T12:31:00Z", "Please rename this.")];
    after(10);
    assert!(watcher.tick().await);
    let told = wakes(&h.view("t1"));
    assert_eq!(told.len(), 2);
    assert!(told[1]["text"].as_str().unwrap().contains("- 1 new comment:\n  - bob: \"Please rename this.\" https://c/IC_2\n"), "{}", told[1]["text"]);
    assert_eq!(told[1]["notification"]["outcome"], "updated");
    assert_eq!(watch_of(&h.view("t1"), 7)["wakes"], 1, "a comments-only wake is counted toward the limit");

    // -- the head moving resets what was told: the same check failing again is news -------------------
    {
        let mut remotes = remotes.lock().unwrap();
        let remote = remotes.get_mut(&7).unwrap();
        remote.head = "bbb2222".into();
        remote.checks = vec![("lint", "IN_PROGRESS", None)];
    }
    after(10);
    assert!(watcher.tick().await);
    let watch_now = watch_of(&h.view("t1"), 7);
    assert_eq!((watch_now["headSha"].clone(), watch_now["failedChecks"].clone(), watch_now["wakes"].clone()), (json!("bbb2222"), json!([]), json!(0)), "{watch_now}");
    assert_eq!(wakes(&h.view("t1")).len(), 2, "a push is not itself news");
    remotes.lock().unwrap().get_mut(&7).unwrap().checks = vec![("lint", "COMPLETED", Some("FAILURE"))];
    after(2);
    assert!(watcher.tick().await);
    let told = wakes(&h.view("t1"));
    assert_eq!(told.len(), 3);
    assert!(told[2]["text"].as_str().unwrap().contains("- Checks failed on bbb2222:"), "{}", told[2]["text"]);

    // -- the pull request merging ends the watch, quietly ---------------------------------------------
    remotes.lock().unwrap().get_mut(&7).unwrap().state = "MERGED";
    after(10);
    assert!(watcher.tick().await);
    assert_eq!(watch_of(&h.view("t1"), 7), Value::Null, "{}", h.view("t1")["thread"]["pullRequests"]);
    assert_eq!(wakes(&h.view("t1")).len(), 3, "a merge wakes nobody");
    let reads = github.graphql_total();
    after(10);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql_total(), reads, "an ended watch is not read");

    // -- a closed pull request ends the watch with a wake -----------------------------------------------
    remotes.lock().unwrap().insert(8, Remote { state: "CLOSED", ..Remote::default() });
    watch("t1", 8, "agent");
    after(2);
    assert!(watcher.tick().await);
    let told = wakes(&h.view("t1"));
    assert_eq!(told.len(), 4);
    assert_eq!(told[3]["text"], "Pull request #8 (https://github.com/acme/web/pull/8) was closed, so T3 Code stopped watching it. Call watch_pull_request if it reopens.");
    assert_eq!(watch_of(&h.view("t1"), 8), Value::Null);

    // -- two threads watching one pull request share the reads -------------------------------------------
    remotes.lock().unwrap().insert(9, Remote::default());
    idle_thread("t2");
    idle_thread("t3");
    settled("t2").await;
    settled("t3").await;
    watch("t2", 9, "agent");
    watch("t3", 9, "manual");
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql(9), 3, "one read of the pull request serves both threads");

    // -- Stop ends a thread's watch; so does archiving it -----------------------------------------------
    remotes.lock().unwrap().insert(10, Remote::default());
    idle_thread("t4");
    settled("t4").await;
    watch("t4", 10, "agent");
    h.execution.dispatch(&json!({"type":"thread.stop","commandId":"stop-t4","threadId":"t4"}), clock.utc_now()).unwrap();
    assert_eq!(watch_of(&h.view("t4"), 10), Value::Null, "Stop ends the watch");
    let reads = github.graphql(10);
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql(10), reads);
    watch("t3", 10, "manual");
    let receipt = threads.dispatch(&json!({"type":"thread.archive","commandId":"archive-t3","threadId":"t3"}), clock.utc_now()).unwrap();
    assert_eq!(receipt.status, "accepted", "{receipt:?}");
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql(10), reads, "an archived thread's watch is not read");

    // -- a settled thread's watch ends at the next pass --------------------------------------------------
    watch("t2", 10, "manual");
    // Settling is a command the port refuses while a provider session is up, so the thread is settled
    // in its projection, as a settle that landed between two passes would leave it.
    h.store
        .transaction(|transaction| {
            let mut projection = t3_server::persistence::read_projection(transaction, "thread", "t2")?.unwrap();
            projection["thread"]["settledOverride"] = json!("settled");
            projection["thread"]["settledAt"] = json!("2026-10-05T14:00:00.000Z");
            t3_server::persistence::write_projection(transaction, "thread", "t2", &projection)
        })
        .unwrap();
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(watch_of(&h.view("t2"), 10), Value::Null, "settling ends the watch");

    // -- a rate-limit pause defers the poll without a request --------------------------------------------
    remotes.lock().unwrap().insert(11, Remote { limited: Some(600), ..Remote::default() });
    idle_thread("t5");
    settled("t5").await;
    watch("t5", 11, "agent");
    after(2);
    assert!(watcher.tick().await);
    let asked = github.graphql(11);
    assert!(asked >= 1);
    for _ in 0..3 {
        after(2);
        assert!(watcher.tick().await);
    }
    assert_eq!(github.graphql(11), asked, "inside the host's pause nothing is asked");
    assert!(watch_of(&h.view("t5"), 11).is_object(), "a pause does not end a watch");
    remotes.lock().unwrap().get_mut(&11).unwrap().limited = None;
    after(10);
    assert!(watcher.tick().await);
    assert!(github.graphql(11) > asked, "the poll resumes once the pause is over");

    // -- background polling never spends the reserve; an interactive read may ------------------------------
    remotes.lock().unwrap().insert(12, Remote { remaining: 100, ..Remote::default() });
    idle_thread("t6");
    settled("t6").await;
    let reference: t3_contracts::PullRequestRef = serde_json::from_value(json!({"projectId":"web","host":"github.com","repository":"acme/web","number":12,"allowStale":false})).unwrap();
    let service = PullRequestService::shared();
    let env = Env { host: test.host.clone(), store: h.store.clone() };
    // An interactive read reports the nearly spent quota ...
    t3_server::source_control_github_api::with_allow_reserve(service.detail(&env, &reference, None)).await.unwrap();
    let spent = github.graphql_total();
    watch("t6", 12, "agent");
    after(2);
    assert!(watcher.tick().await);
    assert_eq!(github.graphql_total(), spent, "the last tenth of the quota is not spent on a background poll");
    // ... which the same interactive caller may still spend.
    after(2);
    t3_server::source_control_github_api::with_allow_reserve(service.activity(&env, &reference, None)).await.unwrap();
    assert!(github.graphql_total() > spent);

    watcher.shutdown().await;
    h.finish().await;
}
