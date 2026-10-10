#![cfg(unix)]
//! What sits above the pull request provider, for a Bitbucket checkout, over a real socket: a pull
//! request is created and linked to its thread by `git.runStackedAction`, resolved by
//! `git.resolvePullRequest`, found by the status of the branch it was opened from, and made into a
//! worktree or a local checkout by `git.preparePullRequestThread`. The host is a scripted server on
//! loopback; the repository, its fork and the pull request's head ref are local bare repositories;
//! a stand-in executable writes the text. No network, no credentials. This binary installs one
//! process-wide host before the first request, so everything runs in one test.
#![recursion_limit = "512"]
mod pull_request_support;
mod source_control_support;

use chrono::Utc;
use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use std::time::Duration;
use t3_server::{project::ProjectService, provider_registry::ProviderRegistry, source_control_host::Host, text_generation_run::Options};

fn user(nick: &str) -> Value {
    json!({"uuid":format!("{{{nick}-uuid}}"),"nickname":nick,"display_name":nick.to_uppercase()})
}

/// A pull request into `acme/web` from `source_repository`'s `branch`.
fn pull_request(number: u64, branch: &str, source_repository: &str, state: &str) -> Value {
    json!({
        "id":number,"title":format!("Pull request {number}"),"state":state,"author":user("alice"),
        "source":{"branch":{"name":branch},"repository":{"full_name":source_repository}},
        "destination":{"branch":{"name":"main"},"repository":{"full_name":"acme/web"}},
        "created_on":"2026-01-01T00:00:00+00:00","updated_on":"2026-01-02T00:00:00+00:00",
        "links":{"html":{"href":format!("https://bitbucket.org/acme/web/pull-requests/{number}")}},
    })
}

fn page(values: Vec<Value>) -> String {
    json!({"values":values,"pagelen":20}).to_string()
}

/// The host's answers. The branch's pull requests are none until one is created: `created` says
/// the host already lists it, otherwise its second listing does.
fn routes(fork_path: &str, created: bool) -> Vec<Value> {
    let ok = |matcher: &str, body: String| json!({"match":matcher,"body":body});
    let forty_two = pull_request(42, "topic/x", "acme/web", "OPEN");
    let listing = if created {
        ok("/repositories/acme/web/pullrequests?pagelen=", page(vec![forty_two.clone()]))
    } else {
        json!({"match":"/repositories/acme/web/pullrequests?pagelen=","seq":[{"body":page(vec![])},{"body":page(vec![forty_two.clone()])}]})
    };
    vec![
        json!({"method":"POST","match":"/repositories/acme/web/pullrequests","body":forty_two.to_string()}),
        listing,
        ok("/repositories/acme/web/pullrequests/7", pull_request(7, "feature", "acme/web", "OPEN").to_string()),
        ok("/repositories/acme/web/pullrequests/12", pull_request(12, "fixes", "fork/web", "OPEN").to_string()),
        ok("/repositories/acme/web/branching-model", "{}".into()),
        ok("/repositories/fork/web", json!({"full_name":"fork/web","links":{"html":{"href":"https://bitbucket.org/fork/web"},"clone":[{"name":"https","href":fork_path},{"name":"ssh","href":fork_path}]},"mainbranch":{"name":"main"}}).to_string()),
        ok("/repositories/acme/web", json!({"full_name":"acme/web","links":{"html":{"href":"https://bitbucket.org/acme/web"},"clone":[]},"mainbranch":{"name":"main"}}).to_string()),
    ]
}

