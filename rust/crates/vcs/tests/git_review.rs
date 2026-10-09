//! Review diffs, whole-file contents and Changes totals against real repositories (ports of
//! the "review diff previews" cases of the original GitVcsDriverCore.test.ts).
mod common;

use std::sync::Arc;

use common::{Live, Repo, bare_remote, core_with, failed_process, live_core};
use t3_vcs::{
    ManualClock,
    status::LocalStatusOptions,
    wire::{
        ReviewDiffChangeType, ReviewDiffFileContentsInput, ReviewDiffFileRequest,
        ReviewDiffFileStat, ReviewDiffPreviewInput, ReviewDiffPreviewResult,
        ReviewDiffPreviewSource, ReviewDiffSourceKind,
    },
};

fn preview_input(repo: &Repo) -> ReviewDiffPreviewInput {
    ReviewDiffPreviewInput {
        cwd: repo.path().to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn with_base(repo: &Repo, base: &str) -> ReviewDiffPreviewInput {
    ReviewDiffPreviewInput {
        base_ref: Some(base.into()),
        ..preview_input(repo)
    }
}

fn source(
    result: &ReviewDiffPreviewResult,
    kind: ReviewDiffSourceKind,
) -> &ReviewDiffPreviewSource {
    result
        .sources
        .iter()
        .find(|source| source.kind == kind)
        .unwrap()
}

fn stat(path: &str, previous: Option<&str>, additions: u64, deletions: u64) -> ReviewDiffFileStat {
    ReviewDiffFileStat {
        path: path.into(),
        previous_path: previous.map(String::from),
        additions,
        deletions,
    }
}

fn file(
    path: &str,
    previous: Option<&str>,
    kind: ReviewDiffSourceKind,
) -> Option<ReviewDiffFileRequest> {
    Some(ReviewDiffFileRequest {
        path: path.into(),
        previous_path: previous.map(String::from),
        source_kind: kind,
    })
}

const DIRTY: ReviewDiffSourceKind = ReviewDiffSourceKind::WorkingTree;
const CHANGES: ReviewDiffSourceKind = ReviewDiffSourceKind::BranchRange;

fn contents_input(
    repo: &Repo,
    f: impl FnOnce(&mut ReviewDiffFileContentsInput),
) -> ReviewDiffFileContentsInput {
    let mut input = ReviewDiffFileContentsInput {
        cwd: repo.path().to_string_lossy().into_owned(),
        source_kind: DIRTY,
        change_type: ReviewDiffChangeType::Change,
        base_ref: Some("HEAD".into()),
        head_ref: None,
        old_path: "README.md".into(),
        new_path: "README.md".into(),
    };
    f(&mut input);
    input
}

#[tokio::test]
async fn files_load_from_a_nested_project_directory_and_scope_to_one_patch() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature/nested"]);
    repo.commit_file("nested/tracked.txt", "committed\n", "nested file");
    repo.write("nested/tracked.txt", "changed\n");
    repo.write("untracked.txt", "new\n");
    let nested = repo.join("nested");
    let input = ReviewDiffPreviewInput {
        cwd: nested.to_string_lossy().into_owned(),
        base_ref: Some(initial),
        ..Default::default()
    };
    let core = live_core();

    let preview = core.get_review_diff_preview(&input).await.unwrap();
    assert_eq!(source(&preview, DIRTY).files.as_ref().unwrap().len(), 2);
    for kind in [DIRTY, CHANGES] {
        for stat in source(&preview, kind).files.clone().unwrap() {
            let scoped = core
                .get_review_diff_preview(&ReviewDiffPreviewInput {
                    file: file(&stat.path, stat.previous_path.as_deref(), kind),
                    ..input.clone()
                })
                .await
                .unwrap();
            let patch = source(&scoped, kind);
            assert_eq!(patch.files.as_ref().unwrap(), &vec![stat.clone()]);
            assert!(patch.diff.contains(&format!("b/{}", stat.path)));
        }
    }
    assert_eq!(preview.cwd, input.cwd);
    assert_eq!(
        preview
            .sources
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["working-tree", "branch-range"]
    );
}

#[tokio::test]
async fn a_directory_outside_any_worktree_has_no_sources() {
    let outside = Repo::bare_dir();
    for cwd in [outside.path().to_path_buf(), outside.join("missing")] {
        let result = live_core()
            .get_review_diff_preview(&ReviewDiffPreviewInput {
                cwd: cwd.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.sources.is_empty());
    }
}

#[tokio::test]
async fn whitespace_filtering_applies_to_both_sources() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature/whitespace"]);
    repo.commit_file("README.md", "#  test\n", "change whitespace");
    repo.write("README.md", "#   test\n");
    let core = live_core();

    let included = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    assert!(
        !source(&included, DIRTY).diff.is_empty() && !source(&included, CHANGES).diff.is_empty()
    );

    let ignored = core
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            ignore_whitespace: Some(true),
            ..with_base(&repo, &initial)
        })
        .await
        .unwrap();
    for kind in [DIRTY, CHANGES] {
        assert_eq!(source(&ignored, kind).diff, "");
        assert_eq!(source(&ignored, kind).files.as_deref(), Some(&[][..]));
    }
}

