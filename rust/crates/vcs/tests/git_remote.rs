//! Remote status, the throttled background refresh, fetch, push, pull and checkout refresh
//! against real repositories with local bare repositories as remotes (ports of the
//! corresponding cases of the original GitVcsDriverCore.test.ts). Nothing touches a network.
mod common;

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{
    Calls, Live, Repo, bare_remote, clone_of, core_with, counting_core, env_value, failed_process,
    live_core,
};
use t3_vcs::{
    ManualClock,
    core::{CreateWorktreeOptions, VcsCreateWorktreeInput, WorktreeSubmodules},
    process::ProcessRunError,
    remote::PushStatus,
    wire::PullStatus,
};

const FETCH: [&str; 2] = ["fetch", "--quiet"];

fn remote_url(remote: &Repo) -> String {
    remote.path().to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------------------------
// Remote status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn remote_status_reports_divergence_without_reading_the_working_tree() {
    let (repo, _remote) = Repo::with_origin();
    repo.git(&["checkout", "-b", "feature/remote-status"]);
    repo.commit_file("feature.txt", "feature\n", "feature commit");
    repo.git(&["push", "-u", "origin", "feature/remote-status"]);
    repo.write("untracked.txt", "local-only\n");

    let status = live_core()
        .status_details_remote(repo.path(), true)
        .await
        .unwrap();

    assert!(status.is_repo && status.has_upstream);
    assert_eq!(status.branch.as_deref(), Some("feature/remote-status"));
    assert_eq!(
        status.upstream_ref.as_deref(),
        Some("origin/feature/remote-status")
    );
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
    assert_eq!(status.ahead_of_default_count, 1);
}

#[tokio::test]
async fn remote_status_on_an_unborn_head_does_not_fail() {
    let repo = Repo::init();
    let initial = repo.git(&["symbolic-ref", "--short", "HEAD"]);

    let status = live_core()
        .status_details_remote(repo.path(), false)
        .await
        .unwrap();

    assert!(status.is_repo && !status.has_upstream);
    assert_eq!(status.branch.as_deref(), Some(initial.trim()));
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
}

#[tokio::test]
async fn non_repositories_and_missing_directories_report_no_remote_status() {
    let outside = Repo::bare_dir();
    for cwd in [outside.path().to_path_buf(), outside.join("missing")] {
        let status = live_core()
            .status_details_remote(&cwd, false)
            .await
            .unwrap();
        assert!(!status.is_repo && status.branch.is_none());
    }
}

#[tokio::test]
async fn remote_status_can_skip_the_upstream_fetch() {
    let (repo, remote) = Repo::with_origin();
    let initial = repo.branch();
    let peer = clone_of(&remote);
    peer.commit_file("remote.txt", "remote\n", "remote commit");
    peer.git(&["push", "origin", &initial]);
    let core = live_core();

    let cached = core
        .status_details_remote(repo.path(), false)
        .await
        .unwrap();
    let refreshed = core.status_details_remote(repo.path(), true).await.unwrap();

    assert_eq!(cached.behind_count, 0);
    assert_eq!(refreshed.behind_count, 1);
}

#[tokio::test]
async fn background_fetches_do_not_start_git_auto_maintenance() {
    let (repo, _remote) = Repo::with_origin();
    repo.git(&["repack", "-d"]);
    repo.commit_file("second.txt", "second\n", "second commit");
    repo.git(&["push"]);
    repo.git(&["repack", "-d"]);
    // Two packs make `git gc --auto` due, and without detaching it would run inside the fetch.
    repo.git(&["config", "gc.autoPackLimit", "1"]);
    repo.git(&["config", "gc.autoDetach", "false"]);
    repo.git(&["config", "maintenance.autoDetach", "false"]);
    let packs = || {
        repo.git(&["count-objects", "-v"])
            .lines()
            .find_map(|line| line.strip_prefix("packs: ").map(String::from))
            .unwrap()
    };
    assert_eq!(packs(), "2");

    live_core()
        .status_details_remote(repo.path(), true)
        .await
        .unwrap();

    assert_eq!(packs(), "2");
}

