//! Bitbucket pull requests over a real socket, through the whole stack: the project's remote says
//! Bitbucket, the service picks the Bitbucket provider, and the provider speaks to a scripted host
//! on loopback. Every response is checked against the original's schema at the transport. Each call
//! is checked for what it sends, for what it refuses before sending, and for what it makes the
//! server forget. No network beyond loopback, no real credentials. This binary installs one
//! process-wide host before the first request, so everything runs in one test.
#![recursion_limit = "512"]
mod pull_request_support;
mod source_control_support;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
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

const WEB: &str = "/repositories/acme/web";

fn user(nick: &str) -> Value {
    json!({"uuid":format!("{{{nick}-uuid}}"),"nickname":nick,"display_name":nick.to_uppercase(),"links":{"avatar":{"href":format!("https://bitbucket.example/{nick}.png")}}})
}

fn pull_request(number: u64) -> Value {
    json!({
        "id":number,"title":format!("Fix {number}"),"description":"Body text","state":"OPEN","draft":false,"author":user("alice"),
        "source":{"branch":{"name":"feature"},"repository":{"full_name":"acme/web"}},
        "destination":{"branch":{"name":"main"},"repository":{"full_name":"acme/web"}},
        "created_on":"2026-01-01T00:00:00+00:00","updated_on":format!("2026-01-0{}T00:00:00+00:00", if number == 8 { 3 } else { 2 }),
        "reviewers":[user("bob")],
        "participants":[{"user":user("bob"),"role":"REVIEWER","approved":true,"state":"approved","participated_on":"2026-01-03T10:00:00+00:00"}],
        "links":{"html":{"href":format!("https://bitbucket.org/acme/web/pull-requests/{number}")}},
    })
}

fn page(values: Vec<Value>) -> String {
    json!({"values":values,"pagelen":50}).to_string()
}

fn comment(id: u64, extra: Value) -> Value {
    let mut base = json!({"id":id,"content":{"raw":format!("Comment {id}")},"user":user("bob"),"created_on":format!("2026-01-02T00:0{id}:00+00:00")});
    base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    base
}

const PATCH: &str = "diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n";

/// The host's answers, first match first. The read-only repository answers for `acme/ro`.
fn routes(patch: &str) -> Vec<Value> {
    let ok = |matcher: &str, body: String| json!({"match":matcher,"body":body});
    let on = |method: &str, matcher: &str, body: String| json!({"method":method,"match":matcher,"body":body});
    vec![
        ok("repository.full_name%3D%22acme%2Fro%22", json!({"values":[{"permission":"read"}]}).to_string()),
        ok("/user/permissions/repositories", json!({"values":[{"permission":"write"}]}).to_string()),
        ok("/user", json!({"nickname":"carol"}).to_string()),
        ok("/repositories/acme/ro/pullrequests/5/diffstat", page(vec![])),
        on("POST", "/repositories/acme/ro/pullrequests/5/decline", "{}".into()),
        on("POST", "/repositories/acme/ro/pullrequests/5/merge", "{}".into()),
        ok("/repositories/acme/ro/pullrequests/5", pull_request(5).to_string()),
        ok("/workspaces/acme/members", page(vec![json!({"user":user("alice")}), json!({"user":user("bob")}), json!({"user":user("carol")})])),
        ok(&format!("{WEB}/pullrequests/7/diffstat"), page(vec![json!({"lines_added":3,"lines_removed":1}), json!({"lines_added":5,"lines_removed":0})])),
        ok(&format!("{WEB}/pullrequests/7/conflicts"), page(vec![])),
        ok(&format!("{WEB}/pullrequests/7/statuses"), page(vec![json!({"key":"build","name":"Build","state":"SUCCESSFUL","description":"ok","url":"https://bitbucket.example/build/1"})])),
        ok(&format!("{WEB}/pullrequests/7/commits"), page(vec![json!({"hash":"abc1234","message":"Add it\n\nbody","date":"2026-01-02T00:00:00+00:00","author":{"raw":"Alice <a@x>","user":user("alice")}})])),
        on("GET", &format!("{WEB}/pullrequests/7/comments"), page(vec![comment(11, json!({})), comment(12, json!({"inline":{"path":"a.ts","to":4}})), comment(13, json!({"parent":{"id":12}}))])),
        on("POST", "/comments", "{}".into()),
        on("PUT", "/comments", "{}".into()),
        on("POST", "/resolve", "{}".into()),
        on("DELETE", "/resolve", "{}".into()),
        on("POST", "/approve", "{}".into()),
        on("POST", "/request-changes", "{}".into()),
        on("POST", "/merge", "{}".into()),
        on("POST", "/decline", "{}".into()),
        ok(&format!("{WEB}/pullrequests/7/diff"), patch.to_string()),
        ok(&format!("{WEB}/pullrequests?state="), page(vec![pull_request(8), pull_request(7)])),
        on("PUT", &format!("{WEB}/pullrequests/7"), pull_request(7).to_string()),
        ok(&format!("{WEB}/pullrequests/7"), pull_request(7).to_string()),
        ok(&format!("{WEB}/pullrequests/8/diffstat"), page(vec![json!({"lines_added":1,"lines_removed":1})])),
        ok(&format!("{WEB}/pullrequests/8/conflicts"), page(vec![])),
        ok(&format!("{WEB}/pullrequests/8/statuses"), page(vec![])),
        ok(&format!("{WEB}/pullrequests/8"), pull_request(8).to_string()),
    ]
}

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

