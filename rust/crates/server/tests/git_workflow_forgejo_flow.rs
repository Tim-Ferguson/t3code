#![cfg(unix)]
//! What sits above the pull request provider, for a Forgejo checkout, over a real socket: a pull
//! request is created and linked to its thread by `git.runStackedAction`, resolved by
//! `git.resolvePullRequest`, found by the status of the branch it was opened from, and made into a
//! worktree or a local checkout by `git.preparePullRequestThread`. The host is a loopback Forgejo; the
//! repository and the pull request's head ref are local bare repositories; a stand-in executable
//! writes the text. No network, no credentials. This binary installs one
//! process-wide host before the first request, so everything runs in one test.
#![recursion_limit = "512"]
use t3_server::test_support::HANG_BOUND;
mod pull_request_support;
mod source_control_support;

use chrono::Utc;
use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use std::time::Duration;
use t3_server::{project::ProjectService, provider_registry::ProviderRegistry, source_control_forgejo::{host_platform, keys_paths}, source_control_host::Host, text_generation_run::Options};

fn repository(clone_url: &str) -> Value {
    json!({"full_name":"acme/web","clone_url":clone_url,"ssh_url":"git@forgejo.invalid:acme/web.git","default_branch":"main","permissions":{"push":true,"admin":false}})
}

fn row(number: u64, branch: &str, sha: &str) -> Value {
    json!({
        "number":number,"title":format!("Pull request {number}"),"html_url":format!("{{origin}}/acme/web/pulls/{number}"),"state":"open","merged":false,"draft":false,
        "head":{"ref":branch,"sha":sha,"repo":{"full_name":"acme/web","owner":{"login":"acme"}}},"base":{"ref":"main","sha":"def456","repo":{"full_name":"acme/web","owner":{"login":"acme"}}},
        "closed_at":null,"merged_at":null,"updated_at":"2026-01-02T00:00:00Z"
    })
}

/// What the loopback Forgejo answers. The branch's pull requests are none until one is created.
fn routes(bare: &str, sha: &str) -> Vec<Value> {
    vec![
        json!({"method":"GET","match":"/pulls/404","status":404,"body":"gone"}),
        json!({"method":"GET","match":"/pulls/7","body":row(7, "feature", sha).to_string()}),
        json!({"method":"GET","match":"page=2","body":"[]"}),
        json!({"method":"GET","match":"/pulls?","seq":[{"body":"[]"},{"body":json!([row(42, "topic/x", sha)]).to_string()}]}),
        json!({"method":"POST","match":"/pulls","body":row(42, "topic/x", sha).to_string()}),
        json!({"method":"GET","match":"/api/v1/repos/acme/web","body":repository(bare).to_string()}),
    ]
}