#[tokio::test]
async fn the_origin_head_decides_the_default_branch_for_a_non_origin_upstream() {
    let repo = Repo::with_commit();
    let (origin, upstream) = (bare_remote(), bare_remote());
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "origin", &remote_url(&origin)]);
    repo.git(&["remote", "add", "upstream", &remote_url(&upstream)]);
    repo.git(&["push", "origin", "main"]);
    repo.git(&["push", "upstream", "main"]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    repo.git(&["checkout", "-b", "release"]);
    repo.commit_file("release.txt", "release\n", "release commit");
    repo.git(&["push", "-u", "upstream", "release"]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/upstream/HEAD",
        "refs/remotes/upstream/release",
    ]);

    let status = live_core()
        .status_details_remote(repo.path(), true)
        .await
        .unwrap();

    assert_eq!(status.branch.as_deref(), Some("release"));
    assert_eq!(status.upstream_ref.as_deref(), Some("upstream/release"));
    assert!(!status.is_default_branch);
    assert_eq!(status.default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn a_branch_without_an_upstream_reuses_its_base_count_for_the_default_delta() {
    let (repo, _remote) = Repo::with_origin();
    repo.git(&["checkout", "-b", "feature/no-upstream"]);
    repo.commit_file("feature.txt", "feature\n", "feature commit");

    let status = live_core().status_details(repo.path()).await.unwrap();

    assert!(!status.has_upstream);
    assert_eq!(
        (
            status.ahead_count,
            status.behind_count,
            status.ahead_of_default_count
        ),
        (1, 0, 1)
    );
}

// ---------------------------------------------------------------------------------------------
// The throttled background refresh
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_upstream_is_fetched_at_most_once_per_interval() {
    let (repo, _remote) = Repo::with_origin();
    let (core, clock, calls) = counting_core();

    for _ in 0..3 {
        core.status_details_remote(repo.path(), true).await.unwrap();
        core.status_details(repo.path()).await.unwrap();
    }
    assert_eq!(calls.count(&FETCH), 1);

    clock.advance(Duration::from_secs(14));
    core.status_details(repo.path()).await.unwrap();
    assert_eq!(calls.count(&FETCH), 1);

    clock.advance(Duration::from_secs(1));
    core.status_details(repo.path()).await.unwrap();
    assert_eq!(calls.count(&FETCH), 2);
}

#[tokio::test]
async fn a_failing_upstream_backs_off_exponentially_across_linked_worktrees() {
    let (repo, remote) = Repo::with_origin();
    let initial = repo.branch();
    let worktrees = Repo::bare_dir();
    let linked = worktrees.join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "feature/linked",
        &linked.to_string_lossy(),
    ]);
    repo.git_in(
        "..",
        &[
            "-C",
            &linked.to_string_lossy(),
            "branch",
            "--set-upstream-to",
            &format!("origin/{initial}"),
            "feature/linked",
        ],
    );
    std::fs::remove_dir_all(remote.path()).unwrap();
    let (core, clock, calls) = counting_core();
    let read = |cwd: &Path| {
        let core = core.clone();
        let cwd = cwd.to_path_buf();
        async move { core.status_details_remote(&cwd, true).await.unwrap() }
    };

    // Both worktrees share one repository, hence one fetch and one failure count.
    read(repo.path()).await;
    read(&linked).await;
    assert_eq!(calls.count(&FETCH), 1);

    clock.advance(Duration::from_secs(29));
    read(&linked).await;
    assert_eq!(calls.count(&FETCH), 1);

    // 30 seconds after the first failure the second attempt runs, and its failure doubles
    // the wait.
    clock.advance(Duration::from_secs(1));
    read(repo.path()).await;
    assert_eq!(calls.count(&FETCH), 2);

    clock.advance(Duration::from_secs(59));
    read(&linked).await;
    assert_eq!(calls.count(&FETCH), 2);

    clock.advance(Duration::from_secs(1));
    read(repo.path()).await;
    assert_eq!(calls.count(&FETCH), 3);
}

