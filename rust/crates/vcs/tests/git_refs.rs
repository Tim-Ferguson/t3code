//! `listRefs` and the caches behind it, against real repositories (ports of the corresponding
//! cases of the original GitVcsDriverCore.test.ts). Cache lifetimes are crossed by advancing
//! a manual clock.
mod common;

use std::{sync::Arc, time::Duration};

use common::{Live, Repo, bare_remote, core_with, counting_core, failed_process, live_core};
use futures_util::future::join_all;
use t3_vcs::{
    ManualClock,
    wire::{RefKind, VcsListRefsInput, VcsRef},
};
use tokio::sync::Notify;

fn list(repo: &Repo) -> VcsListRefsInput {
    VcsListRefsInput {
        cwd: repo.path().to_string_lossy().into_owned(),
        ..VcsListRefsInput::default()
    }
}

fn names(refs: &[VcsRef]) -> Vec<&str> {
    refs.iter()
        .map(|reference| reference.name.as_str())
        .collect()
}

const SNAPSHOT_SCAN: [&str; 3] = ["for-each-ref", "refs/heads", "refs/remotes"];

#[tokio::test]
async fn lists_current_and_default_refs_first_with_worktree_paths() {
    let (repo, _remote) = Repo::with_origin();
    let initial = repo.branch();
    repo.git(&["remote", "set-head", "origin", &initial]);
    repo.git(&["branch", "older"]);
    repo.commit_file("later.txt", "later\n", "later");
    repo.git(&["checkout", "-b", "feature/newest"]);
    repo.commit_file("newest.txt", "n\n", "newest");
    let worktrees = Repo::bare_dir();
    let worktree = worktrees.join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "feature/linked",
        &worktree.to_string_lossy(),
    ]);

    let result = live_core().list_refs(&list(&repo)).await.unwrap();

    assert!(result.is_repo && result.has_primary_remote);
    assert_eq!(result.total_count, result.refs.len());
    assert_eq!(result.next_cursor, None);
    // Current, then default, then everything else by recency; the remote copy of the initial
    // branch is hidden because the local one exists.
    assert_eq!(result.refs[0].name, "feature/newest");
    assert!(result.refs[0].current && !result.refs[0].is_default);
    assert_eq!(result.refs[1].name, initial);
    assert!(result.refs[1].is_default);
    assert!(!names(&result.refs).contains(&format!("origin/{initial}").as_str()));
    assert!(!names(&result.refs).contains(&"origin/HEAD"));
    let linked = result
        .refs
        .iter()
        .find(|r| r.name == "feature/linked")
        .unwrap();
    let expected = std::fs::canonicalize(&worktree).unwrap();
    assert_eq!(linked.worktree_path.as_deref(), expected.to_str());
    assert_eq!(
        result.refs[0].worktree_path.as_deref(),
        std::fs::canonicalize(repo.path()).unwrap().to_str()
    );
    assert_eq!(result.refs[1].worktree_path, None);
    assert!(result.refs.iter().all(|r| !r.is_remote));
}

