#![cfg(unix)]
//! What sits above the pull request provider, for a GitLab checkout, over a real socket: a pull
//! request is created and linked to its thread by `git.runStackedAction`, resolved by
//! `git.resolvePullRequest`, found by the status of the branch it was opened from, and made into a
//! worktree or a local checkout by `git.preparePullRequestThread`. The host is a fake `glab`; the
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
use t3_server::{project::ProjectService, provider_registry::ProviderRegistry, source_control_host::Host, text_generation_run::Options};

fn row(number: u64, branch: &str, state: &str) -> String {
    json!({"iid":number,"title":format!("Pull request {number}"),"web_url":format!("https://gitlab.com/acme/web/-/merge_requests/{number}"),"source_branch":branch,"target_branch":"main","state":state,"draft":false,"updated_at":"2026-01-02T00:00:00.000Z"}).to_string()
}

/// What the fake `glab` answers. The branch's merge requests are none until one is created; a
/// checkout of a merge request does what `glab` does, fetching its branch and switching to it.
fn routes(origin: &str) -> Vec<Value> {
    vec![
        json!({"match":"--version","stdout":"glab 1.50.0\n"}),
        json!({"match":"mr view 404","stderr":"404 Not Found","code":1}),
        json!({"match":"mr view","stdout":row(7, "feature", "opened")}),
        json!({"match":"mr list","seq":["[]", format!("[{}]", row(42, "topic/x", "opened"))]}),
        json!({"match":"POST projects/:fullpath/merge_requests","stdout":"{}"}),
        json!({"match":"api projects/:fullpath","stdout":"{\"default_branch\":\"main\"}"}),
        json!({"match":"mr checkout","exec":format!("git fetch -q '{origin}' feature && git checkout -q -B mr-7 FETCH_HEAD"),"stdout":""}),
    ]
}