#[tokio::test]
async fn a_failed_background_fetch_never_fails_status_and_a_success_resets_the_back_off() {
    let (repo, remote) = Repo::with_origin();
    let moved = Repo::bare_dir();
    std::fs::remove_dir_all(moved.path()).unwrap();
    std::fs::rename(remote.path(), moved.path()).unwrap();
    let (core, clock, calls) = counting_core();

    // The remote is gone: status still answers from the refs it has.
    let status = core.status_details(repo.path()).await.unwrap();
    assert!(status.has_upstream);
    assert_eq!(calls.count(&FETCH), 1);

    // It comes back; once the cooldown passes the fetch succeeds and the interval applies.
    std::fs::rename(moved.path(), remote.path()).unwrap();
    clock.advance(Duration::from_secs(30));
    core.status_details(repo.path()).await.unwrap();
    assert_eq!(calls.count(&FETCH), 2);
    clock.advance(Duration::from_secs(15));
    core.status_details(repo.path()).await.unwrap();
    assert_eq!(calls.count(&FETCH), 3);
}

#[tokio::test]
async fn background_fetches_are_non_interactive() {
    let (repo, _remote) = Repo::with_origin();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let core = core_with(Arc::new(ManualClock::default()), move |input| {
        if input.args.iter().any(|arg| arg == "fetch") {
            recorder.lock().unwrap().push(
                [
                    "GCM_INTERACTIVE",
                    "GIT_ASKPASS",
                    "GIT_TERMINAL_PROMPT",
                    "SSH_ASKPASS",
                    "SSH_ASKPASS_REQUIRE",
                ]
                .map(|name| env_value(&input, name)),
            );
        }
        Live.run(input)
    });

    core.status_details(repo.path()).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0],
        [
            Some("never".into()),
            Some("".into()),
            Some("0".into()),
            Some("".into()),
            Some("never".into())
        ]
    );
}

#[tokio::test]
async fn origin_facts_are_cached_until_a_driver_mutation_or_expiry() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    let (core, clock, _) = counting_core();
    let has_origin = || {
        let core = core.clone();
        let cwd = repo.path().to_path_buf();
        async move {
            core.status_details_local(&cwd, Default::default())
                .await
                .unwrap()
                .has_origin_remote
        }
    };
    assert!(!has_origin().await);

    // Through the driver: the cached answer is dropped.
    core.ensure_remote(repo.path(), "origin", &remote_url(&remote))
        .await
        .unwrap();
    assert!(has_origin().await);

    // Behind the driver's back: stale until the five-minute lifetime passes.
    repo.git(&["remote", "remove", "origin"]);
    assert!(has_origin().await);
    clock.advance(Duration::from_secs(5 * 60));
    assert!(!has_origin().await);
}

// ---------------------------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_real_fetch_failure_is_explained_without_quoting_git() {
    let repo = Repo::with_commit();
    let missing = repo.join("private-missing-remote");
    repo.git(&["remote", "add", "origin", &missing.to_string_lossy()]);

    let error = live_core()
        .fetch_remote(repo.path(), "origin", None)
        .await
        .unwrap_err();

    assert!(
        error
            .detail
            .contains("could not access the remote repository")
    );
    assert_eq!(error.exit_code, Some(128));
    assert!(error.stderr_length.unwrap() > 0);
    assert!(!error.detail.contains(&*missing.to_string_lossy()));
}

#[tokio::test]
async fn a_failed_scoped_fetch_is_not_retried() {
    for (failure, detail) in [
        (
            "offline",
            "Git could not reach the remote. Check the server's network connection and remote host, then retry.",
        ),
        (
            "auth",
            "Git could not authenticate with the remote. Check Git credentials or SSH access on the server, then retry.",
        ),
        ("timeout", "Git command timed out."),
    ] {
        let repo = Repo::with_commit();
        let calls = Calls::default();
        let recorder = calls.clone();
        let core = core_with(Arc::new(ManualClock::default()), move |input| {
            if input.args.first().map(String::as_str) != Some("fetch") {
                return Live.run(input);
            }
            recorder.record(&input);
            Box::pin(async move {
                match failure {
                    "timeout" => Err(ProcessRunError::Timeout { timeout_ms: 30_000 }),
                    "auth" => failed_process(128, "fatal: Authentication failed"),
                    _ => failed_process(128, "fatal: Could not resolve host"),
                }
            })
        });

        let error = core
            .fetch_remote(repo.path(), "origin", Some("main"))
            .await
            .unwrap_err();

        assert_eq!(error.detail, detail, "{failure}");
        assert_eq!(calls.count(&["fetch"]), 1, "{failure}");
    }
}

