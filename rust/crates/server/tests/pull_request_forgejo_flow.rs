//! Forgejo pull requests over a real socket, through the whole stack: the project's remote names a
//! server no URL gives away, `fj` holds a login for it (a key file in a temporary HOME and a fake
//! `fj`), the service picks the Forgejo provider, and the provider talks to a loopback server
//! standing in for Forgejo. Every response is checked against the original's schema at the
//! transport. No network beyond loopback, no real `fj`, no real credentials. This binary installs
//! one process-wide host before the first request, so everything runs in one test.
#![recursion_limit = "512"]
mod pull_request_support;
mod source_control_support;

use chrono::Utc;
use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{project::ProjectService, source_control_forgejo::{host_platform, keys_paths}, source_control_host::Host};

fn user(login: &str) -> Value {
    json!({"login": login, "full_name": login.to_uppercase(), "avatar_url": format!("https://img.example/{login}.png")})
}

fn repo(push: bool) -> Value {
    json!({"full_name":"acme/web","permissions":{"push":push,"admin":false},"allow_merge_commits":true,"allow_squash_merge":true,"allow_rebase":false,"allow_rebase_update":true})
}

fn pull(number: u64, author: &str) -> Value {
    json!({
        "number":number,"title":format!("Fix {number}"),"body":"Body text","html_url":format!("{{origin}}/acme/web/pulls/{number}"),"user":user(author),
        "state":"open","merged":false,"mergeable":true,
        "head":{"ref":"feature","sha":"headsha","repo":repo(true)},"base":{"ref":"main","sha":"basesha","repo":repo(true)},"merge_base":"basesha",
        "created_at":"2026-01-01T00:00:00Z","updated_at":if number == 8 { "2026-01-03T00:00:00Z" } else { "2026-01-02T00:00:00Z" },"closed_at":null,"merged_at":null,
        "additions":3,"deletions":1,"changed_files":2,"labels":[{"id":1,"name":"bug","color":"ff0000"}],"requested_reviewers":[user("bob")]
    })
}

fn routes(push: bool) -> Vec<Value> {
    let page = |rows: Value| rows.to_string();
    // A list says where it ends, as Forgejo does: a server that does not is read until the rows run out.
    let get = |matching: &str, body: String| json!({"method":"GET","match":matching,"body":body,"headers":{"link":"<x?page=1>; rel=\"last\""}});
    vec![
        get("/api/v1/user", user("carol").to_string()),
        get("/pulls/8", pull(8, "alice").to_string()),
        get("/pulls/9", pull(9, "carol").to_string()),
        get("/pulls/7.diff", "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\n".into()),
        get("/pulls/7/reviews/20/comments", page(json!([{"id":31,"body":"Rename this","user":user("bob"),"created_at":"2026-01-02T00:01:00Z","path":"src/a.ts","position":4,"original_position":4,"commit_id":"c1","original_commit_id":"c0","resolver":null}]))),
        get("/pulls/7/reviews?", page(json!([{"id":20,"body":"Needs work","user":user("bob"),"state":"COMMENT","submitted_at":"2026-01-02T00:02:00Z","comments_count":1}]))),
        get("/pulls/7/commits", page(json!([{"sha":"c1","author":user("alice"),"commit":{"message":"Add it\n\nMore","committer":{"date":"2026-01-02T00:00:00Z"}},"parents":[{"sha":"p0"}],"stats":{"additions":3,"deletions":1}}]))),
        get("/pulls/7", pull(7, "alice").to_string()),
        get("/pulls?state=open&sort=recentupdate&limit=50&page=1", page(json!([pull(8, "alice"), pull(7, "alice")]))),
        get("/pulls?", "[]".into()),
        get("/statuses/headsha", page(json!([{"context":"build","status":"success","description":"ok","target_url":"https://ci.example/1","updated_at":"2026-01-02T00:00:00Z"}]))),
        get("/issues/7/comments", page(json!([{"id":11,"body":"Looks fine","user":user("bob"),"created_at":"2026-01-02T00:00:00Z"}]))),
        get("/issues/7/reactions", page(json!([{"content":"+1","user":user("bob")}]))),
        get("/issues/comments/", "[]".into()),
        get("/assignees", page(json!([user("alice"), user("bob"), user("carol")]))),
        get("/labels", page(json!([{"id":1,"name":"bug","color":"ff0000"},{"id":2,"name":"ui"}]))),
        get("/api/v1/repos/acme/web", repo(push).to_string()),
        json!({"method":"POST","match":"/api/v1/repos/acme/web/","body":"{}"}),
        json!({"method":"PATCH","match":"/api/v1/repos/acme/web/","body":"{}"}),
        json!({"method":"DELETE","match":"/api/v1/repos/acme/web/","body":"{}"}),
    ]
}