#[tokio::test]
async fn patches_keep_a_and_b_prefixes_when_the_repository_disables_them() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["config", "diff.noprefix", "true"]);
    repo.git(&["config", "diff.mnemonicPrefix", "true"]);
    repo.git(&["checkout", "-b", "feature/noprefix"]);
    repo.commit_file("README.md", "# committed change\n", "committed change");
    repo.write("README.md", "# dirty change\n");
    repo.write("untracked.txt", "untracked\n");

    let preview = live_core()
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();

    assert!(
        source(&preview, DIRTY)
            .diff
            .contains("diff --git a/README.md b/README.md")
    );
    assert!(source(&preview, DIRTY).diff.contains("+++ b/untracked.txt"));
    assert!(
        source(&preview, CHANGES)
            .diff
            .contains("diff --git a/README.md b/README.md")
    );
}

#[tokio::test]
async fn untracked_files_with_pathspec_magic_are_shown_without_touching_the_index() {
    let repo = Repo::with_commit();
    repo.write(":(exclude)after.ts", "literal pathspec contents\n");
    repo.write("ordinary.ts", "ordinary contents\n");
    let index_before = repo.git(&["ls-files", "--stage"]);
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    let diff = &source(&preview, DIRTY).diff;
    assert!(diff.contains("+literal pathspec contents") && diff.contains("+ordinary contents"));

    let scoped = core
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            file: file(":(exclude)after.ts", None, DIRTY),
            ..preview_input(&repo)
        })
        .await
        .unwrap();
    let scoped = source(&scoped, DIRTY);
    assert_eq!(
        scoped.files.as_ref().unwrap(),
        &vec![stat(":(exclude)after.ts", None, 1, 0)]
    );
    assert!(
        scoped.diff.contains("+literal pathspec contents") && !scoped.diff.contains("ordinary.ts")
    );
    assert_eq!(repo.git(&["ls-files", "--stage"]), index_before);
}

#[tokio::test]
async fn an_unstaged_rename_with_edits_is_detected_without_mutating_a_split_index() {
    let repo = Repo::with_commit();
    repo.commit_file(
        "before.ts",
        "one\ntwo\nthree\nfour\nfive\n",
        "add source file",
    );
    repo.git(&["config", "core.splitIndex", "true"]);
    repo.git(&["config", "splitIndex.sharedIndexExpire", "now"]);
    repo.git(&["update-index", "--split-index"]);
    let index_hash = || repo.git(&["hash-object", ".git/index"]);
    let shared = || {
        let mut names: Vec<_> = std::fs::read_dir(repo.join(".git"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("sharedindex."))
            .collect();
        names.sort();
        names
    };
    let (hash_before, shared_before) = (index_hash(), shared());
    std::fs::rename(repo.join("before.ts"), repo.join("after.ts")).unwrap();
    repo.write("after.ts", "one\ntwo\nTHREE\nfour\nfive\n");
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    let dirty = source(&preview, DIRTY);

    assert_eq!(
        dirty.files.as_ref().unwrap(),
        &vec![stat("after.ts", Some("before.ts"), 1, 1)]
    );
    for expected in [
        "rename from before.ts",
        "rename to after.ts",
        "-three",
        "+THREE",
    ] {
        assert!(dirty.diff.contains(expected), "{expected}");
    }
    assert_eq!(dirty.diff.matches("diff --git ").count(), 1);
    let scoped = core
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            file: file("after.ts", Some("before.ts"), DIRTY),
            ..preview_input(&repo)
        })
        .await
        .unwrap();
    assert_eq!(source(&scoped, DIRTY).diff, dirty.diff);
    assert_eq!((index_hash(), shared()), (hash_before, shared_before));
}