#[tokio::test]
async fn fetch_failures_keep_remote_output_out_of_the_error() {
    let secret = "secret-fetch-token";
    let repo = Repo::with_commit();
    let stderr = format!(
        "fatal: Could not resolve host: example.com\nhttps://user:{secret}@example.com/private?token={secret}"
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let failure = stderr.clone();
    let core = core_with(Arc::new(ManualClock::default()), move |input| {
        if input.args.first().map(String::as_str) != Some("fetch") {
            return Live.run(input);
        }
        recorder.lock().unwrap().push((
            input.args.clone(),
            env_value(&input, "LC_ALL"),
            env_value(&input, "GIT_TERMINAL_PROMPT"),
        ));
        let failure = failure.clone();
        Box::pin(async move {
            Ok(t3_vcs::process::ProcessRunOutput {
                code: Some(128),
                stdout: secret.into(),
                stderr: failure,
                ..Default::default()
            })
        })
    });

    let error = core
        .fetch_remote(repo.path(), "origin", None)
        .await
        .unwrap_err();

    assert!(error.detail.contains("could not reach the remote"));
    assert_eq!(error.exit_code, Some(128));
    assert_eq!(error.stderr_length, Some(stderr.encode_utf16().count()));
    assert_eq!(error.stdout_length, Some(secret.len()));
    let encoded = serde_json::to_string(&error).unwrap();
    assert!(!error.to_string().contains(secret) && !encoded.contains(secret));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0],
        (
            vec!["fetch".into(), "--quiet".into(), "origin".into()],
            Some("C".into()),
            Some("0".into())
        )
    );
}

