//! The git and source control request paths must run on a modest thread stack.
//!
//! A deeply nested `async fn` makes a future that embeds each awaited future by value, and a
//! generic wrapper (a cache loading through `impl Future`) can embed its argument several
//! times over. In a debug build such futures reach hundreds of kilobytes and are moved by
//! value while they are built and polled, which overflowed the 2 MiB stack of a test thread or
//! runtime worker. This test runs the status, ref listing, status subscription, repository
//! lookup and discovery paths on one thread whose stack is far smaller than that, so growth in
//! those futures fails here before it crashes a dev server. An overflow aborts the binary.
//!
//! `STACK_BYTES` is a little over twice what the paths needed when it was chosen (the test
//! overflowed at 288 KiB and passed at 320 KiB, found by running it at shrinking sizes), and
//! well under the 2 MiB default. About half of that need is the one-time compile of git's
//! diagnostic regexes, which a debug build runs deep; the rest is the futures' own frames.
//! If this fails after a change, look for a new large future first: box it where it is built
//! (see `TtlCache::get`), do not raise the size.
//!
//! The size checks pin the entry points' futures. In a debug build, before the fix, they were
//! 164 KiB (`vcs_rpc::command`) and 117 KiB (`vcs_rpc::open_status`); they are now about 5 KiB
//! and 1 KiB. The limits leave room to grow but fail on a return to that shape.
mod source_control_support;

use chrono::Utc;
use serde_json::{Value, json};
use source_control_support::*;
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    source_control_host::Host,
    source_control_rpc, vcs_rpc,
    transport::ApiState,
};

const STACK_BYTES: usize = 768 * 1024;

/// The most an entry point's future may weigh in a debug build.
const FUTURE_LIMIT_BYTES: usize = 32 * 1024;

fn assert_future_fits<F>(name: &str, future: &F) {
    let size = std::mem::size_of_val(future);
    assert!(size <= FUTURE_LIMIT_BYTES, "the future of {name} is {size} bytes (limit {FUTURE_LIMIT_BYTES}): box the large future it awaits");
}

#[test]
fn status_and_source_control_lookup_fit_a_modest_stack() {
    std::thread::Builder::new()
        .name("modest-stack".into())
        .stack_size(STACK_BYTES)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(scenario()))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let api = ScriptedServer::start(vec![]).await;
    let cli = FakeCliDir::new();
    cli.install(
        "gh",
        r#"
case "$1 $2" in
  "auth token") printf 'fake-token';;
  "auth status") printf '{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octo","tokenSource":"keyring"}]}}';;
  *) echo "gh version 2.83.0 (fake)";;
esac
"#,
    );
    cli.install(
        "glab",
        r#"if [ "$1" = "auth" ]; then printf 'code.example.com\n  Logged in to code.example.com as corp-user (token)\n'; else echo 'glab 1.40 (fake)'; fi"#,
    );
    let host = test_host(&[], Some(cli.path()), Some(api.github_resolver()), &base);
    assert!(Host::install(host.host.clone()), "this binary installs the process host once");

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [3; 32], "t3_session_stack".into(), "loopback-browser".into()).unwrap();
    auth.issue_session(
        "fixture",
        "bearer-access-token",
        vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite],
        json!({"deviceType":"unknown"}),
        Utc::now(),
        chrono::Duration::hours(1),
    )
    .unwrap();
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"stack-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };

    assert_future_fits("vcs_rpc::command", &vcs_rpc::command(&state, "vcs.refreshStatus", json!({})));
    assert_future_fits("vcs_rpc::open_status", &vcs_rpc::open_status(&state, &json!({})));
    assert_future_fits("source_control_rpc::command", &source_control_rpc::command(&state, "server.discoverSourceControl", json!({})));
    // The pull request, git workflow and review entry points box what they await.
    for tag in ["pullRequests.runAction", "pullRequests.requestReviewers", "pullRequests.setLabels", "pullRequests.reviewerCandidates", "pullRequests.labelCandidates", "pullRequests.detail"] {
        assert_future_fits(tag, &t3_server::pull_request_rpc::command(&state, tag, json!({})));
    }
    assert_future_fits("git_workflow_rpc::command", &t3_server::git_workflow_rpc::command(&state, "git.resolvePullRequest", json!({})));
    assert_future_fits("review_service::command", &t3_server::review_service::command(&state, "review.getDiffPreview", json!({})));
    let core = t3_vcs::GitCore::new(std::sync::Arc::new(t3_vcs::process::TokioProcessRunner));
    assert_future_fits(
        "source_control_registry::hosting_provider_for_status",
        &t3_server::source_control_registry::hosting_provider_for_status(&core, "/", None),
    );

    // A checkout whose host is recognised only by a signed-in CLI takes the deepest status path:
    // git status, the host badge, remote detection and the CLI probe behind it.
    let (_repo, dir) = repo_with_remotes(&[("origin", "https://code.example.com/team/app.git")]);
    std::fs::write(dir.join("changed.txt"), "one\ntwo\n").unwrap();
    let cwd = dir.to_str().unwrap();

    let status = ok(vcs_rpc::command(&state, "vcs.refreshStatus", json!({"cwd":cwd})).await);
    assert_eq!(status["isRepo"], true);
    assert_eq!(status["hasWorkingTreeChanges"], true);
    assert_eq!(status["sourceControlProvider"]["kind"], "gitlab");

    let refs = ok(vcs_rpc::command(&state, "vcs.listRefs", json!({"cwd":cwd})).await);
    assert_eq!(refs["totalCount"], 1);

    let mut subscription = vcs_rpc::open_status(&state, &json!({"cwd":cwd,"includeRemote":false})).await.unwrap();
    let snapshot = subscription.next().await.expect("a snapshot");
    assert_eq!(snapshot["local"]["isRepo"], true);
    drop(subscription);

    let discovered = ok(source_control_rpc::command(&state, "server.discoverSourceControl", json!({})).await);
    assert_eq!(discovered["sourceControlProviders"][0]["kind"], "github");

    api.set_script(vec![Step::json(
        200,
        json!({"full_name":"acme/web","html_url":"https://github.com/acme/web","ssh_url":"git@github.com:acme/web.git","default_branch":"main"}),
    )]);
    let looked_up = ok(source_control_rpc::command(&state, "sourceControl.lookupRepository", json!({"provider":"github","repository":"acme/web"})).await);
    assert_eq!(looked_up["nameWithOwner"], "acme/web");
}

fn ok(result: Result<Value, Value>) -> Value {
    result.unwrap_or_else(|error| panic!("request failed: {error}"))
}