#[tokio::test]
async fn tracked_changes_stay_visible_when_untracked_discovery_fails() {
    let repo = Repo::with_commit();
    let core = core_with(Arc::new(ManualClock::default()), |input| {
        if input.args.first().map(String::as_str) == Some("ls-files")
            && input.args.get(1).map(String::as_str) == Some("--others")
        {
            return Box::pin(async { failed_process(128, "fatal: not a git repository") });
        }
        Live.run(input)
    });
    repo.write("README.md", "# tracked change\n");

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();

    let diff = &source(&preview, DIRTY).diff;
    assert!(diff.contains("-# test") && diff.contains("+# tracked change"));
}

#[tokio::test]
async fn patch_failures_propagate_instead_of_reporting_an_empty_diff() {
    let repo = Repo::with_commit();
    repo.write("README.md", "changed\n");
    let core = core_with(Arc::new(ManualClock::default()), |input| {
        if input.args.iter().any(|arg| arg == "--patch") {
            return Box::pin(async { failed_process(128, "fatal: not a git repository") });
        }
        Live.run(input)
    });

    let error = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap_err();

    assert_eq!(error.operation, "GitVcsDriver.getReviewDiffPreview.patch");
}

#[tokio::test]
async fn a_staged_deletion_is_kept_when_the_removed_path_still_exists() {
    let repo = Repo::with_commit();
    repo.commit_file("removed.txt", "remove me\n", "add removable file");
    repo.git(&["rm", "--cached", "removed.txt"]);

    let preview = live_core()
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();

    let diff = &source(&preview, DIRTY).diff;
    assert!(
        diff.contains("deleted file mode")
            && diff.contains("-remove me")
            && !diff.contains("new file mode")
    );
}

#[tokio::test]
async fn files_are_visible_before_the_first_commit() {
    let repo = Repo::init();
    repo.write("untracked.txt", "visible before HEAD\n");
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    let dirty = source(&preview, DIRTY);
    assert!(dirty.diff.contains("visible before HEAD") && !dirty.truncated);

    repo.write("staged.txt", "staged\n");
    repo.git(&["add", "staged.txt"]);
    let both = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    assert_eq!(
        source(&both, DIRTY).files.as_ref().unwrap(),
        &vec![
            stat("staged.txt", None, 1, 0),
            stat("untracked.txt", None, 1, 0)
        ]
    );
}