#[tokio::test]
async fn a_scoped_fetch_brings_in_only_that_branch_and_falls_back_to_everything_when_it_is_missing()
{
    let (repo, remote) = Repo::with_origin();
    let initial = repo.branch();
    repo.git(&["remote", "set-head", "origin", &initial]);
    let before = repo.git(&["rev-parse", &format!("refs/remotes/origin/{initial}")]);
    let peer = clone_of(&remote);
    peer.commit_file("remote-change.txt", "remote\n", "remote change");
    peer.git(&["push", "origin", &initial]);
    let remote_head = peer.git(&["rev-parse", "HEAD"]);
    peer.git(&["push", "origin", "HEAD:refs/heads/unrelated"]);
    let core = live_core();

    core.fetch_remote(repo.path(), "origin", Some(&format!("origin/{initial}")))
        .await
        .unwrap();

    assert!(
        !core
            .remote_branch_exists(repo.path(), "origin", "unrelated")
            .await
            .unwrap()
    );
    assert!(
        core.remote_branch_exists(repo.path(), "origin", &initial)
            .await
            .unwrap()
    );
    assert!(
        !core
            .remote_branch_exists(repo.path(), "origin", "local-only")
            .await
            .unwrap()
    );
    let (sha, remote_ref) = core
        .resolve_remote_tracking_commit(repo.path(), &initial, "origin")
        .await
        .unwrap();
    assert_eq!(
        (sha.as_str(), remote_ref.as_str()),
        (remote_head.trim(), format!("origin/{initial}").as_str())
    );
    assert_eq!(
        core.resolve_remote_tracking_commit(repo.path(), &format!("origin/{initial}"), "origin")
            .await
            .unwrap(),
        (sha.clone(), remote_ref.clone())
    );
    assert_eq!(
        repo.git(&["rev-parse", &initial]),
        before,
        "the local branch is untouched"
    );

    // A worktree can start from the fetched commit and remember the branch it came from.
    let worktrees = Repo::bare_dir();
    let path = worktrees.join("fetched-origin");
    core.create_worktree(
        &VcsCreateWorktreeInput {
            cwd: repo.path().to_path_buf(),
            ref_name: sha.clone(),
            new_ref_name: Some("t3code/fetched-origin".into()),
            base_ref_name: Some(remote_ref),
            path: Some(path.clone()),
        },
        &CreateWorktreeOptions {
            worktrees_directory: worktrees.path().to_path_buf(),
            submodules: WorktreeSubmodules::None,
            progress: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(common::run_git(&path, &["rev-parse", "HEAD"]).trim(), sha);
    assert_eq!(
        core.read_config_value(&path, "branch.t3code/fetched-origin.gh-merge-base")
            .await
            .unwrap()
            .as_deref(),
        Some(initial.as_str())
    );
    let status = core.status_details(&path).await.unwrap();
    assert_eq!((status.ahead_count, status.ahead_of_default_count), (0, 0));

    // A branch the remote does not have is not an error: everything is fetched instead.
    core.fetch_remote(repo.path(), "origin", Some("local-only"))
        .await
        .unwrap();
    assert!(
        core.remote_branch_exists(repo.path(), "origin", "unrelated")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn fetching_a_remote_branch_materializes_a_local_branch_and_can_replace_it() {
    let (repo, remote) = Repo::with_origin();
    let peer = clone_of(&remote);
    peer.git(&["checkout", "-b", "topic"]);
    peer.commit_file("topic.txt", "one\n", "topic one");
    peer.git(&["push", "origin", "topic"]);
    let core = live_core();

    core.fetch_remote_branch(repo.path(), "origin", "topic", "local-topic")
        .await
        .unwrap();
    assert_eq!(repo.git(&["show", "local-topic:topic.txt"]), "one\n");

    peer.commit_file("topic.txt", "two\n", "topic two");
    peer.git(&["push", "origin", "topic"]);
    core.fetch_remote_branch(repo.path(), "origin", "topic", "local-topic")
        .await
        .unwrap();
    assert_eq!(repo.git(&["show", "local-topic:topic.txt"]), "two\n");

    peer.commit_file("topic.txt", "three\n", "topic three");
    peer.git(&["push", "origin", "topic"]);
    core.fetch_remote_tracking_branch(repo.path(), "origin", "topic")
        .await
        .unwrap();
    assert_eq!(repo.git(&["show", "origin/topic:topic.txt"]), "three\n");
    assert_eq!(repo.git(&["show", "local-topic:topic.txt"]), "two\n");

    core.set_branch_upstream(repo.path(), "local-topic", "origin", "topic")
        .await
        .unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "--abbrev-ref", "local-topic@{upstream}"])
            .trim(),
        "origin/topic"
    );
}

#[tokio::test]
async fn pull_request_heads_are_fetched_into_a_branch_or_only_into_fetch_head() {
    let (repo, remote) = Repo::with_origin();
    let peer = clone_of(&remote);
    peer.git(&["checkout", "-b", "from-fork"]);
    peer.commit_file("pr.txt", "pr\n", "pr work");
    peer.git(&["push", "origin", "HEAD:refs/pull/7/head"]);
    let pr_head = peer.git(&["rev-parse", "HEAD"]);
    let core = live_core();

    let fetched = core
        .fetch_pull_request_head_commit(repo.path(), 7)
        .await
        .unwrap();
    assert_eq!(fetched, pr_head.trim());
    assert!(repo.git(&["branch", "--list", "pr-7"]).is_empty());

    core.fetch_pull_request_branch(repo.path(), 7, "pr-7")
        .await
        .unwrap();
    assert_eq!(repo.git(&["rev-parse", "pr-7"]), pr_head);
}

#[tokio::test]
async fn ensure_remote_reuses_a_remote_across_ssh_and_https_spellings() {
    let repo = Repo::with_commit();
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/pingdotgg/t3code.git",
    ]);
    let core = live_core();

    for url in [
        "git@github.com:pingdotgg/t3code.git",
        "ssh://git@github.com/pingdotgg/t3code",
        "ssh://github.com/pingdotgg/t3code",
        "ssh://git@github.com:22/pingdotgg/t3code",
        "ssh://git@github.com:22/pingdotgg/t3code.git",
    ] {
        assert_eq!(
            core.ensure_remote(repo.path(), "pingdotgg", url)
                .await
                .unwrap(),
            "origin",
            "{url}"
        );
    }

    let fork = core
        .ensure_remote(repo.path(), "octocat", "git@github.com:octocat/t3code.git")
        .await
        .unwrap();
    assert_eq!(fork, "octocat");
    assert_eq!(repo.git(&["remote"]), "octocat\norigin\n");
    // A taken preferred name gets a numeric suffix.
    let other = core
        .ensure_remote(repo.path(), "octocat", "git@github.com:other/t3code.git")
        .await
        .unwrap();
    assert_eq!(other, "octocat-1");
}