#[test]
fn forgejo_workflows_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let bare = path(&base.join("origin.git"));
    let server = RouteServer::start_at(&routes(&bare, "unknown"), "").await;
    let (server_origin, server_host) = (server.base.clone(), server.base.trim_start_matches("http://").to_string());
    let Checkout { origin, checkout, head_sha } = checkout_with_host(&base, &format!("{server_origin}/acme/web.git"));
    assert_eq!(path(&origin), bare);
    // the pull request is at a commit the checkout really has
    server.replace_routes(&routes(&bare, &head_sha));

    // fj's login for the server, and a fake fj that confirms it
    let home = base.join("home");
    let key_file = keys_paths(host_platform(), &path(&home), None, None).remove(0);
    std::fs::create_dir_all(std::path::Path::new(&key_file).parent().unwrap()).unwrap();
    std::fs::write(&key_file, json!({"hosts": {server_host.clone(): {"type": "Application", "token": "test-token"}}}).to_string()).unwrap();
    let cli = FakeCliDir::new();
    install_routed(&cli, "fj", &[json!({"match":"whoami","stdout":""}), json!({"match":"version","stdout":"fj 0.6.0\n"})], ["unauthorized", "HTTP 429", "404"]);
    let test = test_host(&[("HOME", &path(&home))], Some(cli.path()), None, &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let text_dir = base.join("text-calls");
    std::fs::create_dir_all(&text_dir).unwrap();
    let providers = ProviderRegistry::discover(&text_settings(&text_dir), &base).await.unwrap();
    providers.set_text_generation_options(Options { timeout: HANG_BOUND, retry_delay: Duration::ZERO });
    let harness = Harness::start(&base, "wfg", Some(providers)).await;
    ProjectService::new(harness.store.clone())
        .mutate(json!({"type":"project.create","commandId":"create-web","projectId":"web","title":"Project web","workspaceRoot":path(&checkout)}), Utc::now())
        .unwrap();
    let receipt = t3_server::thread::ThreadService::new(harness.store.clone())
        .dispatch(&json!({"type":"thread.create","commandId":"create-thread","threadId":"thread-1","projectId":"web","title":"Fix it","modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), Utc::now())
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    let mut socket = harness.connect(&harness.writer).await;
    let mut read_only = harness.connect(&harness.reader).await;
    let cwd = path(&checkout);
    let mut id = 0;
    macro_rules! next {
        () => {{
            id += 1;
            id
        }};
    }
    let kinds = |requests: &[Value]| -> Vec<String> { requests.iter().map(|request| format!("{} {}", request["method"].as_str().unwrap(), request["url"].as_str().unwrap().replace("/api/v1/repos/acme/web", "").split('?').next().unwrap())).collect() };

    // -- resolving: `#7` means `7`, and the answer is the host's -----------------------------------
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"#7"})).await;
    assert_eq!(
        value_of(&exit),
        &json!({"pullRequest":{"number":7,"title":"Pull request 7","url":format!("{server_origin}/acme/web/pulls/7"),"baseBranch":"main","headBranch":"feature","state":"open"}})
    );
    assert_eq!(kinds(&server.take()), vec!["GET /pulls/7"]);
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"404"})).await;
    assert_eq!(error_of(&exit)["_tag"], "SourceControlProviderError");
    server.take();

    // -- creating and linking: commit, push, the model's words, the host's create, the thread --------
    git(&checkout, &["checkout", "-q", "-b", "topic/x"]);
    std::fs::write(checkout.join("c.txt"), "c\n").unwrap();
    text_plan(&text_dir, 1, json!({"answer":{"title":"Add the c file\nsecond line","body":"- done  "}}));
    let action = json!({"actionId":"a1","cwd":cwd,"action":"commit_push_pr","commitMessage":"Add c","threadId":"thread-1"});
    let (events, exit) = stream(&mut read_only, next!(), "git.runStackedAction", action.clone()).await;
    assert!(events.is_empty());
    assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write", "a connection that may not write creates nothing");
    assert!(server.take().is_empty());
    let (events, exit) = stream(&mut socket, next!(), "git.runStackedAction", action).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let result = &events.last().unwrap()["result"];
    assert_eq!(result["pr"], json!({"status":"created","url":format!("{server_origin}/acme/web/pulls/42"),"number":42,"baseBranch":"main","headBranch":"topic/x","title":"Pull request 42"}));
    assert_eq!(result["push"]["status"], "pushed");
    assert_eq!(git(&origin, &["rev-parse", "refs/heads/topic/x"]), git(&checkout, &["rev-parse", "HEAD"]), "the branch is on the remote");
    assert!(events.iter().any(|event| event["label"] == "Generating PR content..."), "{:?}", events.iter().map(|event| event["label"].clone()).collect::<Vec<_>>());
    let sent = server.take();
    let created: Vec<&Value> = sent.iter().filter(|request| request["method"] == "POST").collect();
    assert_eq!(created.len(), 1, "one pull request is created: {:?}", kinds(&sent));
    assert_eq!(created[0]["url"], "/api/v1/repos/acme/web/pulls");
    let body = &created[0]["body"];
    assert_eq!((body["base"].clone(), body["head"].clone(), body["title"].clone()), (json!("main"), json!("topic/x"), json!("Add the c file")));
    assert!(body["body"].as_str().unwrap().contains("done"), "the description is the model's: {body}");
    assert!(server.authorizations().iter().all(|(_, header)| header.as_deref() == Some("token test-token")), "the credential is the one fj stores, sent to the server it belongs to");
    let linked = unary(&mut socket, next!(), "pullRequests.linkedThreads", json!({"projectId":"web","repository":"acme/web","number":42})).await;
    assert_eq!(value_of(&linked)["threads"][0]["id"], "thread-1", "the pull request is linked to the thread it ran beside: {linked}");

    // -- the status of the branch the pull request was opened from, in a checkout that knows the host by its origin
    let (_status_repo, status_path) = repo_with_remotes(&[("origin", &format!("{server_origin}/acme/web.git"))]);
    git(&status_path, &["checkout", "-q", "-b", "topic/x"]);
    let status = unary(&mut socket, next!(), "vcs.refreshStatus", json!({"cwd":path(&status_path)})).await;
    assert_eq!(value_of(&status)["sourceControlProvider"]["kind"], "forgejo");
    assert_eq!(value_of(&status)["pr"]["number"], 42);
    assert_eq!(value_of(&status)["pr"]["state"], "open");
    assert_eq!(value_of(&status)["pr"]["url"], format!("{server_origin}/acme/web/pulls/42").as_str());
    server.take();
    git(&checkout, &["checkout", "-q", "main"]);

    // -- preparing a worktree thread: the head becomes a branch in a worktree of its own --------------
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    let prepared = value_of(&exit).clone();
    assert_eq!(prepared["branch"], "feature");
    assert_eq!(prepared["isOnPullRequestHead"], true);
    assert_eq!(prepared["pullRequest"]["number"], 7);
    let worktree = prepared["worktreePath"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&worktree).join("feature.txt").exists(), "the worktree holds the pull request's files");
    assert_eq!(git(std::path::Path::new(&worktree), &["rev-parse", "HEAD"]), head_sha);
    assert_eq!(git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]), "main", "the main checkout is untouched");
    let again = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    assert_eq!(value_of(&again)["worktreePath"], worktree.as_str(), "the same request reuses the worktree");
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"404","mode":"worktree"})).await;
    assert_eq!(error_of(&exit)["_tag"], "SourceControlProviderError", "a pull request the host does not know creates nothing");
    assert_eq!(git(&checkout, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 2);
    server.take();

    // -- the same pull request checked out locally, fetched from the server's clone address, as `fj` has no checkout of its own ------------------
    git(&checkout, &["checkout", "-q", "main"]);
    // git will not check a branch out twice, so the worktree's goes first
    git(&checkout, &["worktree", "remove", "--force", &worktree]);
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"local"})).await;
    let prepared = value_of(&exit).clone();
    assert_eq!((prepared["branch"].clone(), prepared["worktreePath"].clone(), prepared["isOnPullRequestHead"].clone()), (json!("pulls/7"), Value::Null, json!(true)));
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head_sha, "the pull request's head is what is checked out");
    assert!(kinds(&server.take()).contains(&"GET /pulls/7".to_string()));

    drop(socket);
    drop(read_only);
    harness.finish().await;
}
