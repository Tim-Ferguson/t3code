//! Repository queries, refs, worktrees and status against a real git binary (ports of the
//! corresponding cases of the original GitVcsDriver*.test.ts).
mod common;

use std::sync::Arc;

use common::{Live, LiveRunner, Repo, live_core, live_git};
use t3_vcs::{
    VcsDriverKind,
    core::{
        CreateWorktreeOptions, GitCommandFailureReason, GitCore, VcsCreateWorktreeInput,
        WorktreeSubmodules,
    },
    status::LocalStatusOptions,
};

fn current_branch(repo: &Repo) -> String {
    repo.git(&["branch", "--show-current"]).trim().to_string()
}

fn input(
    repo: &Repo,
    ref_name: &str,
    new_ref: Option<&str>,
    path: Option<std::path::PathBuf>,
) -> VcsCreateWorktreeInput {
    VcsCreateWorktreeInput {
        cwd: repo.path().to_path_buf(),
        ref_name: ref_name.into(),
        new_ref_name: new_ref.map(String::from),
        base_ref_name: None,
        path,
    }
}

fn options(worktrees: &Repo) -> CreateWorktreeOptions {
    CreateWorktreeOptions {
        worktrees_directory: worktrees.path().to_path_buf(),
        submodules: WorktreeSubmodules::None,
        progress: None,
    }
}