#[tokio::test]
async fn complete_stats_survive_the_combined_patch_limit() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    let large = "a long line of changed content for the diff preview\n".repeat(4000);
    repo.git(&["checkout", "-b", "feature/large"]);
    repo.write("a-large.txt", &large);
    repo.write("z-last.txt", "last file\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "large change"]);
    repo.write("a-large.txt", &large.replace("changed", "updated"));
    repo.write("z-last.txt", "last file updated\n");
    repo.write("untracked.txt", &large);
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    let (branch, dirty) = (source(&preview, CHANGES), source(&preview, DIRTY));

    assert!(branch.truncated && dirty.truncated);
    assert!(!branch.diff.contains("z-last.txt"));
    assert_eq!(
        branch.files.as_ref().unwrap(),
        &vec![
            stat("a-large.txt", None, 4000, 0),
            stat("untracked.txt", None, 4000, 0),
            stat("z-last.txt", None, 1, 0)
        ]
    );
    assert_eq!(
        dirty.files.as_ref().unwrap(),
        &vec![
            stat("a-large.txt", None, 4000, 4000),
            stat("untracked.txt", None, 4000, 0),
            stat("z-last.txt", None, 1, 1)
        ]
    );
    for (kind, source) in [(CHANGES, branch), (DIRTY, dirty)] {
        for stat in source.files.as_ref().unwrap() {
            let individual = core
                .get_review_diff_preview(&ReviewDiffPreviewInput {
                    file: file(&stat.path, stat.previous_path.as_deref(), kind),
                    ..with_base(&repo, &initial)
                })
                .await
                .unwrap();
            let patch = self::source(&individual, kind);
            assert!(!patch.truncated);
            assert_eq!(patch.files.as_ref().unwrap(), &vec![stat.clone()]);
            assert!(patch.diff.contains(&format!("b/{}", stat.path)));
            let other = individual.sources.iter().find(|s| s.kind != kind).unwrap();
            assert!(other.diff.is_empty());
        }
    }
}

#[tokio::test]
async fn renames_unusual_paths_modes_and_binary_files_keep_their_statistics() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.commit_file(
        "mode-only.sh",
        "echo unchanged\n",
        "add executable candidate",
    );
    repo.git(&["checkout", "-b", "feature/paths"]);
    repo.git(&["mv", "README.md", "renamed.md"]);
    repo.write("[literal].txt", "literal\n");
    repo.write(" leading.txt", "whitespace path\n");
    repo.write("l.txt", "other\n");
    std::fs::write(repo.join("binary.dat"), b"binary\0data").unwrap();
    repo.write("tab\tand\nnewline.txt", "unusual path\n");
    repo.git(&["add", "."]);
    repo.git(&["update-index", "--chmod=+x", "mode-only.sh"]);
    repo.git(&["commit", "-m", "rename and add files"]);
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    let branch = source(&preview, CHANGES);
    let files = branch.files.as_ref().unwrap();
    // (The original also scopes to a mode-only change, which the working tree on disk does not
    // have here: Changes compares with the files on disk, whose mode is the committed one.)
    for path in ["renamed.md", "[literal].txt", " leading.txt"] {
        let expected = files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} in {files:?}"));
        let scoped = core
            .get_review_diff_preview(&ReviewDiffPreviewInput {
                file: file(path, expected.previous_path.as_deref(), CHANGES),
                ..with_base(&repo, &initial)
            })
            .await
            .unwrap();
        let scoped = source(&scoped, CHANGES);
        assert_eq!(scoped.files.as_ref().unwrap(), &vec![expected.clone()]);
        assert!(!scoped.diff.contains("b/l.txt"));
        if path == "renamed.md" {
            assert!(scoped.diff.contains("rename from README.md"));
        }
    }
    assert!(
        branch.diff.contains("rename from README.md")
            && branch.diff.contains("rename to renamed.md")
    );
    for expected in [
        stat("renamed.md", Some("README.md"), 0, 0),
        stat("binary.dat", None, 0, 0),
        stat("tab\tand\nnewline.txt", None, 1, 0),
    ] {
        assert!(files.contains(&expected), "{expected:?}");
    }
}

#[tokio::test]
async fn the_review_index_keeps_files_changed_in_the_preceding_second_cached() {
    // Same-size edits with unchanged timestamps are only visible to git when the temporary
    // index keeps the real index's whole-second mtime.
    for (timestamp, split) in [
        (1_700_000_000.0, false),
        (1_700_000_000.9999, false),
        (1_700_000_000.0, true),
        (1_700_000_000.9999, true),
    ] {
        let repo = Repo::with_commit();
        repo.git(&["config", "core.trustctime", "false"]);
        repo.write("tracked.txt", "before\n");
        common::set_times(&repo.join("tracked.txt"), timestamp);
        repo.git(&["add", "tracked.txt"]);
        repo.git(&["commit", "-m", "record racy file"]);
        if split {
            repo.git(&["update-index", "--split-index"]);
        }
        common::set_times(&repo.join(".git/index"), timestamp);
        let index_before = repo.index_bytes();
        repo.write("tracked.txt", "after!\n");
        common::set_times(&repo.join("tracked.txt"), timestamp);
        repo.write("untracked.txt", "new\n");

        let preview = live_core()
            .get_review_diff_preview(&preview_input(&repo))
            .await
            .unwrap();

        let dirty = source(&preview, DIRTY);
        assert_eq!(
            dirty.files.as_ref().unwrap(),
            &vec![
                stat("tracked.txt", None, 1, 1),
                stat("untracked.txt", None, 1, 0)
            ],
            "{timestamp} split {split}"
        );
        assert!(dirty.diff.contains("-before") && dirty.diff.contains("+after!"));
        assert_eq!(repo.index_bytes(), index_before);
    }
}