/// What the requests were, as the method and the path below the repository.
fn kinds(requests: &[Value]) -> Vec<String> {
    requests.iter().map(|request| format!("{} {}", request["method"].as_str().unwrap(), request["url"].as_str().unwrap().replace("/api/v1/repos/acme/web", "").split('?').next().unwrap())).collect()
}

fn writes(requests: &[Value]) -> Vec<Value> {
    requests.iter().filter(|request| request["method"] != "GET").cloned().collect()
}

#[test]
fn forgejo_pull_requests_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let server = RouteServer::start_at(&routes(true), "").await;
    let (origin, host) = (server.base.clone(), server.base.trim_start_matches("http://").to_string());
    // fj's login for the server, and a fake fj that confirms it
    let home = base.join("home");
    let key_file = keys_paths(host_platform(), &home.to_string_lossy(), None, None).remove(0);
    std::fs::create_dir_all(std::path::Path::new(&key_file).parent().unwrap()).unwrap();
    std::fs::write(&key_file, json!({"hosts": {host.clone(): {"type": "Application", "token": "test-token"}}}).to_string()).unwrap();
    let cli = FakeCliDir::new();
    install_routed(&cli, "fj", &[json!({"match":"whoami","stdout":""}), json!({"match":"version","stdout":"fj 0.6.0\n"})], ["unauthorized", "HTTP 429", "404"]);
    let test = test_host(&[("HOME", &home.to_string_lossy())], Some(cli.path()), None, &base);
    let clock = test.clock.clone();
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let (_web, web_path) = repo_with_remotes(&[("origin", &format!("{origin}/acme/web.git"))]);
    let harness = Harness::start(&base, "pfg", None).await;
    ProjectService::new(harness.store.clone())
        .mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Project web","workspaceRoot":web_path.to_string_lossy()}), Utc::now())
        .unwrap();
    let mut socket = harness.connect(&harness.writer).await;
    let mut reader_socket = harness.connect(&harness.reader).await;
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
            unary(&mut $socket, id, $tag, $payload).await
        }};
    }
    let credential = |server: &RouteServer| server.authorizations().iter().all(|(_, header)| header.as_deref() == Some("token test-token"));

    // -- reads ---------------------------------------------------------------------------------
    let exit = call!(socket, "pullRequests.summary", fresh(7));
    let summary = value_of(&exit);
    assert_eq!(summary["provider"], "forgejo");
    assert_eq!((summary["number"].clone(), summary["state"].clone(), summary["isDraft"].clone(), summary["mergeability"].clone()), (json!(7), json!("open"), json!(false), json!("mergeable")));
    assert_eq!((summary["additions"].clone(), summary["deletions"].clone()), (json!(3), json!(1)));
    assert_eq!(summary["author"], json!({"login":"alice","name":"ALICE","avatarUrl":"https://img.example/alice.png"}));
    assert_eq!(kinds(&server.take()), vec!["GET /pulls/7"], "a summary is the one read");

    let exit = call!(socket, "pullRequests.detail", fresh(7));
    let detail = value_of(&exit);
    assert_eq!(detail["provider"], "forgejo");
    assert_eq!(detail["viewer"], "carol");
    assert_eq!(detail["changedFiles"], 2);
    assert_eq!(detail["baseComparison"], "up-to-date");
    assert_eq!(detail["mergeCapabilities"], json!({"merge":true,"squash":true,"rebase":false}));
    assert_eq!(detail["checks"], json!([{"name":"build","status":"success","description":"ok","url":"https://ci.example/1"}]));
    assert_eq!(detail["reviewers"][0]["login"], "bob");
    assert_eq!(detail["labels"], json!([{"name":"bug","color":"ff0000"}]));
    assert_eq!(detail["capabilities"]["viewedFiles"], "environment");
    assert_eq!(detail["capabilities"]["updateMethods"], json!(["merge", "rebase"]));
    assert_eq!(detail["capabilities"]["labels"], true);
    assert_eq!(detail["capabilities"]["review"], json!({"inlineComment":true,"reply":false,"resolve":false,"verdicts":["comment","approve","request-changes"]}));
    assert_eq!(detail["viewerPermissions"]["actions"], json!(["merge","close","reopen","update-branch"]));
    assert_eq!(detail["viewerPermissions"]["updateMethods"], json!(["merge", "rebase"]));
    assert_eq!(detail["viewerPermissions"]["labels"], true);
    server.take();

    let exit = call!(socket, "pullRequests.checks", fresh(7));
    assert_eq!(value_of(&exit)["checks"][0]["name"], "build");
    server.take();
    let exit = call!(socket, "pullRequests.activity", fresh(7));
    let activity = value_of(&exit);
    assert_eq!(activity["commentCount"], 3, "an issue comment, a review and a line comment");
    assert_eq!(activity["commits"][0]["oid"], "c1");
    assert_eq!(activity["reviewThreads"][0]["path"], "src/a.ts");
    assert_eq!(activity["reactions"], json!([{"content":"thumbs-up","count":1,"actors":["bob"],"viewerHasReacted":false}]));
    server.take();

    // the patch travels over HTTP, where it compresses and is flow-controlled
    let http = reqwest::Client::new();
    let response = http.post(format!("http://{}/api/pull-requests/diff", harness.address)).bearer_auth(&harness.reader).json(&reference(7)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let patch: Value = response.json().await.unwrap();
    assert!(patch["patch"].as_str().unwrap().starts_with("diff --git a/src/a.ts b/src/a.ts\n"));
    assert_eq!((patch["truncated"].clone(), patch["nextCursor"].clone()), (json!(false), Value::Null));
    assert_eq!(kinds(&server.take()), vec!["GET /pulls/7.diff"]);
    let exit = call!(socket, "pullRequests.stack", reference(7));
    assert_eq!(value_of(&exit), &Value::Null);
    let exit = call!(socket, "pullRequests.threadComments", with(reference(7), json!({"threadId":"1","cursor":"x"})));
    assert_eq!(error_of(&exit)["detail"], "This host does not page review thread comments.");

    // -- the listing --------------------------------------------------------------------------------
    let exit = call!(socket, "pullRequests.list", json!({"state":"open"}));
    let page = value_of(&exit).clone();
    assert_eq!(page["entries"].as_array().unwrap().iter().map(|entry| entry["number"].as_u64().unwrap()).collect::<Vec<_>>(), vec![8, 7]);
    assert_eq!(page["entries"][0]["provider"], "forgejo");
    let providers = page["providers"].as_array().unwrap();
    assert!(providers.iter().any(|provider| provider["host"] == host.as_str() && provider["kind"] == "forgejo" && provider["configured"] == true && provider["searchesOnHost"] == false), "{providers:?}");
    server.take();

    // -- viewed files are this environment's own ------------------------------------------------------
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(value_of(&exit), &json!({"files":[],"truncated":false}));
    let exit = call!(socket, "pullRequests.setFilesViewed", with(reference(7), json!({"files":[{"path":"src/a.ts","viewed":true}]})));
    value_of(&exit);
    let exit = call!(socket, "pullRequests.filesViewed", fresh(7));
    assert_eq!(value_of(&exit), &json!({"files":[{"path":"src/a.ts","state":"viewed"}],"truncated":false}));
    server.take();

    // -- a read-only connection is refused before anything is decided ---------------------------------
    for (tag, extra) in [
        ("pullRequests.comment", json!({"body":"hi"})),
        ("pullRequests.runAction", json!({"action":"close"})),
        ("pullRequests.update", json!({"title":"x"})),
        ("pullRequests.setReaction", json!({"content":"heart","reacted":true})),
        ("pullRequests.requestReviewers", json!({"reviewers":[{"id":"bob","kind":"user"}],"requested":true})),
        ("pullRequests.setLabels", json!({"labels":["bug"],"applied":true})),
        ("pullRequests.submitReview", json!({"verdict":"comment","body":"x","comments":[]})),
    ] {
        let exit = call!(reader_socket, tag, with(reference(7), extra));
        assert_eq!(error_of(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write");
    }
    assert!(server.take().is_empty(), "a refused write reaches nothing");

    // -- writes: the account's standing is read first, then the request itself ------------------------------
    let exit = call!(socket, "pullRequests.comment", with(reference(7), json!({"body":"hello \"there\"\ntrue"})));
    value_of(&exit);
    let sent = server.take();
    assert_eq!(writes(&sent)[0]["body"], json!({"body":"hello \"there\"\ntrue"}));
    assert_eq!(kinds(&writes(&sent)), vec!["POST /issues/7/comments"]);
    let exit = call!(socket, "pullRequests.update", with(reference(7), json!({"title":"New title"})));
    value_of(&exit);
    let sent = writes(&server.take());
    assert_eq!((kinds(&sent), sent[0]["body"].clone()), (vec!["PATCH /pulls/7".to_string()], json!({"title":"New title"})));
    let exit = call!(socket, "pullRequests.updateComment", with(reference(7), json!({"commentId":"11","kind":"issue-comment","body":"edited"})));
    value_of(&exit);
    assert_eq!(kinds(&writes(&server.take())), vec!["PATCH /issues/comments/11"]);
    let exit = call!(socket, "pullRequests.requestReviewers", with(reference(7), json!({"reviewers":[{"id":"bob","kind":"user"}],"requested":false})));
    value_of(&exit);
    let sent = writes(&server.take());
    assert_eq!((kinds(&sent), sent[0]["body"].clone()), (vec!["DELETE /pulls/7/requested_reviewers".to_string()], json!({"reviewers":["bob"]})));
    let exit = call!(socket, "pullRequests.reviewerCandidates", reference(7));
    assert_eq!(value_of(&exit)["candidates"].as_array().unwrap().iter().map(|c| (c["login"].as_str().unwrap(), c["isRequested"].as_bool().unwrap())).collect::<Vec<_>>(), vec![("bob", true), ("carol", false)], "the author is not a candidate");
    server.take();
    let exit = call!(socket, "pullRequests.labelCandidates", reference(7));
    assert_eq!(value_of(&exit)["candidates"], json!([{"name":"bug","color":"ff0000","description":null,"isApplied":true},{"name":"ui","color":null,"description":null,"isApplied":false}]));
    let exit = call!(socket, "pullRequests.setLabels", with(reference(7), json!({"labels":["ui"],"applied":true})));
    value_of(&exit);
    let sent = writes(&server.take());
    assert_eq!((kinds(&sent), sent[0]["body"].clone()), (vec!["POST /issues/7/labels".to_string()], json!({"labels":[2]})));
    let exit = call!(socket, "pullRequests.setReaction", with(reference(7), json!({"subjectId":"11","content":"thumbs-up","reacted":true})));
    value_of(&exit);
    let sent = writes(&server.take());
    assert_eq!((kinds(&sent), sent[0]["body"].clone()), (vec!["POST /issues/comments/11/reactions".to_string()], json!({"content":"+1"})));
    // a review with a line comment and a verdict travels as one request
    let exit = call!(socket, "pullRequests.submitReview", with(reference(7), json!({"verdict":"request-changes","body":"No","comments":[{"path":"a.ts","position":{"kind":"added","newLine":4},"body":"Nope"}]})));
    value_of(&exit);
    let sent = writes(&server.take());
    assert_eq!(sent[0]["body"], json!({"event":"REQUEST_CHANGES","body":"No","commit_id":"headsha","comments":[{"path":"a.ts","body":"Nope","old_position":0,"new_position":4}]}));
    // the sentences the page shows for what the host does not offer, with no request made
    for (tag, extra, sentence) in [
        ("pullRequests.replyToThread", json!({"threadId":"1","body":"done"}), "This host cannot reply to a review conversation."),
        ("pullRequests.setThreadResolution", json!({"threadId":"1","resolved":true}), "This host cannot resolve a review conversation."),
        ("pullRequests.runAction", json!({"action":"ready"}), "This host cannot ready a change request."),
        ("pullRequests.runAction", json!({"action":"draft"}), "This host cannot draft a change request."),
        ("pullRequests.runAction", json!({"action":"enable-auto-merge"}), "This host cannot enable-auto-merge a change request."),
    ] {
        let exit = call!(socket, tag, with(reference(7), extra));
        assert_eq!(error_of(&exit)["detail"], sentence, "{tag}");
    }
    assert!(server.take().is_empty(), "what the capabilities hide costs no request");

    // -- actions: merge and update need write access, the rest the author's ------------------------------------
    for (action, extra, expected) in [
        ("merge", json!({"mergeMethod":"squash"}), ("POST /pulls/7/merge", json!({"Do":"squash"}))),
        ("close", json!({}), ("PATCH /pulls/7", json!({"state":"closed"}))),
        ("reopen", json!({}), ("PATCH /pulls/7", json!({"state":"open"}))),
        ("update-branch", json!({"updateMethod":"rebase"}), ("POST /pulls/7/update", Value::Null)),
    ] {
        let exit = call!(socket, "pullRequests.runAction", with(reference(7), with(json!({"action":action}), extra)));
        value_of(&exit);
        let sent = writes(&server.take());
        assert_eq!((kinds(&sent).remove(0), sent[0]["body"].clone()), (expected.0.to_string(), expected.1), "{action}");
        assert!(sent.len() >= 1);
    }
    // a viewer who cannot write may close their own pull request and merge nothing
    server.replace_routes(&routes(false));
    let exit = call!(socket, "pullRequests.runAction", with(reference(8), json!({"action":"merge"})));
    assert_eq!(error_of(&exit)["operation"], "runAction");
    assert!(writes(&server.take()).is_empty(), "a refusal costs no write");
    let exit = call!(socket, "pullRequests.runAction", with(reference(9), json!({"action":"close"})));
    value_of(&exit);
    assert_eq!(kinds(&writes(&server.take())), vec!["PATCH /pulls/9"]);
    server.replace_routes(&routes(true));
    clock.advance(std::time::Duration::from_secs(61));

    // -- failures, told as the original tells them ------------------------------------------------------------
    server.set_mode(Some(401));
    let exit = call!(socket, "pullRequests.summary", fresh(8));
    assert_eq!(error_of(&exit), &json!({"_tag":"PullRequestUnavailableError","reason":"cli-unauthenticated","provider":"forgejo"}));
    server.set_mode(Some(404));
    let exit = call!(socket, "pullRequests.summary", fresh(9));
    assert_eq!(error_of(&exit)["_tag"], "PullRequestOperationError");
    server.set_mode(Some(429));
    let exit = call!(socket, "pullRequests.summary", fresh(10));
    assert_eq!(error_of(&exit)["_tag"], "PullRequestOperationError");
    assert!(error_of(&exit)["detail"].as_str().unwrap().contains("HTTP 429"), "{exit}");
    server.set_mode(None);
    server.take();
    let exit = call!(socket, "pullRequests.summary", fresh(11));
    assert!(error_of(&exit)["detail"].as_str().unwrap().contains("paused"), "a rate-limited host is not asked again: {exit}");
    assert!(server.take().is_empty());
    clock.advance(std::time::Duration::from_secs(61));
    value_of(&call!(socket, "pullRequests.summary", fresh(8)));

    // the credential only ever went to the server it belongs to, in the form fj stores it
    assert!(credential(&server));
    drop(socket);
    drop(reader_socket);
    harness.finish().await;
}