// ---------------------------------------------------------------------------------------------
// GitVcs: the VcsDriver surface
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn detects_a_repository_and_reports_its_identity() {
    let git = live_git();
    let outside = Repo::bare_dir();
    assert!(
        git.detect_repository(outside.path())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!git.is_inside_work_tree(outside.path()).await.unwrap());

    let repo = Repo::with_commit();
    std::fs::create_dir_all(repo.join("packages/app")).unwrap();
    let identity = git
        .detect_repository(&repo.join("packages/app"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity.kind, VcsDriverKind::Git);
    assert_eq!(
        std::fs::canonicalize(&identity.root_path).unwrap(),
        std::fs::canonicalize(repo.path()).unwrap()
    );
    // `--git-common-dir` is relative to the directory it ran in.
    assert_eq!(identity.metadata_path.as_deref(), Some("../../.git"));
    assert_eq!(
        identity.freshness.source,
        t3_vcs::git::VcsFreshnessSource::LiveLocal
    );
    assert!(identity.freshness.expires_at.is_none());
    assert_eq!(git.capabilities().kind, VcsDriverKind::Git);
    assert!(git.capabilities().supports_worktrees);
}

#[tokio::test]
async fn lists_tracked_and_unignored_untracked_files() {
    let repo = Repo::with_commit();
    repo.write(".gitignore", "ignored.log\n");
    repo.write("src/lib.rs", "x\n");
    repo.write("ignored.log", "x\n");
    repo.write("with space/\u{540d}.txt", "x\n");
    let listing = live_git().list_workspace_files(repo.path()).await.unwrap();
    assert!(!listing.truncated);
    let mut paths = listing.paths.clone();
    paths.sort();
    assert_eq!(
        paths,
        [
            ".gitignore",
            "README.md",
            "src/lib.rs",
            "with space/\u{540d}.txt"
        ]
    );

    let outside = Repo::bare_dir();
    let error = live_git()
        .list_workspace_files(outside.path())
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
}

#[tokio::test]
async fn lists_remotes_with_their_fetch_and_push_urls() {
    let repo = Repo::with_commit();
    repo.git(&["remote", "add", "origin", "https://example.com/a.git"]);
    repo.git(&["remote", "add", "fork", "https://example.com/fork.git"]);
    repo.git(&[
        "remote",
        "set-url",
        "--push",
        "fork",
        "git@example.com:fork.git",
    ]);
    let remotes = live_git().list_remotes(repo.path()).await.unwrap().remotes;
    assert_eq!(remotes.len(), 2);
    let origin = remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .unwrap();
    assert_eq!(origin.url, "https://example.com/a.git");
    assert_eq!(
        origin.push_url.as_deref(),
        Some("https://example.com/a.git")
    );
    assert!(origin.is_primary);
    let fork = remotes.iter().find(|remote| remote.name == "fork").unwrap();
    assert_eq!(fork.push_url.as_deref(), Some("git@example.com:fork.git"));
    assert!(!fork.is_primary);
}

#[tokio::test]
async fn filters_ignored_paths_and_keeps_the_callers_order() {
    let repo = Repo::with_commit();
    repo.write(".gitignore", "*.log\nbuild/\n");
    let paths: Vec<String> = [
        "src/a.rs",
        "debug.log",
        "build/out.o",
        "README.md",
        "weird\nname.log",
        "z.rs",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let kept = live_git()
        .filter_ignored_paths(repo.path(), &paths)
        .await
        .unwrap();
    assert_eq!(kept, ["src/a.rs", "README.md", "z.rs"]);
    assert_eq!(
        live_git()
            .filter_ignored_paths(repo.path(), &[])
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    let none_ignored = vec!["a.rs".to_string()];
    assert_eq!(
        live_git()
            .filter_ignored_paths(repo.path(), &none_ignored)
            .await
            .unwrap(),
        none_ignored
    );
}

#[tokio::test]
async fn init_repository_creates_a_repository() {
    let dir = Repo::bare_dir();
    live_git().init_repository(dir.path()).await.unwrap();
    assert!(dir.join(".git").is_dir());
    assert!(live_git().is_inside_work_tree(dir.path()).await.unwrap());
}

// ---------------------------------------------------------------------------------------------
// GitCore: refs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn creates_checks_out_and_renames_branches() {
    let repo = Repo::with_commit();
    let core = live_core();
    assert_eq!(
        core.create_ref(repo.path(), "feature/original", false)
            .await
            .unwrap(),
        "feature/original"
    );
    assert_eq!(
        core.switch_ref(repo.path(), "feature/original")
            .await
            .unwrap()
            .as_deref(),
        Some("feature/original")
    );
    assert_eq!(
        core.rename_branch(repo.path(), "feature/original", "feature/renamed", false)
            .await
            .unwrap(),
        "feature/renamed"
    );
    assert_eq!(current_branch(&repo), "feature/renamed");
    assert!(
        core.list_local_branch_names(repo.path())
            .await
            .unwrap()
            .contains(&"feature/renamed".to_string())
    );

    // createRef can switch in the same call.
    core.create_ref(repo.path(), "feature/switched", true)
        .await
        .unwrap();
    assert_eq!(current_branch(&repo), "feature/switched");
}

#[tokio::test]
async fn rename_suffixes_collisions_unless_the_name_is_exact() {
    let repo = Repo::with_commit();
    let initial = current_branch(&repo);
    let core = live_core();
    core.create_ref(repo.path(), "Julius/ABC-123", false)
        .await
        .unwrap();

    let error = core
        .rename_branch(repo.path(), &initial, "Julius/ABC-123", true)
        .await
        .unwrap_err();
    assert_eq!(
        error.reason,
        Some(GitCommandFailureReason::BranchAlreadyExists)
    );
    assert_eq!(error.detail, "git branch rename failed");
    assert_eq!(current_branch(&repo), initial);

    assert_eq!(
        core.rename_branch(repo.path(), &initial, "Julius/ABC-123", false)
            .await
            .unwrap(),
        "Julius/ABC-123-1"
    );
    // Renaming a branch to its own name is a no-op.
    assert_eq!(
        core.rename_branch(repo.path(), "Julius/ABC-123-1", "Julius/ABC-123-1", false)
            .await
            .unwrap(),
        "Julius/ABC-123-1"
    );
}

#[tokio::test]
async fn switching_to_a_missing_branch_fails_without_touching_a_matching_dirty_file() {
    let repo = Repo::with_commit();
    let initial = current_branch(&repo);
    repo.write("obsolete-branch", "original\n");
    repo.git(&["add", "obsolete-branch"]);
    repo.git(&["commit", "-m", "tracked file"]);
    repo.write("obsolete-branch", "uncommitted work\n");

    assert!(
        live_core()
            .switch_ref(repo.path(), "obsolete-branch")
            .await
            .is_err()
    );
    assert_eq!(repo.read("obsolete-branch"), "uncommitted work\n");
    assert_eq!(current_branch(&repo), initial);
}

#[tokio::test]
async fn creates_and_reuses_remote_tracking_branches_and_allows_detached_refs() {
    let repo = Repo::with_commit();
    let remote = Repo::bare_dir();
    remote.git(&["init", "--bare"]);
    let initial = current_branch(&repo);
    repo.git(&["remote", "add", "origin", &remote.path().to_string_lossy()]);
    repo.git(&["push", "origin", "HEAD:refs/heads/remote-only"]);
    repo.git(&["fetch", "origin"]);
    let core = live_core();

    for _ in 0..2 {
        assert_eq!(
            core.switch_ref(repo.path(), "origin/remote-only")
                .await
                .unwrap()
                .as_deref(),
            Some("remote-only")
        );
        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
                .trim(),
            "origin/remote-only"
        );
        core.switch_ref(repo.path(), &initial).await.unwrap();
    }
    let commit = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(core.switch_ref(repo.path(), &commit).await.unwrap(), None);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]).trim(), commit);
}

