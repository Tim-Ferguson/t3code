//! GitLab merge requests over a real socket, through the whole stack: the project's remote says
//! GitLab, the service picks the GitLab provider, and the provider drives a fake `glab`. Every
//! response is checked against the original's schema at the transport. Each call is checked for
//! what it sends, for what it refuses before sending, and for what it makes the server forget.
//! No network, no real `glab`, no real credentials. This binary installs one process-wide host
//! before the first request, so everything runs in one test.
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
    pull_request_service::PullRequestService,
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn user(id: u64, username: &str) -> Value {
    json!({"id":id,"username":username,"name":username.to_uppercase(),"avatar_url":format!("https://gitlab.example/{username}.png")})
}

/// 7 is an ordinary merge request, 99 one the viewer may not merge, 8 a second row for listings.
fn merge_request(number: u64) -> Value {
    json!({
        "iid":number,"title":format!("Fix {number}"),"web_url":format!("https://gitlab.com/acme/web/-/merge_requests/{number}"),"description":"Body text",
        "author":user(1,"alice"),"source_branch":"feature","target_branch":"main","state":"opened","draft":false,"merge_status":"can_be_merged","has_conflicts":false,
        "created_at":"2026-01-01T00:00:00.000Z","updated_at":format!("2026-01-0{}T00:00:00.000Z", if number == 8 { 3 } else { 2 }),"merged_at":null,"closed_at":null,
        "reviewers":[user(2,"bob")],"labels":["bug"],"changes_count":"3",
        "head_pipeline":{"status":"success","web_url":"https://gitlab.com/acme/web/-/pipelines/9","source":"merge_request_event"},
        "user":{"can_merge":number != 99},"merge_when_pipeline_succeeds":false,"squash_on_merge":false,"diverged_commits_count":2
    })
}

/// A merge request as `glab mr view` and `glab mr list` print it.
fn glab_row(number: u64) -> String {
    json!({"iid":number,"title":format!("Fix {number}"),"web_url":format!("https://gitlab.com/acme/web/-/merge_requests/{number}"),"source_branch":"feature","target_branch":"main","state":"opened","draft":true,"updated_at":"2026-01-02T00:00:00.000Z"}).to_string()
}