// ---------------------------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn changes_combine_commits_uncommitted_edits_and_untracked_files() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature/combined"]);
    repo.write("README.md", "# test\ncommitted\n");
    repo.git(&["commit", "-am", "commit edit"]);
    let core = live_core();

    // A fully committed branch has no uncommitted work, but Changes still shows it.
    let clean = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    assert_eq!(source(&clean, DIRTY).files.as_deref(), Some(&[][..]));
    assert_eq!(
        source(&clean, CHANGES).files.as_ref().unwrap(),
        &vec![stat("README.md", None, 1, 0)]
    );

    repo.write("README.md", "# test\ncommitted\ndirty\n");
    repo.write("new.txt", "new\n");
    let dirty = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    let changes = source(&dirty, CHANGES);
    assert_eq!(
        changes.files.as_ref().unwrap(),
        &vec![stat("README.md", None, 2, 0), stat("new.txt", None, 1, 0)]
    );
    assert_eq!(changes.diff.matches("diff --git a/README.md ").count(), 1);
    assert!(changes.diff.contains("+dirty"));
    assert_eq!(
        source(&dirty, DIRTY).files.as_ref().unwrap(),
        &vec![stat("README.md", None, 1, 0), stat("new.txt", None, 1, 0)]
    );
    assert_eq!(changes.title, format!("Changes vs {initial}"));
    assert_eq!(changes.head_ref.as_deref(), Some("feature/combined"));
}

#[tokio::test]
async fn the_default_branch_is_compared_with_its_remote_copy() {
    let repo = Repo::with_commit();
    let remote = bare_remote();
    repo.git(&["branch", "-M", "main"]);
    repo.commit_file("unpushed.txt", "unpushed\n", "unpushed");
    let core = live_core();

    // Without a remote there is no base, so Changes equals Uncommitted.
    let local = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    assert_eq!(source(&local, CHANGES).base_ref, None);
    assert_eq!(source(&local, CHANGES).title, "Changes");
    assert_eq!(source(&local, CHANGES).files.as_deref(), Some(&[][..]));

    repo.git(&["remote", "add", "origin", &remote.path().to_string_lossy()]);
    repo.git(&["push", "origin", "HEAD~1:refs/heads/main"]);
    repo.git(&["fetch", "origin"]);
    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    let changes = source(&preview, CHANGES);
    assert_eq!(changes.base_ref.as_deref(), Some("origin/main"));
    assert_eq!(
        changes.files.as_ref().unwrap(),
        &vec![stat("unpushed.txt", None, 1, 0)]
    );
}

#[tokio::test]
async fn an_explicit_base_works_on_a_detached_head_and_a_bad_one_is_rejected() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "--detach"]);
    repo.commit_file("detached.txt", "detached\n", "detached work");
    let core = live_core();

    let explicit = core
        .get_review_diff_preview(&with_base(&repo, &initial))
        .await
        .unwrap();
    assert_eq!(
        source(&explicit, CHANGES).files.as_ref().unwrap(),
        &vec![stat("detached.txt", None, 1, 0)]
    );
    assert_eq!(source(&explicit, CHANGES).head_ref.as_deref(), Some("HEAD"));
    let implicit = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    assert_eq!(source(&implicit, CHANGES).files.as_deref(), Some(&[][..]));

    let error = core
        .get_review_diff_preview(&with_base(&repo, "missing-base"))
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.resolveReviewMergeBase");
}