#[tokio::test]
async fn reads_config_resolves_commits_and_deletes_branches() {
    let repo = Repo::with_commit();
    let core = live_core();
    assert_eq!(
        core.read_config_value(repo.path(), "user.name")
            .await
            .unwrap()
            .as_deref(),
        Some("Test")
    );
    assert_eq!(
        core.read_config_value(repo.path(), "no.such.key")
            .await
            .unwrap(),
        None
    );
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        core.resolve_commit(repo.path(), "HEAD").await.unwrap(),
        head
    );
    assert!(
        core.resolve_commit(repo.path(), "nonexistent-ref")
            .await
            .is_err()
    );

    core.create_ref(repo.path(), "temp", false).await.unwrap();
    core.delete_local_branch(repo.path(), "temp", false)
        .await
        .unwrap();
    assert!(!core.branch_exists(repo.path(), "temp").await.unwrap());
}

// ---------------------------------------------------------------------------------------------
// GitCore: worktrees
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn creates_and_removes_a_worktree_and_its_new_branch() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let core = live_core();
    let path = worktrees.join("feature-worktree");

    let created = core
        .create_worktree(
            &input(
                &repo,
                &initial,
                Some("feature/worktree"),
                Some(path.clone()),
            ),
            &options(&worktrees),
        )
        .await
        .unwrap();
    assert_eq!(created.path, path);
    assert_eq!(created.ref_name, "feature/worktree");
    assert_eq!(
        common::run_git(&path, &["branch", "--show-current"]).trim(),
        "feature/worktree"
    );
    let listed = core.list_worktree_paths(repo.path()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|listed| std::fs::canonicalize(listed).unwrap()
                == std::fs::canonicalize(&path).unwrap())
    );

    core.remove_worktree(repo.path(), &path, false)
        .await
        .unwrap();
    core.delete_local_branch(repo.path(), "feature/worktree", true)
        .await
        .unwrap();
    assert!(!path.exists());
    assert!(
        !core
            .list_local_branch_names(repo.path())
            .await
            .unwrap()
            .contains(&"feature/worktree".to_string())
    );
}

#[tokio::test]
async fn the_default_worktree_path_is_repo_name_then_sanitized_branch() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let created = live_core()
        .create_worktree(
            &input(&repo, &initial, Some("feature/a-b"), None),
            &options(&worktrees),
        )
        .await
        .unwrap();
    let repo_name = repo.path().file_name().unwrap();
    assert_eq!(
        created.path,
        worktrees.path().join(repo_name).join("feature-a-b")
    );
    assert!(created.path.join("README.md").exists());
}

#[tokio::test]
async fn a_new_branch_worktree_records_its_merge_base() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    repo.git(&["remote", "add", "origin", "https://example.com/x.git"]);
    let mut request = input(
        &repo,
        &initial,
        Some("feature/base"),
        Some(worktrees.join("wt")),
    );
    request.base_ref_name = Some(format!("origin/{initial}"));
    live_core()
        .create_worktree(&request, &options(&worktrees))
        .await
        .unwrap();
    assert_eq!(
        repo.git(&["config", "--get", "branch.feature/base.gh-merge-base"])
            .trim(),
        initial
    );
}