#[test]
fn gitlab_workflows_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let Checkout { origin, checkout, head_sha } = checkout_with_host(&base, "https://gitlab.com/acme/web.git");

    let cli = FakeCliDir::new();
    install_glab(&cli, &routes(&path(&origin)));
    let test = test_host(&[], Some(cli.path()), None, &base);
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
    let kinds = |calls: &[Value]| -> Vec<String> {
        calls
            .iter()
            .map(|call| {
                let args: Vec<&str> = call["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
                if args[0] == "mr" { args[..2].join(" ") } else { format!("api {}", args[1..].join(" ").split(" --raw-field").next().unwrap()) }
            })
            .collect()
    };

    // -- resolving: `#7` means `7`, and the answer is the host's -----------------------------------
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"#7"})).await;
    assert_eq!(
        value_of(&exit),
        &json!({"pullRequest":{"number":7,"title":"Pull request 7","url":"https://gitlab.com/acme/web/-/merge_requests/7","baseBranch":"main","headBranch":"feature","state":"open"}})
    );
    assert_eq!(kinds(&take_calls(&cli)), vec!["mr view"]);
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"404"})).await;
    assert_eq!(error_of(&exit)["_tag"], "SourceControlProviderError");
    take_calls(&cli);

    // -- creating and linking: commit, push, the model's words, the host's create, the thread --------
    git(&checkout, &["checkout", "-q", "-b", "topic/x"]);
    std::fs::write(checkout.join("c.txt"), "c\n").unwrap();
    text_plan(&text_dir, 1, json!({"answer":{"title":"Add the c file\nsecond line","body":"- done  "}}));
    let action = json!({"actionId":"a1","cwd":cwd,"action":"commit_push_pr","commitMessage":"Add c","threadId":"thread-1"});
    let (events, exit) = stream(&mut read_only, next!(), "git.runStackedAction", action.clone()).await;
    assert!(events.is_empty());
    assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write", "a connection that may not write creates nothing");
    assert!(take_calls(&cli).is_empty());
    let (events, exit) = stream(&mut socket, next!(), "git.runStackedAction", action).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let result = &events.last().unwrap()["result"];
    assert_eq!(result["pr"], json!({"status":"created","url":"https://gitlab.com/acme/web/-/merge_requests/42","number":42,"baseBranch":"main","headBranch":"topic/x","title":"Pull request 42"}));
    assert_eq!(result["push"]["status"], "pushed");
    assert_eq!(git(&origin, &["rev-parse", "refs/heads/topic/x"]), git(&checkout, &["rev-parse", "HEAD"]), "the branch is on the remote");
    assert!(events.iter().any(|event| event["label"] == "Generating MR content...") || events.iter().any(|event| event["label"] == "Generating PR content..."), "{:?}", events.iter().map(|event| event["label"].clone()).collect::<Vec<_>>());
    let sent = take_calls(&cli);
    let created: Vec<&Value> = sent.iter().filter(|call| call["args"].to_string().contains("merge_requests")).collect();
    assert_eq!(created.len(), 1, "one merge request is created: {:?}", kinds(&sent));
    let args: Vec<&str> = created[0]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
    for wanted in ["source_branch=topic/x", "target_branch=main", "title=Add the c file"] {
        assert!(args.contains(&wanted), "{wanted} in {args:?}");
    }
    assert!(args.iter().any(|arg| arg.starts_with("description=@")), "the description travels in a file, not on the command line: {args:?}");
    let linked = unary(&mut socket, next!(), "pullRequests.linkedThreads", json!({"projectId":"web","repository":"acme/web","number":42})).await;
    assert_eq!(value_of(&linked)["threads"][0]["id"], "thread-1", "the merge request is linked to the thread it ran beside: {linked}");

    // -- the status of the branch the merge request was opened from, in a checkout that knows the host by its origin
    let (_status_repo, status_path) = repo_with_remotes(&[("origin", "https://gitlab.com/acme/web.git")]);
    git(&status_path, &["checkout", "-q", "-b", "topic/x"]);
    let status = unary(&mut socket, next!(), "vcs.refreshStatus", json!({"cwd":path(&status_path)})).await;
    assert_eq!(value_of(&status)["sourceControlProvider"]["kind"], "gitlab");
    assert_eq!(value_of(&status)["pr"]["number"], 42);
    assert_eq!(value_of(&status)["pr"]["state"], "open");
    assert_eq!(value_of(&status)["pr"]["url"], "https://gitlab.com/acme/web/-/merge_requests/42");
    take_calls(&cli);
    git(&checkout, &["checkout", "-q", "main"]);

    // -- preparing a worktree thread: the head becomes a branch in a worktree of its own --------------
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    let prepared = value_of(&exit).clone();
    assert_eq!(prepared["branch"], "feature");
    assert_eq!(prepared["isOnPullRequestHead"], true);
    assert_eq!(prepared["pullRequest"]["number"], 7);
    let worktree = prepared["worktreePath"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&worktree).join("feature.txt").exists(), "the worktree holds the merge request's files");
    assert_eq!(git(std::path::Path::new(&worktree), &["rev-parse", "HEAD"]), head_sha);
    assert_eq!(git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]), "main", "the main checkout is untouched");
    let again = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"worktree"})).await;
    assert_eq!(value_of(&again)["worktreePath"], worktree.as_str(), "the same request reuses the worktree");
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"404","mode":"worktree"})).await;
    assert_eq!(error_of(&exit)["_tag"], "SourceControlProviderError", "a merge request the host does not know creates nothing");
    assert_eq!(git(&checkout, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 2);
    take_calls(&cli);

    // -- the same merge request checked out locally, which is `glab`'s own checkout ------------------
    git(&checkout, &["checkout", "-q", "main"]);
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"7","mode":"local"})).await;
    let prepared = value_of(&exit).clone();
    assert_eq!((prepared["branch"].clone(), prepared["worktreePath"].clone(), prepared["isOnPullRequestHead"].clone()), (json!("mr-7"), Value::Null, json!(true)));
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head_sha, "the merge request's head is what is checked out");
    assert!(kinds(&take_calls(&cli)).contains(&"mr checkout".to_string()));

    drop(socket);
    drop(read_only);
    harness.finish().await;
}
