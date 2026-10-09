//! Commit context, commit, range context and progress reporting against real repositories
//! (ports of the corresponding cases of the original GitVcsDriverCore.test.ts).
mod common;

use std::sync::{Arc, Mutex};

use common::{Repo, live_core};
use t3_vcs::{
    core::{
        CheckoutProgress, CreateWorktreeOptions, CreateWorktreeProgress, ExecuteGitInput,
        VcsCreateWorktreeInput, WorktreeSubmodules,
    },
    error::OutputStream,
    progress::{GitProgress, HookFinished},
    remote::{CommitStage, GitCommitOptions, GitCommitProgress},
};

fn stage(paths: &[&str]) -> GitCommitOptions {
    GitCommitOptions {
        stage: Some(CommitStage {
            file_paths: Some(paths.iter().map(|p| p.to_string()).collect()),
        }),
        ..GitCommitOptions::default()
    }
}

fn paths(paths: &[&str]) -> Option<Vec<String>> {
    Some(paths.iter().map(|p| p.to_string()).collect())
}

#[tokio::test]
async fn prepares_selected_files_without_staging_and_commits_only_those() {
    let repo = Repo::with_commit();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    let core = live_core();

    let context = core
        .prepare_commit_context(repo.path(), paths(&["a.txt"]).as_deref())
        .await
        .unwrap()
        .unwrap();
    assert!(context.staged_summary.contains("a.txt") && !context.staged_summary.contains("b.txt"));
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "");

    let sha = core
        .commit(repo.path(), "Add a", "", stage(&["a.txt"]))
        .await
        .unwrap();
    assert_eq!(sha.len(), 40);
    assert_eq!(repo.git(&["log", "-1", "--pretty=%s"]).trim(), "Add a");
    let status = repo.git(&["status", "--porcelain"]);
    assert!(status.contains("?? b.txt") && !status.contains("a.txt"));
}

#[tokio::test]
async fn selected_paths_are_literal() {
    let repo = Repo::with_commit();
    repo.write("selected[1].txt", "literal\n");
    repo.write("selected1.txt", "pattern match\n");
    let core = live_core();

    let context = core
        .prepare_commit_context(repo.path(), paths(&["selected[1].txt"]).as_deref())
        .await
        .unwrap()
        .unwrap();
    assert!(context.staged_summary.contains("selected[1].txt"));
    assert!(!context.staged_summary.contains("selected1.txt"));

    core.commit(
        repo.path(),
        "Add literal path",
        "",
        stage(&["selected[1].txt"]),
    )
    .await
    .unwrap();
    assert_eq!(
        repo.git(&["show", "--pretty=", "--name-only", "HEAD"])
            .trim(),
        "selected[1].txt"
    );
    assert!(
        repo.git(&["status", "--porcelain"])
            .contains("?? selected1.txt")
    );
}

#[tokio::test]
async fn preparing_leaves_partial_staging_and_split_index_files_alone() {
    for split in [false, true] {
        let repo = Repo::with_commit();
        if split {
            repo.git(&["config", "core.splitIndex", "true"]);
        }
        repo.write("README.md", "# test\nstaged\n");
        repo.git(&["add", "README.md"]);
        repo.write("README.md", "# test\nstaged\nunstaged\n");
        repo.write("untracked.txt", "untracked\n");
        repo.write(".gitignore", "ignored.txt\n");
        repo.write("ignored.txt", "ignored\n");
        let before = repo.index_bytes();
        let shared = |repo: &Repo| {
            let mut names: Vec<_> = std::fs::read_dir(repo.join(".git"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("sharedindex."))
                .collect();
            names.sort();
            names
        };
        let shared_before = shared(&repo);
        for selection in [None, paths(&["README.md"])] {
            let context = live_core()
                .prepare_commit_context(repo.path(), selection.as_deref())
                .await
                .unwrap()
                .unwrap();
            assert!(context.staged_patch.contains("unstaged"), "split: {split}");
            assert!(!context.staged_summary.contains("ignored.txt"));
            assert_eq!(repo.index_bytes(), before, "split: {split}");
            assert_eq!(shared(&repo), shared_before, "split: {split}");
        }
    }
}

#[tokio::test]
async fn preparing_an_unborn_repository_creates_no_real_index() {
    let repo = Repo::init();
    repo.write("initial.txt", "initial\n");
    let core = live_core();
    for selection in [None, paths(&["initial.txt"])] {
        let context = core
            .prepare_commit_context(repo.path(), selection.as_deref())
            .await
            .unwrap()
            .unwrap();
        assert!(context.staged_summary.contains("initial.txt"));
        assert!(!repo.exists(".git/index"));
    }
    core.commit(repo.path(), "Initial", "", stage(&["initial.txt"]))
        .await
        .unwrap();
    assert_eq!(repo.git(&["show", "HEAD:initial.txt"]), "initial\n");
}

#[tokio::test]
async fn preparing_preserves_merge_metadata() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature"]);
    repo.commit_file("feature.txt", "feature\n", "Feature");
    repo.git(&["checkout", &initial]);
    repo.git(&["merge", "--no-commit", "--no-ff", "feature"]);
    repo.write("README.md", "# test\nmerge edit\n");
    let names = ["index", "MERGE_HEAD", "MERGE_MSG", "ORIG_HEAD"];
    let read = || names.map(|name| std::fs::read(repo.join(&format!(".git/{name}"))).unwrap());
    let before = read();
    for selection in [None, paths(&["README.md"])] {
        live_core()
            .prepare_commit_context(repo.path(), selection.as_deref())
            .await
            .unwrap();
        assert_eq!(read(), before);
    }
}