// ---------------------------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn push_publishes_with_an_upstream_and_then_skips_when_up_to_date() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["remote", "add", "origin", &remote_url(&remote)]);
    let core = live_core();
    core.create_ref(repo.path(), "feature/push", false)
        .await
        .unwrap();
    core.switch_ref(repo.path(), "feature/push").await.unwrap();
    repo.commit_file("feature.txt", "feature\n", "Add feature");

    let pushed = core
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();
    assert_eq!(
        (
            pushed.status,
            pushed.branch.as_str(),
            pushed.upstream_branch.as_deref(),
            pushed.set_upstream
        ),
        (
            PushStatus::Pushed,
            "feature/push",
            Some("origin/feature/push"),
            Some(true)
        )
    );
    assert_eq!(
        repo.git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .trim(),
        "origin/feature/push"
    );

    let skipped = core
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();
    assert_eq!(
        (
            skipped.status,
            skipped.branch.as_str(),
            skipped.upstream_branch.as_deref()
        ),
        (
            PushStatus::SkippedUpToDate,
            "feature/push",
            Some("origin/feature/push")
        )
    );
    assert_eq!(
        serde_json::to_value(&skipped).unwrap()["status"],
        "skipped_up_to_date"
    );
}

#[tokio::test]
async fn pushes_run_without_the_default_command_timeout() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["remote", "add", "origin", &remote_url(&remote)]);
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let recorder = timeouts.clone();
    let core = core_with(Arc::new(ManualClock::default()), move |input| {
        if input.args.first().map(String::as_str) == Some("push") {
            recorder.lock().unwrap().push(input.timeout);
        }
        Live.run(input)
    });

    let pushed = core
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();

    assert_eq!(
        (pushed.status, pushed.set_upstream),
        (PushStatus::Pushed, Some(true))
    );
    let timeouts = timeouts.lock().unwrap();
    assert_eq!(timeouts.len(), 1);
    assert!(timeouts[0] > Duration::from_secs(60 * 60 * 24));
}

#[tokio::test]
async fn a_tracked_branch_pushes_to_the_remote_branch_name() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "origin", &remote_url(&remote)]);
    repo.git(&["push", "-u", "origin", "main"]);
    repo.commit_file("upstream.txt", "upstream\n", "Add upstream update");

    let pushed = live_core()
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();

    assert_eq!(
        (
            pushed.status,
            pushed.branch.as_str(),
            pushed.upstream_branch.as_deref(),
            pushed.set_upstream
        ),
        (PushStatus::Pushed, "main", Some("origin/main"), Some(false))
    );
    assert_eq!(
        remote.git(&["log", "-1", "--pretty=%s", "main"]).trim(),
        "Add upstream update"
    );
    assert!(remote.git(&["branch", "--list", "origin/main"]).is_empty());
}

#[tokio::test]
async fn a_branch_cut_from_another_is_published_under_its_own_name() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "origin", &remote_url(&remote)]);
    repo.git(&["push", "-u", "origin", "main"]);
    repo.git(&["checkout", "-b", "dev"]);
    repo.git(&["push", "-u", "origin", "dev"]);
    let dev_sha = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&["checkout", "-b", "feature/x", "origin/dev"]);
    repo.commit_file("feature.txt", "feature\n", "Add feature");

    let pushed = live_core()
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();

    assert_eq!(
        (
            pushed.status,
            pushed.branch.as_str(),
            pushed.upstream_branch.as_deref(),
            pushed.set_upstream
        ),
        (
            PushStatus::Pushed,
            "feature/x",
            Some("origin/feature/x"),
            Some(true)
        )
    );
    assert_eq!(
        remote
            .git(&["log", "-1", "--pretty=%s", "feature/x"])
            .trim(),
        "Add feature"
    );
    assert_eq!(
        remote.git(&["rev-parse", "dev"]),
        dev_sha,
        "the base branch is untouched"
    );
    assert_eq!(
        repo.git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .trim(),
        "origin/feature/x"
    );
    // The base it was cut from is remembered, since the upstream now points at its own branch.
    assert_eq!(
        repo.git(&["config", "branch.feature/x.gh-merge-base"])
            .trim(),
        "dev"
    );
}

#[tokio::test]
async fn a_recorded_merge_base_survives_publishing() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "origin", &remote_url(&remote)]);
    repo.git(&["push", "-u", "origin", "main"]);
    repo.git(&["checkout", "-b", "feature/y", "origin/main"]);
    repo.git(&["config", "branch.feature/y.gh-merge-base", "release/v2"]);
    repo.commit_file("feature.txt", "feature\n", "Add feature");

    let pushed = live_core()
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();

    assert_eq!(pushed.upstream_branch.as_deref(), Some("origin/feature/y"));
    assert_eq!(
        repo.git(&["config", "branch.feature/y.gh-merge-base"])
            .trim(),
        "release/v2"
    );
}