#[tokio::test]
async fn changes_before_the_first_commit_compare_with_the_empty_tree() {
    let upstream = Repo::with_commit();
    upstream.git(&["branch", "-M", "main"]);
    let repo = Repo::bare_dir();
    repo.git(&["init", "-b", "main"]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        &upstream.path().to_string_lossy(),
    ]);
    repo.git(&["fetch", "origin"]);
    repo.write("new.txt", "one\ntwo\n");
    let core = live_core();

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    let changes = source(&preview, CHANGES);
    assert_eq!(changes.base_ref, None);
    assert_eq!(
        changes.files.as_ref().unwrap(),
        &vec![stat("new.txt", None, 2, 0)]
    );

    let status = core
        .status_details_local(
            repo.path(),
            LocalStatusOptions {
                include_branch_changes: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let totals = status.branch_changes.unwrap();
    assert_eq!(
        (totals.base_ref, totals.insertions, totals.deletions),
        (None, 2, 0)
    );
}

#[tokio::test]
async fn changes_do_not_show_newer_base_commits_as_deletions() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature/rebased"]);
    repo.commit_file("feature.txt", "feature\n", "feature work");
    repo.git(&["checkout", &initial]);
    repo.commit_file("upstream.txt", "upstream\n", "base moves");
    repo.git(&["checkout", "feature/rebased"]);

    for rebase in [false, true] {
        if rebase {
            repo.git(&["rebase", &initial]);
        }
        let preview = live_core()
            .get_review_diff_preview(&with_base(&repo, &initial))
            .await
            .unwrap();
        assert_eq!(
            source(&preview, CHANGES).files.as_ref().unwrap(),
            &vec![stat("feature.txt", None, 1, 0)]
        );
    }
}

#[tokio::test]
async fn changes_totals_count_untracked_files_and_the_base_branch() {
    let repo = Repo::with_commit();
    repo.git(&["branch", "-M", "main"]);
    repo.git(&["checkout", "-b", "feature/totals"]);
    repo.write("README.md", "# test\ncommitted\n");
    repo.git(&["commit", "-am", "commit edit"]);
    repo.write("nested/untracked.txt", "one\ntwo\n");
    let core = live_core();

    let status = core
        .status_details_local(
            &repo.join("nested"),
            LocalStatusOptions {
                include_branch_changes: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let totals = status.branch_changes.unwrap();
    assert_eq!(
        (
            totals.base_ref.as_deref(),
            totals.insertions,
            totals.deletions
        ),
        (Some("main"), 3, 0)
    );
    assert!(
        core.status_details_local(repo.path(), Default::default())
            .await
            .unwrap()
            .branch_changes
            .is_none()
    );
}

#[tokio::test]
async fn thousands_of_untracked_files_leave_totals_out_and_mark_the_preview_incomplete() {
    let repo = Repo::with_commit();
    for index in 0..5_001 {
        repo.write(&format!("bulk/{index}.txt"), "x\n");
    }
    repo.write("README.md", "changed\n");
    let core = live_core();

    let status = core
        .status_details_local(
            repo.path(),
            LocalStatusOptions {
                include_branch_changes: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(status.has_working_tree_changes && status.branch_changes.is_none());

    let preview = core
        .get_review_diff_preview(&preview_input(&repo))
        .await
        .unwrap();
    assert!(!preview.sources.is_empty());
    assert!(
        preview
            .sources
            .iter()
            .all(|source| source.truncated && source.files.is_none())
    );
    assert!(source(&preview, DIRTY).diff.contains("+changed"));
}

// ---------------------------------------------------------------------------------------------
// File contents
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn working_tree_contents_pair_the_commit_with_the_file_on_disk() {
    let repo = Repo::with_commit();
    repo.write("nested/.keep", "");
    repo.write("README.md", "# changed\nunchanged context\n");

    let contents = live_core()
        .get_review_diff_file_contents(&contents_input(&repo, |input| {
            input.cwd = repo.join("nested").to_string_lossy().into_owned();
        }))
        .await
        .unwrap();

    assert_eq!(contents.old_contents, "# test\n");
    assert_eq!(contents.new_contents, "# changed\nunchanged context\n");
}

#[tokio::test]
async fn new_and_deleted_files_do_not_read_their_missing_side() {
    let repo = Repo::with_commit();
    repo.write("added.ts", "export const added = true;\n");
    repo.remove("README.md");
    let core = live_core();

    let added = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.old_path = "added.ts".into();
            i.new_path = "added.ts".into();
        }))
        .await
        .unwrap();
    let deleted = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::Deleted
        }))
        .await
        .unwrap();

    assert_eq!(
        (added.old_contents.as_str(), added.new_contents.as_str()),
        ("", "export const added = true;\n")
    );
    assert_eq!(
        (deleted.old_contents.as_str(), deleted.new_contents.as_str()),
        ("# test\n", "")
    );
}