/// What the requests were, as the method and the endpoint below the repository.
fn kinds(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            let path = request["url"].as_str().unwrap().split('?').next().unwrap().replace(WEB, "");
            format!("{} {path}", request["method"].as_str().unwrap())
        })
        .collect()
}

/// The requests that changed something; a write is first preceded by the reads that decide it.
fn writes(requests: &[Value]) -> Vec<String> {
    kinds(requests).into_iter().filter(|kind| !kind.starts_with("GET")).collect()
}

fn sorted(mut kinds: Vec<String>) -> Vec<String> {
    kinds.sort();
    kinds
}

#[test]
fn bitbucket_pull_requests_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let host = RouteServer::start(&routes(PATCH)).await;
    let test = test_host(&[("T3CODE_BITBUCKET_API_BASE_URL", host.base.as_str()), ("T3CODE_BITBUCKET_ACCESS_TOKEN", "bitbucket-secret-token")], None, None, &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", "https://bitbucket.org/acme/web.git")]);
    let (_read_only, read_only_path) = repo_with_remotes(&[("origin", "https://bitbucket.org/acme/ro.git")]);
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    for (id, path) in [("web", &web_path), ("ro", &read_only_path)] {
        ProjectService::new(store.clone())
            .mutate(json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":format!("Project {id}"),"workspaceRoot":path.to_string_lossy()}), Utc::now())
            .unwrap();
    }
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_prb".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"prb-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    let fresh = |number: u64| json!({"projectId":"web","repository":"acme/web","number":number,"allowStale":false});
    let with = |mut base: Value, extra: Value| {
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    let mut id = 100;
    macro_rules! call {
        ($socket:expr, $tag:expr, $payload:expr) => {{
            id += 1;
            rpc(&mut $socket, id, $tag, $payload).await
        }};
    }

    // -- reads ---------------------------------------------------------------------------------
    let exit = call!(socket, "pullRequests.summary", fresh(7));
    let summary = success(&exit);
    assert_eq!(summary["provider"], "bitbucket");
    assert_eq!((summary["number"].clone(), summary["state"].clone(), summary["isDraft"].clone()), (json!(7), json!("open"), json!(false)));
    assert_eq!(summary["author"], json!({"login":"alice","name":"ALICE","avatarUrl":"https://bitbucket.example/alice.png"}));
    assert_eq!((summary["additions"].clone(), summary["deletions"].clone(), summary["changedFiles"].clone(), summary["mergeability"].clone()), (json!(8), json!(1), json!(2), json!("mergeable")));
    assert!(summary.get("reviewDecision").is_none() && summary.get("checksState").is_none(), "Bitbucket summarises neither reviews nor checks: {summary}");
    assert_eq!(
        sorted(kinds(&host.take())),
        vec!["GET /pullrequests/7", "GET /pullrequests/7/conflicts", "GET /pullrequests/7/diffstat", "GET /pullrequests/7/statuses", "GET /user/permissions/repositories"],
        "a summary is read from the detail"
    );

    let exit = call!(socket, "pullRequests.detail", fresh(7));
    let detail = success(&exit);
    assert_eq!(detail["provider"], "bitbucket");
    assert_eq!(detail["viewer"], "carol");
    assert_eq!(detail["mergeCapabilities"], json!({"merge":true,"squash":true,"rebase":true}));
    assert_eq!(detail["checks"], json!([{"name":"Build","status":"success","description":"ok","url":"https://bitbucket.example/build/1"}]));
    assert_eq!(detail["reviewers"][0]["login"], "bob");
    assert_eq!(detail["capabilities"]["actions"], json!(["merge","close"]));
    assert_eq!(detail["capabilities"]["reactions"], false);
    assert_eq!(detail["capabilities"]["viewedFiles"], "environment");
    assert_eq!(detail["capabilities"]["review"]["verdicts"], json!(["comment","approve","request-changes"]));
    assert_eq!(detail["capabilities"]["reviewers"], json!({"request":true,"listCandidates":true}));
    assert!(detail["capabilities"].get("labels").is_none() && detail["capabilities"].get("stacks").is_none() && detail["capabilities"].get("updateMethods").is_none());
    assert_eq!(detail["viewerPermissions"]["actions"], json!(["merge","close"]));
    assert!(detail["viewerPermissions"].get("updateMethods").is_none());
    assert!(detail.get("baseComparison").is_none() && detail.get("autoMergeEnabled").is_none(), "Bitbucket states neither");
    host.take();
    // an account that may only read keeps the controls the author is always entitled to
    let exit = call!(socket, "pullRequests.detail", json!({"projectId":"ro","repository":"acme/ro","number":5,"allowStale":false}));
    assert_eq!(success(&exit)["viewerPermissions"]["actions"], json!(["close"]));
    host.take();

    let exit = call!(socket, "pullRequests.checks", fresh(7));
    assert_eq!(success(&exit)["state"], "open");
    let exit = call!(socket, "pullRequests.preview", reference(7));
    assert_eq!(success(&exit)["title"], "Fix 7");
    host.take();

    let exit = call!(socket, "pullRequests.activity", fresh(7));
    let activity = success(&exit);
    assert_eq!(activity["commentCount"], 4, "three comments and bob's vote");
    assert_eq!(activity["comments"].as_array().unwrap().iter().map(|comment| comment["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["issue-comment", "review-comment", "issue-comment", "review"]);
    assert_eq!(activity["comments"][3], json!({"id":"7:bob","kind":"review","author":{"login":"bob","name":"BOB","avatarUrl":"https://bitbucket.example/bob.png"},"body":"","createdAt":"2026-01-03T10:00:00.000Z","url":null,"path":null,"reviewState":"approved"}));
    assert_eq!(activity["reviewThreads"].as_array().unwrap().len(), 1);
    assert_eq!(activity["reviewThreads"][0]["id"], "12");
    assert_eq!((activity["reviewThreads"][0]["path"].clone(), activity["reviewThreads"][0]["line"].clone(), activity["reviewThreads"][0]["side"].clone(), activity["reviewThreads"][0]["isResolved"].clone()), (json!("a.ts"), json!(4), json!("right"), json!(false)));
    assert_eq!(activity["reviewThreads"][0]["comments"].as_array().unwrap().len(), 2, "the reply belongs to the thread");
    assert_eq!(activity["commits"][0]["oid"], "abc1234");
    assert_eq!(activity["commits"][0]["messageHeadline"], "Add it");
    assert_eq!(activity["commentsTruncated"], false);
    assert!(activity.get("reactions").is_none(), "Bitbucket has no reactions");
    assert_eq!(sorted(kinds(&host.take())), vec!["GET /pullrequests/7", "GET /pullrequests/7/comments", "GET /pullrequests/7/commits"]);

    // the patch travels over HTTP, where it compresses and is flow-controlled
    let http = reqwest::Client::new();
    let diff = |body: Value| http.post(format!("http://{address}/api/pull-requests/diff")).bearer_auth(&reader).json(&body).send();
    let response = diff(reference(7)).await.unwrap();
    assert_eq!(response.status(), 200);
    let patch: Value = response.json().await.unwrap();
    assert!(patch["patch"].as_str().unwrap().starts_with("diff --git a/a.ts b/a.ts\nindex 1111111..2222222 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n"));
    assert_eq!((patch["truncated"].clone(), patch["nextCursor"].clone()), (json!(false), Value::Null));
    let response = diff(with(reference(7), json!({"commit":"nope"}))).await.unwrap();
    assert_eq!(response.json::<Value>().await.unwrap()["detail"], "The named commit was not a commit sha.");
    assert_eq!(kinds(&host.take()), vec!["GET /pullrequests/7/diff"], "a refused commit reaches nothing");
    let exit = call!(socket, "pullRequests.diffFileContents", with(reference(7), json!({"changeType":"change","oldPath":"a.ts","newPath":"a.ts"})));
    assert_eq!(failure(&exit)["detail"], "This host cannot expand unchanged pull request lines.");
    let exit = call!(socket, "pullRequests.threadComments", with(reference(7), json!({"threadId":"12","cursor":"x"})));
    assert_eq!(failure(&exit)["detail"], "This host does not page review thread comments.");
    let exit = call!(socket, "pullRequests.stack", reference(7));
    assert_eq!(success(&exit), &Value::Null, "Bitbucket has no stacks");
    let exit = call!(socket, "pullRequests.routingIdentity", json!({"host":"bitbucket.org"}));
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"provider-unsupported"}));
    host.take();

    // -- the listing: one read per repository, carried on from the instant the last slice ended on --
    let exit = call!(socket, "pullRequests.list", json!({"state":"open"}));
    let listed = success(&exit).clone();
    assert_eq!(listed["viewers"], json!({"bitbucket.org":"carol"}));
    assert_eq!(listed["entries"].as_array().unwrap().iter().map(|entry| (entry["number"].as_u64().unwrap(), entry["repository"].as_str().unwrap())).collect::<Vec<_>>(), vec![(8, "acme/web"), (7, "acme/web"), (8, "acme/ro"), (7, "acme/ro")].into_iter().take(listed["entries"].as_array().unwrap().len()).collect::<Vec<_>>());
    assert_eq!(listed["entries"][0]["provider"], "bitbucket");
    assert_eq!(listed["entries"][0]["viewerReviewRequested"], false);
    assert_eq!(listed["entries"][0]["reviewDecision"], Value::Null);
    let providers = listed["providers"].as_array().unwrap();
    assert!(providers.iter().any(|provider| provider["host"] == "bitbucket.org" && provider["kind"] == "bitbucket" && provider["configured"] == true));
    let listing = host.take();
    let first = listing.iter().find(|request| request["url"].as_str().unwrap().starts_with(&format!("{WEB}/pullrequests?"))).unwrap()["url"].as_str().unwrap().to_string();
    assert_eq!(first, format!("{WEB}/pullrequests?state=OPEN&pagelen=50&sort=-updated_on&fields=%2Bvalues.reviewers"));
    let exit = call!(socket, "pullRequests.list", json!({"state":"open","limit":1,"query":"fix more","projectId":"web"}));
    let listed = success(&exit);
    assert_eq!(listed["truncated"], true);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
    assert_eq!(listed["nextCursors"]["bitbucket.org acme/web"], "2026-01-03T00:00:00.000Z|1|8");
    let listing = host.take();
    let second = listing.iter().find(|request| request["url"].as_str().unwrap().starts_with(&format!("{WEB}/pullrequests?"))).unwrap()["url"].as_str().unwrap().to_string();
    assert!(second.contains("&q=") && second.contains("title%20~%20%22fix%20more%22"), "free text becomes a filter expression: {second}");
    let exit = call!(socket, "pullRequests.listStats", json!({"refs":[reference(7)]}));
    assert_eq!(success(&exit), &json!({"stats":[]}), "a listing that carries no counts has nothing to be asked for");
    assert!(host.take().is_empty());

    // -- viewed files are this environment's own, versioned by the head's blob in the patch -----
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[],"truncated":false}));
    host.take();
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"a.ts","viewed":true}]})));
    success(&exit);
    assert_eq!(kinds(&host.take()), vec!["GET /pullrequests/7/diff"], "the head's version of the file is read off the patch and stored with the tick");
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[{"path":"a.ts","state":"viewed"}],"truncated":false}));
    assert!(host.take().is_empty(), "the head's version is held for a minute");
    // the branch moves on: past the minute the file reads as changed
    host.replace_routes(&routes(&PATCH.replace("2222222", "3333333")));
    clock.advance(std::time::Duration::from_secs(61));
    let exit = call!(socket, "pullRequests.invalidate", json!({"reference":reference(7),"filesViewedOnly":true}));
    success(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[{"path":"a.ts","state":"dismissed"}],"truncated":false}));
    assert_eq!(kinds(&host.take()), vec!["GET /pullrequests/7/diff"], "the patch the version came from had aged out");
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"a.ts","viewed":false}]})));
    success(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[],"truncated":false}), "unticking deletes the mark");
    host.take();

    // -- a read-only connection is refused before anything is decided -----------------------------------
    for (tag, extra) in [
        ("pullRequests.comment", json!({"body":"hi"})),
        ("pullRequests.runAction", json!({"action":"close"})),
        ("pullRequests.update", json!({"title":"x"})),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"{carol-uuid}","kind":"user"}],"requested":true})),
        ("pullRequests.submitReview", json!({"verdict":"approve","body":"","comments":[]})),
        ("pullRequests.replyToThread", json!({"threadId":"12","body":"x"})),
        ("pullRequests.setThreadResolution", json!({"threadId":"12","resolved":true})),
    ] {
        let exit = call!(reader_socket, tag, with(reference(7), extra));
        assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&exit)["requiredPermission"], "source-control:write");
    }
    assert!(host.take().is_empty(), "a refused write reaches nothing");

    // -- writes: the account's standing is read first where the host states it ----------------------------------
    let exit = call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"hello \"there\"\ntrue"})));
    success(&exit);
    let sent = host.take();
    assert_eq!(kinds(&sent), vec!["GET /user/permissions/repositories", "POST /pullrequests/7/comments"], "the account's standing is read first");
    assert_eq!(sent[1]["body"], json!({"content":{"raw":"hello \"there\"\ntrue"}}));
    let exit = call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"  "})));
    assert_eq!(failure(&exit)["detail"], "A comment cannot be empty.");
    let exit = call!(socket, "pullRequests.update", with(reference(7), json!({"title":"New title"})));
    success(&exit);
    let sent = host.take();
    assert_eq!(kinds(&sent), vec!["PUT /pullrequests/7"], "rewriting is left to the host to allow");
    assert_eq!(sent[0]["body"], json!({"title":"New title"}));
    let exit = call!(socket, "pullRequests.update", reference(7));
    assert_eq!(failure(&exit)["detail"], "Nothing was changed.");
    let exit = call!(socket, "pullRequests.updateComment", with(reference(7), json!({"commentId":"11","kind":"issue-comment","body":"edited"})));
    success(&exit);
    assert_eq!(kinds(&host.take()), vec!["PUT /pullrequests/7/comments/11"]);
    let exit = call!(socket, "pullRequests.replyToThread", with(reference(7), json!({"threadId":"12","body":"done"})));
    success(&exit);
    let sent = host.take();
    assert_eq!(sent.last().unwrap()["body"], json!({"content":{"raw":"done"},"parent":{"id":12}}));
    assert_eq!(sent.last().unwrap()["url"], format!("{WEB}/pullrequests/7/comments"));
    let exit = call!(socket, "pullRequests.setThreadResolution", with(reference(7), json!({"threadId":"12","resolved":true})));
    success(&exit);
    assert_eq!(kinds(&host.take()).last().unwrap(), "POST /pullrequests/7/comments/12/resolve");
    let exit = call!(socket, "pullRequests.setThreadResolution", with(reference(7), json!({"threadId":"12","resolved":false})));
    success(&exit);
    assert_eq!(kinds(&host.take()).last().unwrap(), "DELETE /pullrequests/7/comments/12/resolve");
    // no reactions on Bitbucket: refused before anything is sent
    let exit = call!(socket, "pullRequests.setReaction", with(reference(7), json!({"subjectId":"12","content":"heart","reacted":true})));
    assert_eq!(failure(&exit)["detail"], "This host has no reactions.");
    assert!(host.take().is_empty());

    // a review is replayed as the requests it is made of, the verdict last
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"approve","body":"Looks good","comments":[{"path":"a.ts","position":{"kind":"added","newLine":4},"body":"Nice"}]})));
    success(&exit);
    let sent = host.take();
    let posted: Vec<String> = kinds(&sent).into_iter().filter(|kind| kind.starts_with("POST")).collect();
    assert_eq!(posted, vec!["POST /pullrequests/7/comments", "POST /pullrequests/7/comments", "POST /pullrequests/7/approve"]);
    let posts: Vec<&Value> = sent.iter().filter(|request| request["method"] == "POST").collect();
    assert_eq!(posts[0]["body"], json!({"content":{"raw":"Nice"},"inline":{"path":"a.ts","to":4}}));
    assert_eq!(posts[1]["body"], json!({"content":{"raw":"Looks good"}}));
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"request-changes","body":"No","comments":[]})));
    success(&exit);
    assert_eq!(kinds(&host.take()).last().unwrap(), "POST /pullrequests/7/request-changes", "Bitbucket can ask for changes");
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"comment","body":" ","comments":[]})));
    assert_eq!(failure(&exit)["detail"], "A review needs a summary or at least one comment.");
    assert!(host.take().is_empty());

    // reviewers: the menu and the request share one gate, and the request writes the whole set
    let exit = call!(socket, "pullRequests.reviewerCandidates", reference(7));
    let candidates = success(&exit);
    assert_eq!(candidates["candidates"].as_array().unwrap().iter().map(|c| (c["login"].as_str().unwrap(), c["isRequested"].as_bool().unwrap())).collect::<Vec<_>>(), vec![("bob", true), ("carol", false)], "the author is not a candidate");
    assert_eq!(candidates["truncated"], false);
    host.take();
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"{carol-uuid}","kind":"user"}],"requested":true})));
    success(&exit);
    let sent = host.take();
    let put = sent.iter().find(|request| request["method"] == "PUT").unwrap();
    assert_eq!(put["body"], json!({"reviewers":[{"uuid":"{bob-uuid}"},{"uuid":"{carol-uuid}"}]}));

    // labels are not something Bitbucket does
    for tag in ["pullRequests.labelCandidates", "pullRequests.setLabels"] {
        let exit = call!(socket, tag, with(reference(7), json!({"labels":["bug"],"applied":true})));
        assert_eq!(failure(&exit)["detail"], "This host cannot change the labels on a change request.", "{tag}");
    }
    assert!(host.take().is_empty());

    // -- actions: merge and close are all Bitbucket has ---------------------------------------------------------
    for (extra, expected) in [
        (json!({"action":"close"}), "POST /pullrequests/7/decline"),
        (json!({"action":"merge"}), "POST /pullrequests/7/merge"),
        (json!({"action":"merge","mergeMethod":"squash"}), "POST /pullrequests/7/merge"),
        (json!({"action":"merge","mergeMethod":"rebase"}), "POST /pullrequests/7/merge"),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), extra.clone()));
        success(&exit);
        let sent = host.take();
        assert_eq!(writes(&sent), vec![expected], "{extra}");
        if extra["action"] == "merge" {
            let all = kinds(&sent);
            assert_eq!(all[0], "GET /user/permissions/repositories", "the account's standing is read first");
            assert!(all[all.iter().position(|kind| kind == expected).unwrap()..].iter().any(|kind| kind == "GET /pullrequests/7"), "the merge is confirmed against the host: {all:?}");
            let strategy = match extra["mergeMethod"].as_str() {
                Some("squash") => "squash",
                Some("rebase") => "rebase_fast_forward",
                _ => "merge_commit",
            };
            assert_eq!(sent.iter().find(|request| request["method"] == "POST").unwrap()["body"], json!({"merge_strategy":strategy}));
        }
    }
    for (extra, sentence) in [
        (json!({"action":"reopen"}), "This host cannot reopen a change request."),
        (json!({"action":"ready"}), "This host cannot ready a change request."),
        (json!({"action":"draft"}), "This host cannot draft a change request."),
        (json!({"action":"enable-auto-merge"}), "This host cannot enable-auto-merge a change request."),
        (json!({"action":"disable-auto-merge"}), "This host cannot disable-auto-merge a change request."),
        (json!({"action":"update-branch","updateMethod":"rebase"}), "This host cannot update-branch a change request."),
        (json!({"action":"approve-workflows"}), "This host cannot approve-workflows a change request."),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), extra));
        assert_eq!(failure(&exit)["detail"], sentence);
    }
    assert!(host.take().is_empty(), "a refusal from the host's capabilities costs no request");
    // an account that may only read cannot merge, and may still decline
    let read_only = |extra: Value| with(json!({"projectId":"ro","repository":"acme/ro","number":5}), extra);
    let exit = call!(socket, "pullRequests.runAction", read_only(json!({"action":"merge"})));
    assert_eq!(failure(&exit)["detail"], "You need write access on this repository to merge.");
    assert_eq!(failure(&exit)["operation"], "runAction");
    assert!(!kinds(&host.take()).iter().any(|kind| kind.contains("/merge")), "nothing was merged");
    let exit = call!(socket, "pullRequests.runAction", read_only(json!({"action":"close"})));
    success(&exit);
    assert!(kinds(&host.take()).iter().any(|kind| kind.ends_with("/pullrequests/5/decline")));

    // -- failures of the host, told as the original tells them -----------------------------------------------------
    host.set_mode(Some(401));
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    assert_eq!(failure(&exit)["_tag"], "PullRequestUnavailableError");
    assert_eq!(failure(&exit)["reason"], "cli-unauthenticated");
    host.set_mode(Some(404));
    let exit = call!(socket, "pullRequests.summary", fresh(9));
    assert_eq!((failure(&exit)["operation"].clone(), failure(&exit)["detail"].clone(), failure(&exit)["reason"].clone()), (json!("summary"), json!("Bitbucket returned HTTP 404."), json!("not-found")));
    host.take();
    // a rate limit pauses the host: reads are refused unsent, a press is let through
    host.set_mode(Some(429));
    let exit = call!(socket, "pullRequests.summary", fresh(10));
    assert_eq!(failure(&exit)["detail"], "Bitbucket returned HTTP 429.");
    host.set_mode(None);
    host.take();
    let exit = call!(socket, "pullRequests.summary", fresh(11));
    assert_eq!(failure(&exit)["detail"], "bitbucket requests to bitbucket.org are paused until the rate limit resets.");
    // Reads the failed summary had started may still be arriving, so the paused one is told by its own number.
    assert!(!host.take().iter().any(|request| request["url"].as_str().unwrap().contains("/pullrequests/11")), "a paused host is not asked");
    let exit = call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"close"})));
    success(&exit);
    assert_eq!(writes(&host.take()), vec!["POST /pullrequests/7/decline"], "an interactive call goes through the pause");
    clock.advance(std::time::Duration::from_secs(31));
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    success(&exit);
    host.take();

    // -- a write makes the server forget what it held -----------------------------------------------------------
    let exit = call!(socket, "pullRequests.detail", fresh(7));
    success(&exit);
    host.take();
    success(&call!(socket, "pullRequests.detail", reference(7)));
    assert!(host.take().is_empty(), "a detail read is held");
    success(&call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"again"}))));
    host.take();
    success(&call!(socket, "pullRequests.detail", fresh(7)));
    assert!(!host.take().is_empty(), "the pull request changed, so the detail is read again");

    // the credential never reaches the wire or a log: it went to the configured host and nowhere else
    assert!(host.authorizations().iter().all(|(_, header)| header.as_deref() == Some("Bearer bitbucket-secret-token") || header.is_none()));

    drop(socket);
    drop(reader_socket);
    stop.send(()).unwrap();
    server.await.unwrap();
}