#[test]
fn bitbucket_workflows_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let Checkout { origin, checkout, head_sha } = checkout_with_host(&base, "https://bitbucket.org/acme/web.git");

    // A fork that holds the branch a cross-repository pull request comes from.
    let fork = base.join("fork.git");
    std::fs::create_dir_all(&fork).unwrap();
    git(&fork, &["init", "--bare", "-q", "--initial-branch=main"]);
    git(&checkout, &["push", "-q", &path(&fork), "origin/feature:refs/heads/fixes"]);

    let host = RouteServer::start(&routes(&path(&fork), false)).await;
    let test = test_host(&[("T3CODE_BITBUCKET_API_BASE_URL", host.base.as_str()), ("T3CODE_BITBUCKET_ACCESS_TOKEN", "bitbucket-secret-token")], None, None, &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    let text_dir = base.join("text-calls");
    std::fs::create_dir_all(&text_dir).unwrap();
    let providers = ProviderRegistry::discover(&text_settings(&text_dir), &base).await.unwrap();
    providers.set_text_generation_options(Options { timeout: Duration::from_secs(60), retry_delay: Duration::ZERO });
    let harness = Harness::start(&base, "wfb", Some(providers)).await;
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

    // -- resolving: `#7` means `7`, and the answer is the host's -----------------------------------
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"#7"})).await;
    assert_eq!(
        value_of(&exit),
        &json!({"pullRequest":{"number":7,"title":"Pull request 7","url":"https://bitbucket.org/acme/web/pull-requests/7","baseBranch":"main","headBranch":"feature","state":"open"}})
    );
    assert_eq!(host.take().iter().map(|request| request["url"].as_str().unwrap().to_string()).collect::<Vec<_>>(), vec!["/repositories/acme/web/pullrequests/7".to_string()]);
    let exit = unary(&mut socket, next!(), "git.resolvePullRequest", json!({"cwd":cwd,"reference":"https://bitbucket.org/acme/web/pull-requests/7"})).await;
    assert_eq!(value_of(&exit)["pullRequest"]["number"], 7, "a pull request's own address names it");
    host.take();

    // -- creating and linking: commit, push, the model's words, the host's create, the thread --------
    git(&checkout, &["checkout", "-q", "-b", "topic/x"]);
    std::fs::write(checkout.join("c.txt"), "c\n").unwrap();
    text_plan(&text_dir, 1, json!({"answer":{"title":"Add the c file\nsecond line","body":"- done  "}}));
    host.replace_routes(&routes(&path(&fork), false));
    let action = json!({"actionId":"a1","cwd":cwd,"action":"commit_push_pr","commitMessage":"Add c","threadId":"thread-1"});
    let (events, exit) = stream(&mut read_only, next!(), "git.runStackedAction", action.clone()).await;
    assert!(events.is_empty());
    assert_eq!(error_of(&exit)["requiredPermission"], "source-control:write", "a connection that may not write creates nothing");
    assert!(host.take().is_empty());
    let (events, exit) = stream(&mut socket, next!(), "git.runStackedAction", action).await;
    assert_eq!(exit["_tag"], "Success", "{exit}");
    let result = &events.last().unwrap()["result"];
    assert_eq!(result["pr"], json!({"status":"created","url":"https://bitbucket.org/acme/web/pull-requests/42","number":42,"baseBranch":"main","headBranch":"topic/x","title":"Pull request 42"}));
    assert_eq!(result["push"]["status"], "pushed");
    assert_eq!(git(&origin, &["rev-parse", "refs/heads/topic/x"]), git(&checkout, &["rev-parse", "HEAD"]), "the branch is on the remote");
    assert!(events.iter().any(|event| event["label"] == "Generating PR content...") && events.iter().any(|event| event["label"] == "Creating pull request..."), "{:?}", events.iter().map(|event| event["label"].clone()).collect::<Vec<_>>());
    let sent = host.take();
    let created: Vec<&Value> = sent.iter().filter(|request| request["method"] == "POST").collect();
    assert_eq!(created.len(), 1, "one pull request is created: {sent:?}");
    assert_eq!(created[0]["url"], "/repositories/acme/web/pullrequests");
    assert_eq!(created[0]["body"]["title"], "Add the c file");
    assert_eq!(created[0]["body"]["description"], "- done");
    assert_eq!(created[0]["body"]["source"], json!({"branch":{"name":"topic/x"}}));
    assert_eq!(created[0]["body"]["destination"], json!({"branch":{"name":"main"}}));
    let linked = unary(&mut socket, next!(), "pullRequests.linkedThreads", json!({"projectId":"web","repository":"acme/web","number":42})).await;
    assert_eq!(value_of(&linked)["threads"][0]["id"], "thread-1", "the pull request is linked to the thread it ran beside: {linked}");

    // -- the status of the branch the pull request was opened from, in a checkout that knows the host by its origin
    host.replace_routes(&routes(&path(&fork), true));
    let (_status_repo, status_path) = repo_with_remotes(&[("origin", "https://bitbucket.org/acme/web.git")]);
    git(&status_path, &["checkout", "-q", "-b", "topic/x"]);
    let status = unary(&mut socket, next!(), "vcs.refreshStatus", json!({"cwd":path(&status_path)})).await;
    assert_eq!(value_of(&status)["sourceControlProvider"]["kind"], "bitbucket");
    assert_eq!(value_of(&status)["pr"]["number"], 42);
    assert_eq!(value_of(&status)["pr"]["state"], "open");
    assert_eq!(value_of(&status)["pr"]["url"], "https://bitbucket.org/acme/web/pull-requests/42");
    host.take();
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
    host.take();

    // -- a pull request from a fork is checked out locally, from the fork the host names -------------
    let exit = unary(&mut socket, next!(), "git.preparePullRequestThread", json!({"cwd":cwd,"reference":"12","mode":"local"})).await;
    let prepared = value_of(&exit).clone();
    assert_eq!(prepared["branch"], "t3code/pr-12/fixes");
    assert_eq!((prepared["worktreePath"].clone(), prepared["isOnPullRequestHead"].clone()), (Value::Null, json!(true)));
    assert_eq!(git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]), "t3code/pr-12/fixes");
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head_sha);
    assert_eq!(git(&checkout, &["remote", "get-url", "fork"]), path(&fork), "the fork's own address was added as a remote");
    let requested: Vec<String> = host.take().iter().map(|request| request["url"].as_str().unwrap().to_string()).collect();
    assert!(requested.contains(&"/repositories/acme/web/pullrequests/12".to_string()) && requested.contains(&"/repositories/fork/web".to_string()), "{requested:?}");

    // the credential went to the configured host only
    assert!(host.authorizations().iter().all(|(_, header)| header.as_deref() == Some("Bearer bitbucket-secret-token") || header.is_none()));
    drop(socket);
    drop(read_only);
    harness.finish().await;
}