#[tokio::test]
async fn changes_contents_use_the_merge_base_and_the_disk() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature/context"]);
    repo.commit_file(
        "README.md",
        "# branch change\nunchanged context\n",
        "change readme",
    );
    repo.write("README.md", "# dirty change\nunchanged context\n");

    let contents = live_core()
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.source_kind = CHANGES;
            i.base_ref = Some(initial);
            i.head_ref = Some("feature/context".into());
        }))
        .await
        .unwrap();

    assert_eq!(contents.old_contents, "# test\n");
    assert_eq!(contents.new_contents, "# dirty change\nunchanged context\n");
}

#[tokio::test]
async fn file_contents_failures_name_the_failing_operation() {
    let repo = Repo::with_commit();
    let core = live_core();

    let missing = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.old_path = "missing.ts".into();
            i.new_path = "missing.ts".into();
        }))
        .await
        .unwrap_err();
    assert_eq!(
        missing.operation,
        "GitVcsDriver.getReviewDiffFileContents.workingTree.fs.realPath"
    );
    assert_eq!(missing.command, "fs.realPath");
    assert_eq!(missing.detail, "Could not resolve diff file 'missing.ts'.");
    assert_eq!(missing.cwd, repo.path().to_string_lossy());
    assert!(missing.cause.is_some());

    let escape = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.new_path = "../outside.txt".into();
        }))
        .await
        .unwrap_err();
    assert_eq!(
        escape.operation,
        "GitVcsDriver.getReviewDiffFileContents.workingTree.path.resolve"
    );
    assert_eq!(
        escape.detail,
        "Diff file '../outside.txt' resolves outside the review workspace."
    );

    repo.write("dir/file.txt", "x\n");
    let not_a_file = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.new_path = "dir".into();
        }))
        .await
        .unwrap_err();
    assert_eq!(not_a_file.detail, "Diff path 'dir' is not a file.");

    std::fs::write(repo.join("bin.dat"), b"a\0b").unwrap();
    let binary = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.new_path = "bin.dat".into();
        }))
        .await
        .unwrap_err();
    assert_eq!(binary.detail, "Cannot expand binary file 'bin.dat'.");

    std::fs::write(repo.join("huge.txt"), vec![b'a'; 1024 * 1024 + 1]).unwrap();
    let huge = core
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.new_path = "huge.txt".into();
        }))
        .await
        .unwrap_err();
    assert_eq!(
        huge.detail,
        "Diff file 'huge.txt' exceeds the 1 MB expansion limit."
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_refused() {
    let repo = Repo::with_commit();
    let outside = Repo::bare_dir();
    outside.write("secret.txt", "secret\n");
    std::os::unix::fs::symlink(outside.join("secret.txt"), repo.join("link.txt")).unwrap();

    let error = live_core()
        .get_review_diff_file_contents(&contents_input(&repo, |i| {
            i.change_type = ReviewDiffChangeType::New;
            i.new_path = "link.txt".into();
        }))
        .await
        .unwrap_err();

    assert_eq!(
        error.operation,
        "GitVcsDriver.getReviewDiffFileContents.workingTree.fs.realPath"
    );
    assert!(
        error
            .detail
            .contains("resolves outside the review workspace")
    );
}