#[tokio::test]
async fn worktree_creation_names_the_branch_conflict_without_quoting_stderr() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let error = live_core()
        .create_worktree(
            &input(&repo, &initial, None, Some(worktrees.join("conflict"))),
            &options(&worktrees),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.reason,
        Some(GitCommandFailureReason::BranchCheckedOutInWorktree)
    );
    assert_eq!(error.detail, "git worktree add failed");
    assert_eq!(error.command, "git");
    assert!(
        error
            .to_string()
            .ends_with("git worktree add failed (branch_checked_out_in_worktree)")
    );
    assert!(
        !error
            .to_string()
            .contains(&worktrees.path().to_string_lossy().into_owned())
    );
    assert!(error.stderr_length.unwrap() > 0);
}

#[tokio::test]
async fn worktree_creation_populates_submodules_when_asked() {
    let sub = Repo::with_commit();
    let repo = Repo::with_commit();
    // Local file transport is blocked by default for submodules.
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        &sub.path().to_string_lossy(),
        "vendor/sub",
    ]);
    repo.git(&["commit", "-m", "add submodule"]);
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);

    // Git refuses `file:` submodule transports by default (CVE-2022-39253) and ignores
    // repository config for it, so a local fixture needs the environment's allowance. Real
    // submodules are https/ssh and need none of this.
    struct AllowFileProtocol;
    impl t3_vcs::process::ProcessRunner for AllowFileProtocol {
        fn run(
            &self,
            mut input: t3_vcs::process::ProcessRunInput,
        ) -> futures_util::future::BoxFuture<'_, common::RunResult> {
            input
                .env
                .insert(0, ("GIT_ALLOW_PROTOCOL".into(), Some("file".into())));
            Live.run(input)
        }
    }
    let core = GitCore::new(Arc::new(AllowFileProtocol));
    let mut with_submodules = options(&worktrees);
    with_submodules.submodules = WorktreeSubmodules::TopLevel;
    core.create_worktree(
        &input(
            &repo,
            &initial,
            Some("with-sub"),
            Some(worktrees.join("with")),
        ),
        &with_submodules,
    )
    .await
    .unwrap();
    assert!(worktrees.join("with/vendor/sub/README.md").exists());

    core.create_worktree(
        &input(
            &repo,
            &initial,
            Some("without-sub"),
            Some(worktrees.join("without")),
        ),
        &options(&worktrees),
    )
    .await
    .unwrap();
    assert!(!worktrees.join("without/vendor/sub/README.md").exists());
}