#[tokio::test]
async fn pages_filters_and_selects_the_kind_of_refs() {
    let (repo, _remote) = Repo::with_origin();
    let initial = repo.branch();
    for index in 0..5 {
        repo.git(&["branch", &format!("topic/{index}")]);
    }
    let core = live_core();

    let first = core
        .list_refs(&VcsListRefsInput {
            limit: Some(2),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert_eq!(
        (first.refs.len(), first.next_cursor, first.total_count),
        (2, Some(2), 6)
    );
    let last = core
        .list_refs(&VcsListRefsInput {
            cursor: Some(4),
            limit: Some(2),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert_eq!((last.refs.len(), last.next_cursor), (2, None));

    let matching = core
        .list_refs(&VcsListRefsInput {
            query: Some("TOPIC/3".into()),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert_eq!(names(&matching.refs), ["topic/3"]);

    // The remote copy of a local branch is hidden unless asked for.
    let remote_hidden = core
        .list_refs(&VcsListRefsInput {
            ref_kind: Some(RefKind::Remote),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert!(remote_hidden.refs.is_empty());
    let remote_only = core
        .list_refs(&VcsListRefsInput {
            ref_kind: Some(RefKind::Remote),
            include_matching_remote_refs: Some(true),
            limit: Some(1),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert_eq!(
        names(&remote_only.refs),
        [format!("origin/{initial}").as_str()]
    );
    assert!(remote_only.refs[0].is_remote);
    assert_eq!(remote_only.refs[0].remote_name.as_deref(), Some("origin"));
    let complete = core
        .list_refs(&VcsListRefsInput {
            include_matching_remote_refs: Some(true),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert!(names(&complete.refs).contains(&initial.as_str()));
    assert!(names(&complete.refs).contains(&format!("origin/{initial}").as_str()));
    let local = core
        .list_refs(&VcsListRefsInput {
            ref_kind: Some(RefKind::Local),
            include_matching_remote_refs: Some(true),
            ..list(&repo)
        })
        .await
        .unwrap();
    assert!(local.refs.iter().all(|r| !r.is_remote));
}

#[tokio::test]
async fn marks_the_origin_default_ref_when_no_local_copy_exists() {
    let (repo, _remote) = Repo::with_origin();
    let initial = repo.branch();
    repo.git(&["remote", "set-head", "origin", &initial]);
    repo.git(&["checkout", "-b", "feature/only-local"]);
    repo.git(&["branch", "-D", &initial]);

    let result = live_core().list_refs(&list(&repo)).await.unwrap();

    let remote_default = result
        .refs
        .iter()
        .find(|r| r.name == format!("origin/{initial}"))
        .unwrap();
    assert!(remote_default.is_remote && remote_default.is_default);
    // Current and default refs lead the first page.
    assert_eq!(result.refs[0].name, "feature/only-local");
    assert_eq!(result.refs[1].name, format!("origin/{initial}"));
}

#[tokio::test]
async fn directories_that_are_not_repositories_or_are_gone_have_no_refs() {
    let outside = Repo::bare_dir();
    let core = live_core();
    for cwd in [outside.path().to_path_buf(), outside.join("missing")] {
        let result = core
            .list_refs(&VcsListRefsInput {
                cwd: cwd.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!result.is_repo && !result.has_primary_remote);
        assert!(result.refs.is_empty());
        assert_eq!((result.total_count, result.next_cursor), (0, None));
    }
}

#[tokio::test]
async fn one_snapshot_serves_every_page_until_a_mutation_or_a_refresh() {
    let repo = Repo::with_commit();
    let (core, clock, calls) = counting_core();

    // Concurrent first requests share one repository scan.
    let pages = join_all((0..30).map(|index| {
        let core = core.clone();
        let input = VcsListRefsInput {
            query: Some(format!("missing-{index}")),
            ..list(&repo)
        };
        tokio::spawn(async move { core.list_refs(&input).await.unwrap() })
    }))
    .await;
    assert!(pages.iter().all(|page| {
        let page = page.as_ref().unwrap();
        page.is_repo && page.refs.is_empty()
    }));
    core.list_refs(&VcsListRefsInput {
        cursor: Some(1),
        ..list(&repo)
    })
    .await
    .unwrap();
    assert_eq!(calls.count(&SNAPSHOT_SCAN), 1);
    assert_eq!(calls.count(&["worktree", "--porcelain"]), 1);

    // A mutation through the driver drops the snapshot.
    core.create_ref(repo.path(), "feature/cache-invalidation", false)
        .await
        .unwrap();
    let refreshed = core.list_refs(&list(&repo)).await.unwrap();
    assert!(names(&refreshed.refs).contains(&"feature/cache-invalidation"));
    assert_eq!(calls.count(&SNAPSHOT_SCAN), 2);

    // A change made behind the driver's back shows up when the snapshot expires.
    repo.git(&["branch", "external"]);
    assert!(!names(&core.list_refs(&list(&repo)).await.unwrap().refs).contains(&"external"));
    clock.advance(Duration::from_secs(2 * 60));
    assert!(names(&core.list_refs(&list(&repo)).await.unwrap().refs).contains(&"external"));
    assert_eq!(calls.count(&SNAPSHOT_SCAN), 3);
}

#[tokio::test]
async fn forced_refreshes_are_coalesced_for_a_few_seconds() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["branch", "external-checkout"]);
    let (core, clock, calls) = counting_core();
    let refresh = VcsListRefsInput {
        refresh: Some(true),
        ..list(&repo)
    };

    let initial_refs = core.list_refs(&refresh).await.unwrap();
    assert!(
        initial_refs
            .refs
            .iter()
            .find(|r| r.name == initial)
            .unwrap()
            .current
    );

    // An external checkout, then forced refreshes inside the coalescing window: no new scan,
    // so the answer is the one already computed.
    repo.git(&["checkout", "external-checkout"]);
    for _ in 0..3 {
        let coalesced = core.list_refs(&refresh).await.unwrap();
        assert!(
            coalesced
                .refs
                .iter()
                .find(|r| r.name == initial)
                .unwrap()
                .current
        );
    }
    assert_eq!(calls.count(&SNAPSHOT_SCAN), 1);

    clock.advance(Duration::from_secs(6));
    let after = core.list_refs(&refresh).await.unwrap();
    assert!(
        after
            .refs
            .iter()
            .find(|r| r.name == "external-checkout")
            .unwrap()
            .current
    );
    assert!(
        !after
            .refs
            .iter()
            .find(|r| r.name == initial)
            .unwrap()
            .current
    );
}

#[tokio::test]
async fn a_mutation_that_fails_after_changing_git_still_invalidates_the_snapshot() {
    let repo = Repo::with_commit();
    let core = core_with(Arc::new(ManualClock::default()), |input| {
        if input.args.first().map(String::as_str) == Some("branch")
            && input.args.get(1).map(String::as_str) == Some("feature/partial-failure")
        {
            let live = Live.run(input);
            return Box::pin(async move {
                live.await.unwrap();
                failed_process(128, "fatal: not a git repository")
            });
        }
        Live.run(input)
    });
    core.list_refs(&VcsListRefsInput {
        refresh: Some(true),
        ..list(&repo)
    })
    .await
    .unwrap();

    core.create_ref(repo.path(), "feature/partial-failure", false)
        .await
        .unwrap_err();

    let refs = core.list_refs(&list(&repo)).await.unwrap();
    assert!(names(&refs.refs).contains(&"feature/partial-failure"));
}

#[tokio::test]
async fn a_snapshot_invalidated_while_it_loads_is_read_again() {
    let repo = Repo::with_commit();
    let (worktree_scan_started, release_worktree_scan, ref_scan_done) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    let delayed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let core = {
        let (started, release, done, delayed, scans) = (
            worktree_scan_started.clone(),
            release_worktree_scan.clone(),
            ref_scan_done.clone(),
            delayed.clone(),
            scans.clone(),
        );
        core_with(Arc::new(ManualClock::default()), move |input| {
            use std::sync::atomic::Ordering::SeqCst;
            let is_worktree_scan = input.args.iter().any(|a| a == "worktree")
                && input.args.iter().any(|a| a == "--porcelain");
            let is_ref_scan = SNAPSHOT_SCAN
                .iter()
                .all(|needle| input.args.iter().any(|a| a == needle));
            let hold = is_worktree_scan && delayed.swap(false, SeqCst);
            let (started, release, done, scans) = (
                started.clone(),
                release.clone(),
                done.clone(),
                scans.clone(),
            );
            let live = Live.run(input);
            Box::pin(async move {
                if hold {
                    started.notify_one();
                    release.notified().await;
                }
                let result = live.await;
                if is_ref_scan && scans.fetch_add(1, SeqCst) == 0 {
                    done.notify_one();
                }
                result
            })
        })
    };

    let in_flight = tokio::spawn({
        let (core, input) = (
            core.clone(),
            VcsListRefsInput {
                refresh: Some(true),
                ..list(&repo)
            },
        );
        async move { core.list_refs(&input).await.unwrap() }
    });
    worktree_scan_started.notified().await;
    ref_scan_done.notified().await;
    core.create_ref(repo.path(), "feature/during-refresh", false)
        .await
        .unwrap();
    release_worktree_scan.notify_one();

    let refs = in_flight.await.unwrap();
    assert!(names(&refs.refs).contains(&"feature/during-refresh"));
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_failed_scan_is_not_retried_during_the_cooldown() {
    let repo = Repo::with_commit();
    let (clock, calls) = (Arc::new(ManualClock::default()), common::Calls::default());
    let recorder = calls.clone();
    let core = core_with(clock.clone(), move |input| {
        recorder.record(&input);
        if input.args.iter().any(|a| a == "for-each-ref") {
            return Box::pin(async { failed_process(128, "fatal: not a git repository") });
        }
        Live.run(input)
    });
    let refresh = VcsListRefsInput {
        refresh: Some(true),
        ..list(&repo)
    };

    let error = core.list_refs(&refresh).await.unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.listRefs.snapshotRefs");
    assert_eq!(error.detail, "Git ref snapshot enumeration failed.");
    assert_eq!(error.exit_code, Some(128));
    core.list_refs(&refresh).await.unwrap_err();
    assert_eq!(calls.count(&["for-each-ref"]), 1);

    clock.advance(Duration::from_secs(30));
    core.list_refs(&refresh).await.unwrap_err();
    assert_eq!(calls.count(&["for-each-ref"]), 2);
}

#[tokio::test]
async fn the_current_branch_is_marked_without_worktree_metadata() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    let core = core_with(Arc::new(ManualClock::default()), |input| {
        let unavailable = input.args.iter().any(|a| a == "--show-toplevel")
            || (input.args.iter().any(|a| a == "worktree")
                && input.args.iter().any(|a| a == "--porcelain"));
        if unavailable {
            return Box::pin(async { failed_process(128, "fatal: not a git repository") });
        }
        Live.run(input)
    });

    let refs = core
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list(&repo)
        })
        .await
        .unwrap();

    assert!(refs.is_repo);
    assert!(
        refs.refs
            .iter()
            .find(|r| r.name == initial)
            .unwrap()
            .current
    );
}

#[tokio::test]
async fn worktrees_whose_directory_is_gone_are_ignored() {
    let repo = Repo::with_commit();
    repo.git(&["branch", "stale-worktree"]);
    let core = core_with(Arc::new(ManualClock::default()), |input| {
        if input.args.iter().any(|a| a == "worktree")
            && input.args.iter().any(|a| a == "--porcelain")
        {
            return Box::pin(async {
                Ok(t3_vcs::process::ProcessRunOutput {
                    code: Some(0),
                    stdout: "worktree /missing/deleted-worktree\0HEAD deadbeef\0branch refs/heads/stale-worktree\0\0".into(),
                    ..Default::default()
                })
            });
        }
        Live.run(input)
    });

    let refs = core
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list(&repo)
        })
        .await
        .unwrap();

    assert_eq!(
        refs.refs
            .iter()
            .find(|r| r.name == "stale-worktree")
            .unwrap()
            .worktree_path,
        None
    );
}

#[tokio::test]
async fn the_remote_a_branch_publishes_to_leads_has_primary_remote() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    let core = live_core();
    assert!(
        !core
            .list_refs(&list(&repo))
            .await
            .unwrap()
            .has_primary_remote
    );

    core.ensure_remote(repo.path(), "origin", &remote.path().to_string_lossy())
        .await
        .unwrap();

    // ensure_remote is a mutation, so the next page already sees the remote.
    assert!(
        core.list_refs(&list(&repo))
            .await
            .unwrap()
            .has_primary_remote
    );
}