#[tokio::test]
async fn a_git_mangled_tracking_alias_still_pushes_to_its_upstream_head() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "my-org/upstream", &remote_url(&remote)]);
    repo.git(&["push", "my-org/upstream", "main:effect-atom"]);
    repo.git(&["fetch", "my-org/upstream"]);
    // `checkout --track my-org/upstream/effect-atom` cannot name the local branch
    // `effect-atom`, so git keeps `upstream/effect-atom`. Its upstream is still its published
    // head.
    repo.git(&["checkout", "--track", "my-org/upstream/effect-atom"]);
    assert_eq!(repo.branch(), "upstream/effect-atom");
    repo.commit_file("alias.txt", "alias\n", "Add alias update");

    let pushed = live_core()
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap();

    assert_eq!(
        (
            pushed.status,
            pushed.branch.as_str(),
            pushed.upstream_branch.as_deref(),
            pushed.set_upstream
        ),
        (
            PushStatus::Pushed,
            "upstream/effect-atom",
            Some("my-org/upstream/effect-atom"),
            Some(false)
        )
    );
    assert_eq!(
        remote
            .git(&["log", "-1", "--pretty=%s", "effect-atom"])
            .trim(),
        "Add alias update"
    );
}

#[tokio::test]
async fn a_requested_remote_wins_over_the_primary_remote() {
    let repo = Repo::with_commit();
    let (origin, publish) = (bare_remote(), bare_remote());
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["remote", "add", "origin", &remote_url(&origin)]);
    repo.git(&["remote", "add", "origin-1", &remote_url(&publish)]);

    let pushed = live_core()
        .push_current_branch(repo.path(), None, Some(" origin-1 "))
        .await
        .unwrap();

    assert_eq!(
        (
            pushed.status,
            pushed.upstream_branch.as_deref(),
            pushed.set_upstream
        ),
        (PushStatus::Pushed, Some("origin-1/main"), Some(true))
    );
    assert_eq!(
        publish.git(&["log", "-1", "--pretty=%s", "main"]).trim(),
        "initial commit"
    );
    assert!(origin.git(&["branch", "--list"]).is_empty());
}

#[tokio::test]
async fn pushing_needs_a_branch_and_a_remote() {
    let repo = Repo::with_commit();
    let core = live_core();
    let no_remote = core
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap_err();
    assert_eq!(
        no_remote.detail,
        "Cannot push because no git remote is configured for this repository."
    );

    repo.git(&["checkout", "--detach"]);
    let detached = core
        .push_current_branch(repo.path(), None, None)
        .await
        .unwrap_err();
    assert_eq!(detached.detail, "Cannot push from detached HEAD.");
    assert_eq!(detached.argument_count, Some(1));
}

// ---------------------------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn pull_fast_forwards_and_reports_when_nothing_changed() {
    let (repo, remote) = Repo::with_origin();
    let initial = repo.branch();
    let peer = clone_of(&remote);
    peer.commit_file("remote.txt", "remote\n", "remote commit");
    peer.git(&["push", "origin", &initial]);
    let core = live_core();

    let pulled = core.pull_current_branch(repo.path()).await.unwrap();
    assert_eq!(pulled.status, PullStatus::Pulled);
    assert_eq!(pulled.ref_name, initial);
    assert_eq!(
        pulled.upstream_ref.as_deref(),
        Some(format!("origin/{initial}").as_str())
    );
    assert_eq!(repo.read("remote.txt"), "remote\n");

    let again = core.pull_current_branch(repo.path()).await.unwrap();
    assert_eq!(again.status, PullStatus::SkippedUpToDate);
}

#[tokio::test]
async fn pull_refuses_without_a_branch_or_an_upstream() {
    let repo = Repo::with_commit();
    let core = live_core();
    let no_upstream = core.pull_current_branch(repo.path()).await.unwrap_err();
    assert_eq!(
        no_upstream.detail,
        "Current branch has no upstream configured. Push with upstream first."
    );
    repo.git(&["checkout", "--detach"]);
    let detached = core.pull_current_branch(repo.path()).await.unwrap_err();
    assert_eq!(detached.detail, "Cannot pull from detached HEAD.");
}