#[tokio::test]
async fn removing_a_worktree_twice_is_a_no_op() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let core = live_core();
    let path = worktrees.join("shared");
    core.create_worktree(
        &input(&repo, &initial, Some("feature/shared"), Some(path.clone())),
        &options(&worktrees),
    )
    .await
    .unwrap();
    core.remove_worktree(repo.path(), &path, false)
        .await
        .unwrap();
    core.remove_worktree(repo.path(), &path, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn removing_an_already_gone_worktree_prunes_stale_registrations() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let core = live_core();
    let stale = worktrees.join("stale");
    core.create_worktree(
        &input(&repo, &initial, Some("feature/stale"), Some(stale.clone())),
        &options(&worktrees),
    )
    .await
    .unwrap();
    // Deleted behind git's back, so the registration goes stale.
    std::fs::remove_dir_all(&stale).unwrap();

    core.remove_worktree(repo.path(), &worktrees.join("never-registered"), false)
        .await
        .unwrap();
    assert!(
        !repo
            .git(&["worktree", "list", "--porcelain"])
            .contains("stale")
    );
    assert_eq!(
        core.list_worktree_paths(repo.path()).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn removal_refuses_untracked_files_even_when_status_hides_them_unless_forced() {
    let repo = Repo::with_commit();
    let worktrees = Repo::bare_dir();
    let initial = current_branch(&repo);
    let core = live_core();
    let path = worktrees.join("dirty");
    core.create_worktree(
        &input(&repo, &initial, Some("feature/dirty"), Some(path.clone())),
        &options(&worktrees),
    )
    .await
    .unwrap();
    std::fs::write(path.join("untracked.txt"), "keep me\n").unwrap();
    repo.git(&["config", "status.showUntrackedFiles", "no"]);

    let error = core
        .remove_worktree(repo.path(), &path, false)
        .await
        .unwrap_err();
    assert_eq!(error.detail, "git worktree remove failed");
    assert!(path.join("untracked.txt").exists());

    core.remove_worktree(repo.path(), &path, true)
        .await
        .unwrap();
    assert!(!path.exists());
}

// ---------------------------------------------------------------------------------------------
// GitCore: errors and status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_cwd_is_a_spawn_failure_with_context_and_a_cause() {
    let core = live_core();
    let missing = Repo::bare_dir().path().join("gone");
    let error = core
        .read_config_value(&missing, "user.name")
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.readConfigValue");
    assert_eq!(error.detail, "Failed to spawn Git process.");
    assert_eq!(error.command, "git");
    assert!(error.cause.is_some());
    assert!(std::error::Error::source(&error).is_some());
}

#[tokio::test]
async fn failures_name_a_missing_repository_without_retaining_stderr() {
    let dir = Repo::bare_dir();
    let error = live_core()
        .list_local_branch_names(dir.path())
        .await
        .unwrap_err();
    assert_eq!(error.reason, Some(GitCommandFailureReason::NotARepository));
    assert_eq!(error.detail, "Git command exited with a non-zero status.");
    assert_eq!(error.exit_code, Some(128));
    assert!(error.stderr_length.unwrap() > 0);
    assert_eq!(error.stdout_length, Some(0));
    assert!(!error.to_string().contains("not a git repository"));
}

#[tokio::test]
async fn reports_non_repository_directories_without_failing() {
    let core = live_core();
    let outside = Repo::bare_dir();
    let status = core
        .status_details_local(outside.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(!status.is_repo);
    assert_eq!(status, Default::default());
    // A directory that does not exist is recovered the same way.
    let gone = outside.path().join("gone");
    assert!(
        !core
            .status_details_local(&gone, LocalStatusOptions::default())
            .await
            .unwrap()
            .is_repo
    );
}

#[tokio::test]
async fn reports_branch_and_dirty_state_for_a_repository() {
    let repo = Repo::with_commit();
    let initial = current_branch(&repo);
    repo.write("feature.ts", "export const value = 1;\n");
    let status = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(status.is_repo);
    assert_eq!(status.branch.as_deref(), Some(initial.as_str()));
    assert!(status.has_working_tree_changes);
    assert!(
        status
            .working_tree
            .files
            .iter()
            .any(|file| file.path == "feature.ts")
    );
    assert!(!status.has_upstream);
}

#[tokio::test]
async fn reports_changes_to_a_file_named_head() {
    let repo = Repo::with_commit();
    repo.write("HEAD", "first line\n");
    repo.git(&["add", "HEAD"]);
    repo.git(&["commit", "-m", "add HEAD file"]);
    repo.write("HEAD", "first line\nsecond line\n");
    let status = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(
        status
            .working_tree
            .files
            .iter()
            .any(|file| file.path == "HEAD" && file.insertions == 1 && file.deletions == 0)
    );
}

#[tokio::test]
async fn reports_combined_staged_and_unstaged_edits_to_the_same_file() {
    let repo = Repo::with_commit();
    repo.write("feature.ts", "// line one\n");
    repo.git(&["add", "feature.ts"]);
    repo.git(&["commit", "-m", "add feature"]);
    repo.write("feature.ts", "// line one\n// line two\n");
    repo.git(&["add", "feature.ts"]);
    repo.write("feature.ts", "// line one\n// line two\n// line three\n");
    let status = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    let file = status
        .working_tree
        .files
        .iter()
        .find(|file| file.path == "feature.ts")
        .unwrap();
    // Net change from HEAD, not the sum of the staged and unstaged halves.
    assert_eq!((file.insertions, file.deletions), (2, 0));
    assert_eq!(
        (
            status.working_tree.insertions,
            status.working_tree.deletions
        ),
        (2, 0)
    );
}

#[tokio::test]
async fn reports_staged_file_counts_on_unborn_head() {
    let repo = Repo::init();
    repo.write("initial.ts", "// first file\n");
    repo.git(&["add", "initial.ts"]);
    let status = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(status.is_repo);
    assert_eq!(status.working_tree.files.len(), 1);
    assert_eq!(status.working_tree.files[0].path, "initial.ts");
    assert_eq!(status.working_tree.files[0].insertions, 1);
}

#[tokio::test]
async fn reports_default_branch_delta_separately_from_upstream_delta() {
    let repo = Repo::with_commit();
    let remote = Repo::bare_dir();
    remote.git(&["init", "--bare"]);
    let initial = current_branch(&repo);
    repo.git(&["remote", "add", "origin", &remote.path().to_string_lossy()]);
    repo.git(&["push", "-u", "origin", &initial]);
    repo.git(&["checkout", "-b", "feature/synced"]);
    repo.write("feature.txt", "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "feature commit"]);
    repo.git(&["push", "-u", "origin", "feature/synced"]);

    let status = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(status.has_upstream);
    assert_eq!(
        status.upstream_ref.as_deref(),
        Some("origin/feature/synced")
    );
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
    assert_eq!(status.ahead_of_default_count, 1);
    assert!(status.has_origin_remote);
    assert!(!status.is_default_branch);

    // Without divergence, no revision walk happens and every count is zero.
    let local = live_core()
        .status_details_local(
            repo.path(),
            LocalStatusOptions {
                include_divergence: false,
                ..LocalStatusOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (
            local.ahead_count,
            local.behind_count,
            local.ahead_of_default_count
        ),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn a_branch_without_an_upstream_counts_commits_against_the_base_branch() {
    let repo = Repo::with_commit();
    let initial = current_branch(&repo);
    repo.git(&["checkout", "-b", "topic"]);
    for name in ["one", "two"] {
        repo.write(name, "x\n");
        repo.git(&["add", name]);
        repo.git(&["commit", "-m", name]);
    }
    let core = live_core();
    let status = core
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert!(!status.has_upstream);
    // The default branch is only main/master by name when origin/HEAD is unknown.
    if initial == "main" || initial == "master" {
        assert_eq!((status.ahead_count, status.ahead_of_default_count), (2, 2));
    }
    repo.git(&["config", "branch.topic.gh-merge-base", &initial]);
    let status = core
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert_eq!(
        (
            status.ahead_count,
            status.behind_count,
            status.ahead_of_default_count
        ),
        (2, 0, 2)
    );
    assert!(!status.is_default_branch);
}

#[tokio::test]
async fn status_refuses_while_the_index_is_locked() {
    let repo = Repo::with_commit();
    repo.write(".git/index.lock", "");
    let error = live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.detail,
        "Git index is locked. Status will resume when the index lock is removed."
    );
    assert_eq!(error.operation, "GitVcsDriver.statusDetails.indexPath");
    repo.remove(".git/index.lock");
    assert!(
        live_core()
            .status_details_local(repo.path(), LocalStatusOptions::default())
            .await
            .unwrap()
            .is_repo
    );
}

#[tokio::test]
async fn status_maps_to_the_wire_shape() {
    let repo = Repo::with_commit();
    repo.write("a.txt", "x\n");
    let status = live_core().status(repo.path()).await.unwrap();
    assert!(status.is_repo && status.has_working_tree_changes);
    assert_eq!(
        status.ref_name.as_deref(),
        Some(current_branch(&repo).as_str())
    );
}

#[tokio::test]
async fn status_does_not_take_the_index_lock_or_rewrite_the_index() {
    let repo = Repo::with_commit();
    repo.write("a.txt", "x\n");
    let before = repo.index_bytes();
    live_core()
        .status_details_local(repo.path(), LocalStatusOptions::default())
        .await
        .unwrap();
    assert_eq!(repo.index_bytes(), before);
}

#[tokio::test]
async fn execute_runs_git_with_optional_locks_disabled_and_env_overrides() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    struct Spy(Arc<std::sync::Mutex<Option<t3_vcs::process::ProcessRunInput>>>);
    impl t3_vcs::process::ProcessRunner for Spy {
        fn run(
            &self,
            input: t3_vcs::process::ProcessRunInput,
        ) -> futures_util::future::BoxFuture<'_, common::RunResult> {
            *self.0.lock().unwrap() = Some(input.clone());
            Live.run(input)
        }
    }
    let core = GitCore::new(Arc::new(Spy(seen.clone())));
    let repo = Repo::with_commit();
    core.read_config_value(repo.path(), "user.name")
        .await
        .unwrap();
    let input = seen.lock().unwrap().take().unwrap();
    assert_eq!(
        input.env[0],
        ("GIT_OPTIONAL_LOCKS".to_string(), Some("0".to_string()))
    );
    assert_eq!(input.args, ["config", "--get", "user.name"]);
    assert_eq!(input.spawn_cwd.as_deref(), Some(repo.path()));
    let _ = LiveRunner;
}