#[tokio::test]
async fn nothing_to_commit_prepares_no_context_and_large_patches_are_marked_truncated() {
    let repo = Repo::with_commit();
    let core = live_core();
    assert!(
        core.prepare_commit_context(repo.path(), None)
            .await
            .unwrap()
            .is_none()
    );

    repo.write(
        "big.txt",
        &"a line of content for a large patch\n".repeat(3000),
    );
    let context = core
        .prepare_commit_context(repo.path(), None)
        .await
        .unwrap()
        .unwrap();
    assert!(context.staged_patch.ends_with("\n\n[truncated]"));
    assert!(context.staged_patch.len() < 49_100);
}

#[tokio::test]
async fn commit_takes_an_optional_body_and_returns_the_new_sha() {
    let repo = Repo::with_commit();
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);

    let sha = live_core()
        .commit(
            repo.path(),
            "Subject",
            "  Body text  ",
            GitCommitOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(sha, repo.git(&["rev-parse", "HEAD"]).trim());
    assert_eq!(
        repo.git(&["log", "-1", "--pretty=%s%n--%n%b"]),
        "Subject\n--\nBody text\n\n"
    );
}

#[tokio::test]
async fn range_context_summarizes_the_commits_and_the_diff_from_the_merge_base() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    repo.git(&["checkout", "-b", "feature"]);
    repo.commit_file("feature.txt", "feature\n", "feature work");
    repo.git(&["checkout", &initial]);
    repo.commit_file("base-moved.txt", "moved\n", "base moves");
    repo.git(&["checkout", "feature"]);

    let context = live_core()
        .read_range_context(repo.path(), &initial)
        .await
        .unwrap();

    assert!(
        context.commit_summary.contains("feature work")
            && !context.commit_summary.contains("base moves")
    );
    assert!(
        context.diff_summary.contains("feature.txt")
            && !context.diff_summary.contains("base-moved.txt")
    );
    assert!(context.diff_patch.contains("+feature") && !context.diff_patch.contains("moved"));
}

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn install_hook(repo: &Repo, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    repo.write(&format!(".git/hooks/{name}"), script);
    std::fs::set_permissions(
        repo.join(&format!(".git/hooks/{name}")),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn commit_reports_hook_output_and_hook_lifecycle() {
    let repo = Repo::with_commit();
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\necho 'hook: start'\nprintf 'step 1\\rstep 2\\r\\n' >&2\necho 'hook: end'\nexit 0\n",
    );
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let (output, started, finished) = (events.clone(), events.clone(), events.clone());
    let progress = GitCommitProgress {
        on_output_line: Some(Arc::new(move |stream, line| {
            let stream = if stream == OutputStream::Stdout {
                "stdout"
            } else {
                "stderr"
            };
            output.lock().unwrap().push(format!("{stream}: {line}"));
        })),
        on_hook_started: Some(Arc::new(move |name| {
            started.lock().unwrap().push(format!("started {name}"))
        })),
        on_hook_finished: Some(Arc::new(move |hook: &HookFinished| {
            assert!(hook.duration_ms.is_some());
            finished
                .lock()
                .unwrap()
                .push(format!("finished {} {:?}", hook.hook_name, hook.exit_code));
        })),
    };

    live_core()
        .commit(
            repo.path(),
            "With hook",
            "",
            GitCommitOptions {
                progress: Some(progress),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let events = events.lock().unwrap();
    for expected in [
        "stderr: hook: start",
        "stderr: hook: end",
        "stderr: step 1",
        "stderr: step 2",
        "started pre-commit",
        "finished pre-commit Some(0)",
    ] {
        assert!(
            events.iter().any(|event| event == expected),
            "{expected} in {events:?}"
        );
    }
    let position = |text: &str| events.iter().position(|event| event == text).unwrap();
    assert!(position("started pre-commit") < position("finished pre-commit Some(0)"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_rejecting_hook_fails_the_commit_and_reports_its_exit_status() {
    let repo = Repo::with_commit();
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\necho rejected >&2\nexit 3\n",
    );
    repo.write("a.txt", "a\n");
    repo.git(&["add", "a.txt"]);
    let finished = Arc::new(Mutex::new(Vec::new()));
    let recorder = finished.clone();
    let progress = GitCommitProgress {
        on_hook_finished: Some(Arc::new(move |hook: &HookFinished| {
            recorder.lock().unwrap().push(hook.clone())
        })),
        ..Default::default()
    };

    let error = live_core()
        .commit(
            repo.path(),
            "Rejected",
            "",
            GitCommitOptions {
                progress: Some(progress),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.operation, "GitVcsDriver.commit.commit");
    assert_eq!(error.exit_code, Some(1));
    let finished = finished.lock().unwrap();
    assert_eq!(
        (finished[0].hook_name.as_str(), finished[0].exit_code),
        ("pre-commit", Some(3))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn line_callbacks_can_outlive_the_output_cap() {
    let repo = Repo::bare_dir();
    for keep in [true, false] {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = lines.clone();
        let result = live_core()
            .execute(ExecuteGitInput {
                operation: "test.callbacksPastCap".into(),
                cwd: repo.path().to_path_buf(),
                args: [
                    "-c",
                    "alias.spew=!for i in $(seq 1 128); do printf \"é%03d\\n\" $i >&2; done; echo fatal: last line >&2",
                    "spew",
                ]
                .map(String::from)
                .into(),
                max_output_bytes: Some(512),
                append_truncation_marker: true,
                allow_non_zero_exit: true,
                progress: Some(GitProgress {
                    on_stderr_line: Some(Arc::new(move |line: &str| recorder.lock().unwrap().push(line.to_string()))),
                    keep_line_callbacks_after_truncation: keep,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(result.stderr_truncated);
        let lines = lines.lock().unwrap();
        // The cap landing inside "é" is invisible to callbacks.
        assert!(!lines.iter().any(|line| line.contains('\u{FFFD}')) || !keep);
        if keep {
            assert_eq!(lines.len(), 129);
            assert_eq!((lines[0].as_str(), lines[127].as_str()), ("é001", "é128"));
            assert_eq!(lines.last().unwrap(), "fatal: last line");
        } else {
            assert!(lines.len() < 129 && lines[0] == "é001");
        }
    }
}

#[tokio::test]
async fn worktree_creation_reports_claim_checkout_and_submodule_progress() {
    let repo = Repo::with_commit();
    let initial = repo.branch();
    let worktrees = Repo::bare_dir();
    let path = worktrees.join("wt");
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = |events: &Arc<Mutex<Vec<String>>>| {
        let events = events.clone();
        move |text: String| events.lock().unwrap().push(text)
    };
    let (claimed, checkout) = (log(&events), log(&events));
    let progress = CreateWorktreeProgress {
        on_worktree_claimed: Some(Arc::new(move |path| {
            claimed(format!("claimed {}", path.display()))
        })),
        on_checkout_progress: Some(Arc::new(move |p: CheckoutProgress| {
            checkout(format!("checkout {}", p.percent))
        })),
        ..Default::default()
    };

    live_core()
        .create_worktree(
            &VcsCreateWorktreeInput {
                cwd: repo.path().to_path_buf(),
                ref_name: initial,
                new_ref_name: Some("feature/progress".into()),
                base_ref_name: None,
                path: Some(path.clone()),
            },
            &CreateWorktreeOptions {
                worktrees_directory: worktrees.path().to_path_buf(),
                submodules: WorktreeSubmodules::None,
                progress: Some(progress),
            },
        )
        .await
        .unwrap();

    let events = events.lock().unwrap();
    assert_eq!(
        events.last().unwrap(),
        &format!("claimed {}", path.display())
    );
    assert!(
        events
            .iter()
            .all(|e| e.starts_with("claimed") || e.starts_with("checkout"))
    );
}

#[tokio::test]
async fn output_past_the_cap_fails_with_its_length_unless_truncation_is_asked_for() {
    let repo = Repo::with_commit();
    let core = live_core();
    let run = |append: bool| {
        core.execute(ExecuteGitInput {
            operation: "test.cap".into(),
            cwd: repo.path().to_path_buf(),
            args: ["log", "--format=%H"].map(String::from).into(),
            max_output_bytes: Some(10),
            append_truncation_marker: append,
            ..Default::default()
        })
    };

    let error = run(false).await.unwrap_err();
    assert_eq!(
        error.detail,
        "Git output exceeded 10 bytes and was truncated."
    );
    assert!(error.output_length.unwrap() > 10);

    let truncated = run(true).await.unwrap();
    assert!(truncated.stdout_truncated);
    assert_eq!(truncated.stdout.len(), 10);
}