fn routes() -> Vec<Value> {
    let mr = |number: u64| merge_request(number).to_string();
    let award = |name: &str, username: &str| json!({"name":name,"user":{"username":username}});
    vec![
        json!({"match":"--version","stdout":"glab 1.50.0\n"}),
        json!({"match":"auth status","stdout":"git.example.com\n  \u{2713} Logged in to git.example.com as carol (token)\n"}),
        json!({"match":"api user","stdout":"{\"username\":\"carol\"}"}),
        json!({"match":"merge_requests/99?include_diverged","stdout":mr(99)}),
        json!({"match":"merge_requests/8?include_diverged","stdout":mr(8)}),
        json!({"match":"merge_requests/55?include_diverged","stdout":json!({"iid":55,"title":"Done","web_url":"https://gitlab.com/acme/web/-/merge_requests/55","source_branch":"f","target_branch":"main","state":"merged","merged_at":"2026-01-04T00:00:00Z","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-04T00:00:00Z"}).to_string()}),
        json!({"match":"merge_requests/7?include_diverged","stdout":mr(7)}),
        json!({"match":"?license=false","stdout":"{\"merge_method\":\"merge\",\"squash_option\":\"default_off\"}"}),
        json!({"match":"web/merge_requests?state","stdout":json!([merge_request(8), merge_request(7)]).to_string()}),
        json!({"match":"/merge_requests/7/notes?","stdout":json!([{"id":11,"body":"Looks fine","author":user(2,"bob"),"created_at":"2026-01-02T00:00:00.000Z","system":false,"type":null},{"id":12,"body":"Rename this","author":user(2,"bob"),"created_at":"2026-01-02T00:01:00.000Z","system":false,"type":"DiffNote","position":{"new_path":"a.ts","old_path":"a.ts"}}]).to_string()}),
        json!({"match":"/merge_requests/7/commits?","stdout":json!([{"id":"abc1234","title":"Add it","committed_date":"2026-01-02T00:00:00.000Z","parent_ids":["0000000"],"author_name":"Alice","stats":{"additions":3,"deletions":1}}]).to_string()}),
        json!({"match":"/merge_requests/7/discussions?","stdout":json!([{"id":"d1","notes":[{"id":12,"body":"Rename this","author":user(2,"bob"),"created_at":"2026-01-02T00:01:00.000Z","system":false,"resolvable":true,"resolved":false,"position":{"position_type":"text","new_path":"a.ts","old_path":"a.ts","new_line":4,"old_line":null}}]}]).to_string()}),
        json!({"match":"api graphql","stdinIncludes":"awardEmoji","stdout":json!({"data":{"currentUser":{"username":"carol"},"project":{"mergeRequest":{"awardEmoji":{"nodes":[award("tada","bob")]},"notes":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"gid://gitlab/DiffNote/12","awardEmoji":{"nodes":[award("thumbsup","carol"),award("thumbsup","bob")]}}]}}}}}).to_string()}),
        json!({"match":"api graphql","stdinIncludes":"blobs","stdout":json!({"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"a.ts","oid":"blob-1"}]}}}}}).to_string()}),
        json!({"match":"/merge_requests/7/diffs?","stdout":json!([{"old_path":"a.ts","new_path":"a.ts","diff":"@@ -1 +1 @@\n-a\n+b\n","new_file":false,"renamed_file":false,"deleted_file":false}]).to_string()}),
        json!({"match":"web/users?","stdout":json!([user(1,"alice"),user(2,"bob"),user(3,"carol")]).to_string()}),
        json!({"match":"award_emoji/","stdout":""}),
        json!({"match":"award_emoji?name=","stdout":"{}"}),
        json!({"match":"award_emoji","stdout":json!([{"id":5,"name":"thumbsup","user":{"username":"carol"}}]).to_string()}),
        json!({"match":"--method","stdout":"{}"}),
        json!({"match":"mr view","stdout":glab_row(7)}),
        json!({"match":"mr list","stdout":format!("[{}]", glab_row(7))}),
        json!({"match":"mr ","stdout":""}),
        json!({"match":"merge_requests/7","stdout":json!({"diff_refs":{"base_sha":"b0","head_sha":"h1","start_sha":"s2"}}).to_string()}),
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

/// What the requests were, as the endpoint and method they went to.
fn kinds(calls: &[Value]) -> Vec<String> {
    calls
        .iter()
        .map(|call| {
            let args: Vec<&str> = call["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
            if args[0] == "mr" {
                return args[..2].join(" ");
            }
            let path = args[1].split('?').next().unwrap().replace("projects/acme%2Fweb/", "").replace("projects/acme%2Fweb", "project");
            let method = args.iter().position(|arg| *arg == "--method").map_or("GET", |at| args[at + 1]);
            format!("{method} {path}")
        })
        .collect()
}

#[test]
fn gitlab_merge_requests_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let cli = FakeCliDir::new();
    let routes = routes();
    install_glab(&cli, &routes);
    let blobs_route = routes.iter().position(|route| route["stdinIncludes"] == "blobs").unwrap();
    let test = test_host(&[], Some(cli.path()), None, &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", "https://gitlab.com/acme/web.git")]);
    let (_self_hosted, self_path) = repo_with_remotes(&[("origin", "https://git.example.com/acme/lab.git")]);
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    for (id, path) in [("web", &web_path), ("lab", &self_path)] {
        ProjectService::new(store.clone())
            .mutate(json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":format!("Project {id}"),"workspaceRoot":path.to_string_lossy()}), Utc::now())
            .unwrap();
    }
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_prg".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1
    };
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"prg-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    assert_eq!(summary["provider"], "gitlab");
    assert_eq!((summary["number"].clone(), summary["state"].clone(), summary["isDraft"].clone()), (json!(7), json!("open"), json!(false)));
    assert_eq!(summary["author"], json!({"login":"alice","name":"ALICE","avatarUrl":"https://gitlab.example/alice.png"}));
    assert_eq!((summary["additions"].clone(), summary["changedFiles"].clone(), summary["mergeability"].clone()), (json!(0), json!(3), json!("mergeable")));
    assert!(summary.get("reviewDecision").is_none() && summary.get("checksState").is_none(), "GitLab summarises neither reviews nor checks: {summary}");
    let mut sent = kinds(&take_calls(&cli));
    sent.sort();
    assert_eq!(sent, vec!["GET merge_requests/7", "GET project"], "a summary is read from the detail, with the project's merge settings");

    let exit = call!(socket, "pullRequests.detail", fresh(7));
    let detail = success(&exit);
    assert_eq!(detail["provider"], "gitlab");
    assert_eq!(detail["viewer"], "carol");
    assert_eq!(detail["mergeCapabilities"], json!({"merge":true,"squash":true,"rebase":false}));
    assert_eq!((detail["baseComparison"].clone(), detail["behindBy"].clone()), (json!("behind"), json!(2)));
    assert_eq!(detail["checks"], json!([{"name":"Pipeline","status":"success","description":"merge_request_event","url":"https://gitlab.com/acme/web/-/pipelines/9"}]));
    assert_eq!(detail["reviewers"][0]["login"], "bob");
    assert_eq!(detail["labels"], json!([{"name":"bug","color":null}]));
    assert_eq!(detail["capabilities"]["viewedFiles"], "environment");
    assert_eq!(detail["capabilities"]["updateMethods"], json!(["rebase"]));
    assert_eq!(detail["capabilities"]["review"]["verdicts"], json!(["comment", "approve"]));
    assert!(detail["capabilities"].get("labels").is_none() && detail["capabilities"].get("stacks").is_none());
    assert_eq!(detail["viewerPermissions"]["actions"], json!(["merge","ready","draft","close","reopen","update-branch","enable-auto-merge","disable-auto-merge"]));
    assert_eq!(detail["viewerPermissions"]["updateMethods"], json!(["rebase"]));
    take_calls(&cli);
    // a viewer GitLab will not let merge keeps the controls the author is always entitled to
    let exit = call!(socket, "pullRequests.detail", fresh(99));
    assert_eq!(success(&exit)["viewerPermissions"]["actions"], json!(["ready","draft","close","reopen"]));
    assert!(success(&exit)["viewerPermissions"].get("updateMethods").is_none());
    take_calls(&cli);

    let exit = call!(socket, "pullRequests.checks", fresh(7));
    assert_eq!(success(&exit)["state"], "open");
    let exit = call!(socket, "pullRequests.preview", reference(7));
    assert_eq!(success(&exit)["title"], "Fix 7");
    take_calls(&cli);

    let exit = call!(socket, "pullRequests.activity", fresh(7));
    let activity = success(&exit);
    assert_eq!(activity["commentCount"], 2);
    assert_eq!(activity["comments"][1]["kind"], "review-comment");
    assert_eq!(activity["comments"][1]["reactions"], json!([{"content":"thumbs-up","count":2,"actors":["bob"],"viewerHasReacted":true}]));
    assert_eq!(activity["comments"][0]["reactions"], json!([]));
    assert_eq!(activity["reactions"], json!([{"content":"hooray","count":1,"actors":["bob"],"viewerHasReacted":false}]));
    assert_eq!(activity["reviewThreads"][0], json!({"id":"d1","path":"a.ts","line":4,"side":"right","isResolved":false,"isOutdated":false,"comments":[{"id":"12","author":{"login":"bob","name":"BOB","avatarUrl":"https://gitlab.example/bob.png"},"body":"Rename this","createdAt":"2026-01-02T00:01:00.000Z","url":null,"reactions":[{"content":"thumbs-up","count":2,"actors":["bob"],"viewerHasReacted":true}]}]}));
    assert_eq!(activity["commits"][0]["oid"], "abc1234");
    assert_eq!(activity["commentsTruncated"], false);
    assert_eq!(take_calls(&cli).len(), 4, "notes, commits, discussions and awards");

    // the patch travels over HTTP, where it compresses and is flow-controlled
    let http = reqwest::Client::new();
    let diff = |body: Value| http.post(format!("http://{address}/api/pull-requests/diff")).bearer_auth(&reader).json(&body).send();
    let response = diff(reference(7)).await.unwrap();
    assert_eq!(response.status(), 200);
    let patch: Value = response.json().await.unwrap();
    assert!(patch["patch"].as_str().unwrap().starts_with("diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n"));
    assert_eq!((patch["truncated"].clone(), patch["nextCursor"].clone()), (json!(false), Value::Null));
    let response = diff(with(reference(7), json!({"cursor":"abc"}))).await.unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(response.json::<Value>().await.unwrap()["detail"], "The diff cursor was not one this merge request handed out.");
    let response = diff(with(reference(7), json!({"commit":"nope"}))).await.unwrap();
    assert_eq!(response.json::<Value>().await.unwrap()["detail"], "The named commit was not a commit sha.");
    let exit = call!(socket, "pullRequests.diffFileContents", with(reference(7), json!({"changeType":"change","oldPath":"a.ts","newPath":"a.ts"})));
    assert_eq!(failure(&exit)["detail"], "This host cannot expand unchanged pull request lines.");
    let exit = call!(socket, "pullRequests.threadComments", with(reference(7), json!({"threadId":"d1","cursor":"x"})));
    assert_eq!(failure(&exit)["detail"], "This host does not page review thread comments.");
    let exit = call!(socket, "pullRequests.stack", reference(7));
    assert_eq!(success(&exit), &Value::Null, "GitLab has no stacks");
    let exit = call!(socket, "pullRequests.routingIdentity", json!({"host":"gitlab.com"}));
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"provider-unsupported"}));
    take_calls(&cli);

    // -- the listing: one read per repository, carried on by the number of rows already sent ----------
    let exit = call!(socket, "pullRequests.list", json!({"state":"open"}));
    let page = success(&exit).clone();
    assert_eq!(page["viewers"], json!({"gitlab.com":"carol","git.example.com":"carol"}));
    assert_eq!(page["entries"].as_array().unwrap().iter().map(|entry| (entry["number"].as_u64().unwrap(), entry["repository"].as_str().unwrap())).collect::<Vec<_>>(), vec![(8, "acme/web"), (7, "acme/web")]);
    assert_eq!(page["entries"][0]["provider"], "gitlab");
    assert_eq!(page["entries"][0]["viewerReviewRequested"], false);
    assert_eq!(page["entries"][1]["reviewDecision"], Value::Null);
    let providers = page["providers"].as_array().unwrap();
    assert!(providers.iter().any(|provider| provider["host"] == "gitlab.com" && provider["configured"] == true && provider["searchesOnHost"] == true));
    assert!(providers.iter().any(|provider| provider["host"] == "git.example.com" && provider["kind"] == "gitlab" && provider["configured"] == true), "a self-hosted install is read once glab is signed in to it");
    assert_eq!(page["errors"].as_array().unwrap().len(), 1, "{page}");
    let listing = take_calls(&cli);
    let query = listing.iter().find(|call| call["args"][1].as_str().unwrap().contains("merge_requests?")).unwrap()["args"][1].as_str().unwrap().to_string();
    assert_eq!(query, "projects/acme%2Fweb/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=100&page=1");
    let exit = call!(socket, "pullRequests.list", json!({"state":"open","involvement":"reviewing","limit":1,"query":"fix & more","projectId":"web"}));
    let page = success(&exit);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["nextCursors"]["gitlab.com acme/web"], "2026-01-03T00:00:00.000Z|1|8");
    let listing = take_calls(&cli);
    let query = listing.iter().find(|call| call["args"][1].as_str().unwrap().contains("merge_requests?")).unwrap()["args"][1].as_str().unwrap().to_string();
    assert_eq!(query, "projects/acme%2Fweb/merge_requests?state=opened&reviewer_username=carol&search=fix%20%26%20more&order_by=updated_at&sort=desc&per_page=2&page=1");
    let exit = call!(socket, "pullRequests.listStats", json!({"refs":[reference(7)]}));
    assert_eq!(success(&exit), &json!({"stats":[]}), "a listing that carries no counts has nothing to be asked for");
    assert!(take_calls(&cli).is_empty());

    // -- viewed files are this environment's own --------------------------------------------------
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[],"truncated":false}));
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"a.ts","viewed":true}]})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["GET merge_requests/7", "POST graphql"], "the head's version of the file is stored with the tick");
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[{"path":"a.ts","state":"viewed"}],"truncated":false}));
    assert!(take_calls(&cli).is_empty(), "the head's version is held for a minute");
    // the branch moves on: past the minute the file reads as changed
    std::fs::write(cli.dir.path().join(format!("route.{blobs_route}.out")), json!({"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"a.ts","oid":"blob-2"}]}}}}}).to_string()).unwrap();
    clock.advance(std::time::Duration::from_secs(61));
    let exit = call!(socket, "pullRequests.invalidate", json!({"reference":reference(7),"filesViewedOnly":true}));
    success(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[{"path":"a.ts","state":"dismissed"}],"truncated":false}));
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"a.ts","viewed":false}]})));
    success(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(success(&exit), &json!({"files":[],"truncated":false}), "unticking deletes the mark");
    take_calls(&cli);

    // -- a read-only connection is refused before anything is decided -----------------------------------
    for (tag, extra) in [
        ("pullRequests.comment", json!({"body":"hi"})),
        ("pullRequests.runAction", json!({"action":"close"})),
        ("pullRequests.update", json!({"title":"x"})),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"3","kind":"user"}],"requested":true})),
    ] {
        let exit = call!(reader_socket, tag, with(reference(7), extra));
        assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&exit)["requiredPermission"], "source-control:write");
    }
    assert!(take_calls(&cli).is_empty(), "a refused write reaches nothing");

    // -- writes: the account's standing is read first, then exactly one request ------------------------------
    let exit = call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"hello \"there\"\ntrue"})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["GET merge_requests/7", "POST merge_requests/7/notes"]);
    assert_eq!(serde_json::from_str::<Value>(sent[1]["stdin"].as_str().unwrap()).unwrap(), json!({"body":"hello \"there\"\ntrue"}));
    assert!(!sent[1]["args"].to_string().contains("hello"), "the body is piped, never put on the command line");
    let exit = call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"  "})));
    assert_eq!(failure(&exit)["detail"], "A comment cannot be empty.");
    let exit = call!(socket, "pullRequests.update", with(reference(7), json!({"title":"New title"})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["PUT merge_requests/7"], "rewriting is left to the host to allow");
    assert_eq!(sent[0]["stdin"], "{\"title\":\"New title\"}");
    let exit = call!(socket, "pullRequests.update", reference(7));
    assert_eq!(failure(&exit)["detail"], "Nothing was changed.");
    let exit = call!(socket, "pullRequests.updateComment", with(reference(7), json!({"commentId":"11","kind":"issue-comment","body":"edited"})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["PUT merge_requests/7/notes/11"]);
    let exit = call!(socket, "pullRequests.replyToThread", with(reference(7), json!({"threadId":"d1","body":"done"})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["GET merge_requests/7", "POST merge_requests/7/discussions/d1/notes"]);
    let exit = call!(socket, "pullRequests.setThreadResolution", with(reference(7), json!({"threadId":"d1","resolved":true})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["GET merge_requests/7", "PUT merge_requests/7/discussions/d1"]);
    assert_eq!(sent[1]["stdin"], "{\"resolved\":true}");
    let exit = call!(socket, "pullRequests.setReaction", with(reference(7), json!({"subjectId":"12","content":"heart","reacted":true})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["POST merge_requests/7/notes/12/award_emoji"]);
    let exit = call!(socket, "pullRequests.setReaction", with(reference(7), json!({"content":"thumbs-up","reacted":false})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["GET user", "GET merge_requests/7/award_emoji", "DELETE merge_requests/7/award_emoji/5"]);

    // a review is replayed as the requests it is made of, the verdict last
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"approve","body":"Looks good","comments":[{"path":"a.ts","position":{"kind":"added","newLine":4},"body":"Nice"}]})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(kinds(&sent), vec!["GET merge_requests/7", "GET merge_requests/7", "POST merge_requests/7/discussions", "POST merge_requests/7/notes", "POST merge_requests/7/approve"]);
    assert_eq!(
        serde_json::from_str::<Value>(sent[2]["stdin"].as_str().unwrap()).unwrap(),
        json!({"body":"Nice","position":{"base_sha":"b0","head_sha":"h1","start_sha":"s2","position_type":"text","old_path":"a.ts","new_path":"a.ts","new_line":4}})
    );
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"request-changes","body":"No","comments":[]})));
    assert_eq!(failure(&exit)["detail"], "This host cannot request changes on a change request.");
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"comment","body":" ","comments":[]})));
    assert_eq!(failure(&exit)["detail"], "A review needs a summary or at least one comment.");
    assert!(take_calls(&cli).is_empty());

    // reviewers: the menu and the request share one gate, and the request writes the whole set
    let exit = call!(socket, "pullRequests.reviewerCandidates", reference(7));
    let candidates = success(&exit);
    assert_eq!(candidates["candidates"].as_array().unwrap().iter().map(|c| (c["login"].as_str().unwrap(), c["isRequested"].as_bool().unwrap())).collect::<Vec<_>>(), vec![("bob", true), ("carol", false)], "the author is not a candidate");
    assert_eq!(candidates["truncated"], false);
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"3","kind":"user"}],"requested":true})));
    success(&exit);
    let sent = take_calls(&cli);
    let put = sent.iter().find(|call| call["args"].to_string().contains("PUT")).unwrap();
    assert_eq!(put["stdin"], "{\"reviewer_ids\":[2,3]}");

    // labels are not something GitLab does here
    for tag in ["pullRequests.labelCandidates", "pullRequests.setLabels"] {
        let exit = call!(socket, tag, with(reference(7), json!({"labels":["bug"],"applied":true})));
        assert_eq!(failure(&exit)["detail"], "This host cannot change the labels on a change request.", "{tag}");
    }
    assert!(take_calls(&cli).is_empty());

    // -- actions ---------------------------------------------------------------------------------------------
    for (action, extra, expected) in [
        ("close", json!({}), vec!["mr close"]),
        ("reopen", json!({}), vec!["mr reopen"]),
        ("ready", json!({}), vec!["mr update"]),
        ("draft", json!({}), vec!["mr update"]),
        ("disable-auto-merge", json!({}), vec!["POST merge_requests/7/cancel_merge_when_pipeline_succeeds"]),
        ("enable-auto-merge", json!({"mergeMethod":"squash"}), vec!["mr merge"]),
        ("update-branch", json!({"updateMethod":"rebase"}), vec!["mr rebase"]),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), with(json!({"action":action}), extra)));
        success(&exit);
        let sent = kinds(&take_calls(&cli));
        let mut wanted = vec!["GET merge_requests/7".to_string()];
        wanted.extend(expected.iter().map(|kind| kind.to_string()));
        assert_eq!(sent, wanted, "{action}: the account's standing is read first");
    }
    let exit = call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"merge","mergeMethod":"rebase"})));
    success(&exit);
    let sent = take_calls(&cli);
    assert_eq!(sent[1]["args"], json!(["mr","merge","7","--repo","acme/web","--auto-merge=false","--yes","--rebase"]));
    let mut confirmed = kinds(&sent)[2..].to_vec();
    confirmed.sort();
    assert_eq!(kinds(&sent)[..2], ["GET merge_requests/7", "mr merge"]);
    assert_eq!(confirmed, vec!["GET merge_requests/7", "GET project"], "the merge is confirmed against the host");

    // refused without a request: what the host does not offer, and what the account may not do
    for (extra, sentence) in [
        (json!({"action":"revert"}), "This host cannot revert a change request."),
        (json!({"action":"approve-workflows"}), "This host cannot approve-workflows a change request."),
        (json!({"action":"update-branch","updateMethod":"merge"}), "This host cannot update a branch by merge."),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), extra));
        assert_eq!(failure(&exit)["detail"], sentence);
    }
    assert!(take_calls(&cli).is_empty(), "a refusal from the host's capabilities costs no request");
    for (action, sentence) in [
        ("merge", "You need write access on this repository to merge."),
        ("enable-auto-merge", "You need write access on this repository to have it merged for you once it is ready."),
        ("update-branch", "You need write access on this repository, or to have opened this change request, to update its branch."),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(99), json!({"action":action,"updateMethod":"rebase"})));
        assert_eq!(failure(&exit)["detail"], sentence, "{action}");
        assert_eq!(failure(&exit)["operation"], "runAction");
        assert_eq!(kinds(&take_calls(&cli)), vec!["GET merge_requests/99"], "{action}: only the standing was read");
    }
    let exit = call!(socket, "pullRequests.runAction", with(reference(99), json!({"action":"close"})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["GET merge_requests/99", "mr close"], "the author may close whatever their role");

    // -- failures of the CLI, told as the original tells them ---------------------------------------------------
    std::fs::write(cli.dir.path().join("mode"), "auth\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-unauthenticated","provider":"gitlab"}));
    std::fs::write(cli.dir.path().join("mode"), "notfound\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(9));
    assert_eq!(failure(&exit)["_tag"], "PullRequestOperationError");
    assert_eq!((failure(&exit)["operation"].clone(), failure(&exit)["detail"].clone()), (json!("summary"), json!("GitLab CLI command failed.")));
    assert!(failure(&exit).get("reason").is_none(), "glab reports a missing merge request as a failed command");
    take_calls(&cli);
    // a rate limit pauses the host: reads are refused unsent, a press is let through
    std::fs::write(cli.dir.path().join("mode"), "rate\n").unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(10));
    assert_eq!(failure(&exit)["detail"], "GitLab API rate limit exceeded.");
    std::fs::write(cli.dir.path().join("mode"), "ok\n").unwrap();
    take_calls(&cli);
    let exit = call!(socket, "pullRequests.summary", fresh(11));
    assert_eq!(failure(&exit)["detail"], "gitlab requests to gitlab.com are paused until the rate limit resets.");
    assert!(take_calls(&cli).is_empty(), "a paused host is not asked");
    let exit = call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"close"})));
    success(&exit);
    assert_eq!(kinds(&take_calls(&cli)), vec!["GET merge_requests/7", "mr close"], "an interactive call goes through the pause");
    clock.advance(std::time::Duration::from_secs(31));
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    success(&exit);
    take_calls(&cli);
    // a glab that is not installed
    std::fs::rename(cli.dir.path().join("glab"), cli.dir.path().join("glab.away")).unwrap();
    let exit = call!(socket, "pullRequests.summary", fresh(55));
    assert_eq!(failure(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-missing","provider":"gitlab"}));
    std::fs::rename(cli.dir.path().join("glab.away"), cli.dir.path().join("glab")).unwrap();

    // -- a write makes the server forget what it held ---------------------------------------------------------------
    let exit = call!(socket, "pullRequests.detail", fresh(7));
    success(&exit);
    take_calls(&cli);
    success(&call!(socket, "pullRequests.detail", reference(7)));
    assert!(take_calls(&cli).is_empty(), "a detail read is held");
    success(&call!(socket, "pullRequests.runAction", with(reference(7), json!({"action":"ready"}))));
    take_calls(&cli);
    success(&call!(socket, "pullRequests.detail", fresh(7)));
    assert!(!take_calls(&cli).is_empty(), "the merge request changed, so the detail is read again");

    // -- above the provider: resolving a reference and the status of a checkout are the host's own ------------
    let cwd = web_path.to_string_lossy().into_owned();
    let exit = call!(socket, "git.resolvePullRequest", json!({"cwd":cwd,"reference":"#7"}));
    assert_eq!(success(&exit), &json!({"pullRequest":{"number":7,"title":"Fix 7","url":"https://gitlab.com/acme/web/-/merge_requests/7","baseBranch":"main","headBranch":"feature","state":"open"}}));
    assert_eq!(kinds(&take_calls(&cli)), vec!["mr view"]);
    git(&web_path, &["checkout", "-q", "-b", "feature"]);
    let exit = call!(socket, "vcs.refreshStatus", json!({"cwd":cwd}));
    assert_eq!(success(&exit)["pr"]["number"], 7);
    assert_eq!(success(&exit)["pr"]["url"], "https://gitlab.com/acme/web/-/merge_requests/7");
    assert_eq!(success(&exit)["pr"]["isDraft"], true);
    assert!(kinds(&take_calls(&cli)).contains(&"mr list".to_string()), "the checkout's merge request is looked up by its branch");

    // -- the self-hosted project is served by the same provider ---------------------------------------------------------
    let exit = call!(socket, "pullRequests.summary", json!({"projectId":"lab","repository":"acme/lab","number":8}));
    assert_eq!(success(&exit)["provider"], "gitlab");

    // the credential never reaches the wire or a log: nothing the fake saw or said carries one
    let _ = PullRequestService::shared();
    drop(socket);
    drop(reader_socket);
    stop.send(()).unwrap();
    server.await.unwrap();
}