#[tokio::test]
async fn pull_does_not_merge_diverged_history() {
    let (repo, remote) = Repo::with_origin();
    let initial = repo.branch();
    let peer = clone_of(&remote);
    peer.commit_file("remote.txt", "remote\n", "remote commit");
    peer.git(&["push", "origin", &initial]);
    repo.commit_file("local.txt", "local\n", "local commit");

    let error = live_core()
        .pull_current_branch(repo.path())
        .await
        .unwrap_err();

    assert_eq!(error.operation, "GitVcsDriver.pullCurrentBranch.pull");
    assert_eq!(error.detail, "git pull failed");
    assert!(!repo.exists("remote.txt"));
}

// ---------------------------------------------------------------------------------------------
// Refreshing a checked-out branch
// ---------------------------------------------------------------------------------------------

fn sha(repo: &Repo, revision: &str) -> String {
    repo.git(&["rev-parse", revision]).trim().to_string()
}

#[tokio::test]
async fn a_checkout_on_the_target_commit_is_left_alone() {
    let repo = Repo::with_commit();
    let head = sha(&repo, "HEAD");

    let result = live_core()
        .refresh_checked_out_branch(repo.path(), &head, None)
        .await
        .unwrap();

    assert_eq!(
        (result.head_commit, result.moved, result.on_target),
        (head, false, true)
    );
}

#[tokio::test]
async fn a_clean_checkout_fast_forwards_and_a_dirty_one_does_not_move() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    let old = sha(&repo, "HEAD");
    repo.git(&["checkout", "-b", "next"]);
    repo.commit_file("next.txt", "next\n", "next");
    let target = sha(&repo, "HEAD");
    repo.git(&["checkout", &initial]);
    let core = live_core();

    repo.write("scratch.txt", "dirty\n");
    let dirty = core
        .refresh_checked_out_branch(repo.path(), &target, None)
        .await
        .unwrap();
    assert_eq!(
        (dirty.head_commit.as_str(), dirty.moved, dirty.on_target),
        (old.as_str(), false, false)
    );

    repo.remove("scratch.txt");
    let moved = core
        .refresh_checked_out_branch(repo.path(), &target, None)
        .await
        .unwrap();
    assert_eq!(
        (moved.head_commit.as_str(), moved.moved, moved.on_target),
        (target.as_str(), true, true)
    );
    assert_eq!(sha(&repo, "HEAD"), target);
    assert_eq!(repo.read("next.txt"), "next\n");
}

#[tokio::test]
async fn a_rewritten_head_is_taken_only_when_the_checkout_holds_no_work_of_its_own() {
    let repo = Repo::with_commit();
    repo.git(&["checkout", "-b", "pr"]);
    repo.commit_file("pr.txt", "v1\n", "pr v1");
    let before_rewrite = sha(&repo, "HEAD");
    repo.git(&["checkout", "-b", "rewritten", "HEAD~1"]);
    repo.commit_file("pr.txt", "v2\n", "pr v2");
    let target = sha(&repo, "HEAD");
    repo.git(&["checkout", "pr"]);
    let core = live_core();

    // The checkout has not been told it still sits where the upstream was: not safe to reset.
    let refused = core
        .refresh_checked_out_branch(repo.path(), &target, None)
        .await
        .unwrap();
    assert_eq!((refused.moved, refused.on_target), (false, false));
    let wrong = core
        .refresh_checked_out_branch(repo.path(), &target, Some(&target))
        .await
        .unwrap();
    assert!(!wrong.moved);
    assert_eq!(sha(&repo, "HEAD"), before_rewrite);

    let reset = core
        .refresh_checked_out_branch(repo.path(), &target, Some(&before_rewrite))
        .await
        .unwrap();
    assert_eq!(
        (reset.head_commit.as_str(), reset.moved, reset.on_target),
        (target.as_str(), true, true)
    );
    assert_eq!(sha(&repo, "HEAD"), target);
    assert_eq!(repo.read("pr.txt"), "v2\n");
    // The commit that was reset away stays findable.
    assert_eq!(sha(&repo, "refs/t3code/pre-refresh"), before_rewrite);
}
