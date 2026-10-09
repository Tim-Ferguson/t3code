//! Ports of the original CheckpointStore.test.ts, run against a real git binary.
mod common;

use common::{Repo, live_git};
use t3_vcs::{
    CheckpointStore, DiffCheckpointsInput, DiffFormat, TurnDiffFileSummary,
    checkpoint_ref_for_thread_turn, parse_turn_diff_files_from_numstat,
};

fn store() -> CheckpointStore {
    CheckpointStore::new(live_git())
}

fn diff_input(
    repo: &Repo,
    from: &str,
    to: &str,
    ignore_whitespace: bool,
    format: DiffFormat,
) -> DiffCheckpointsInput {
    DiffCheckpointsInput {
        cwd: repo.path().to_path_buf(),
        from_checkpoint_ref: from.into(),
        to_checkpoint_ref: to.into(),
        fallback_from_to_head: false,
        ignore_whitespace,
        format,
    }
}

fn large_text(line_count: usize) -> String {
    (0..line_count)
        .map(|index| format!("line {index:05}\n"))
        .collect()
}

fn summary(path: &str, additions: u64, deletions: u64) -> TurnDiffFileSummary {
    TurnDiffFileSummary {
        path: path.into(),
        additions,
        deletions,
    }
}

#[tokio::test]
async fn is_git_repository_is_false_outside_a_repository_and_true_inside_one() {
    let store = store();
    assert!(
        !store
            .is_git_repository(Repo::bare_dir().path())
            .await
            .unwrap()
    );
    let repo = Repo::with_commit();
    assert!(store.is_git_repository(repo.path()).await.unwrap());
    std::fs::create_dir_all(repo.join("packages/nested")).unwrap();
    assert!(
        store
            .is_git_repository(&repo.join("packages/nested"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn operations_outside_a_repository_fail_with_the_registry_error() {
    let dir = Repo::bare_dir();
    let error = store()
        .capture_checkpoint(dir.path(), "refs/t3/checkpoints/x")
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "VcsUnsupportedOperationError");
    assert_eq!(
        error.to_string(),
        format!(
            "VCS operation is unsupported for unknown in CheckpointStore.captureCheckpoint: No supported VCS repository was detected at {}.",
            dir.path().display()
        )
    );
}

#[tokio::test]
async fn returns_full_oversized_checkpoint_diffs_without_truncation() {
    let repo = Repo::with_commit();
    let store = store();
    let from = checkpoint_ref_for_thread_turn("thread-checkpoint-store", 0);
    let to = checkpoint_ref_for_thread_turn("thread-checkpoint-store", 1);
    store.capture_checkpoint(repo.path(), &from).await.unwrap();
    repo.write("README.md", &large_text(5_000));
    store.capture_checkpoint(repo.path(), &to).await.unwrap();

    let diff = store
        .diff_checkpoints(&diff_input(&repo, &from, &to, true, DiffFormat::Patch))
        .await
        .unwrap();
    assert!(diff.contains("diff --git"));
    assert!(!diff.contains("[truncated]"));
    assert!(diff.contains("+line 04999"));
}

#[tokio::test]
async fn keeps_a_and_b_patch_prefixes_when_the_repository_disables_them() {
    let repo = Repo::with_commit();
    repo.git(&["config", "diff.noprefix", "true"]);
    let store = store();
    let from = checkpoint_ref_for_thread_turn("thread-checkpoint-store-noprefix", 0);
    let to = checkpoint_ref_for_thread_turn("thread-checkpoint-store-noprefix", 1);
    store.capture_checkpoint(repo.path(), &from).await.unwrap();
    repo.write("README.md", "# changed\n");
    store.capture_checkpoint(repo.path(), &to).await.unwrap();

    let diff = store
        .diff_checkpoints(&diff_input(&repo, &from, &to, false, DiffFormat::Patch))
        .await
        .unwrap();
    assert!(diff.contains("diff --git a/README.md b/README.md"));
}

#[tokio::test]
async fn can_hide_indentation_churn_when_changes_wrap_existing_lines() {
    let repo = Repo::with_commit();
    let store = store();
    let from = checkpoint_ref_for_thread_turn("thread-checkpoint-store-whitespace", 0);
    let to = checkpoint_ref_for_thread_turn("thread-checkpoint-store-whitespace", 1);
    repo.write(
        "Component.tsx",
        "export function View() {\n  return (\n    <section>\n      <h1>Title</h1>\n      <p>Body</p>\n    </section>\n  );\n}\n",
    );
    store.capture_checkpoint(repo.path(), &from).await.unwrap();
    repo.write(
        "Component.tsx",
        "export function View() {\n  return (\n    <section>\n      {isReady ? (\n        <div>\n          <h1>Title</h1>\n          <p>Body</p>\n        </div>\n      ) : null}\n    </section>\n  );\n}\n",
    );
    store.capture_checkpoint(repo.path(), &to).await.unwrap();

    let normal = store
        .diff_checkpoints(&diff_input(&repo, &from, &to, false, DiffFormat::Patch))
        .await
        .unwrap();
    let ignored = store
        .diff_checkpoints(&diff_input(&repo, &from, &to, true, DiffFormat::Patch))
        .await
        .unwrap();
    assert!(normal.contains("diff --git"));
    assert!(normal.contains("-      <h1>Title</h1>"));
    assert!(normal.contains("+          <h1>Title</h1>"));
    assert!(ignored.contains("+      {isReady ? ("));
    assert!(ignored.contains("+        <div>"));
    assert!(!ignored.contains("-      <h1>Title</h1>"));
    assert!(!ignored.contains("+          <h1>Title</h1>"));

    for ignore_whitespace in [false, true] {
        let numstat = store
            .diff_checkpoints(&diff_input(
                &repo,
                &from,
                &to,
                ignore_whitespace,
                DiffFormat::Numstat,
            ))
            .await
            .unwrap();
        assert_eq!(
            parse_turn_diff_files_from_numstat(&numstat),
            [summary(
                "Component.tsx",
                if ignore_whitespace { 4 } else { 6 },
                if ignore_whitespace { 0 } else { 2 }
            )]
        );
    }
}

#[tokio::test]
async fn counts_changes_whose_full_patch_exceeds_the_output_limit() {
    let repo = Repo::with_commit();
    let store = store();
    let from = checkpoint_ref_for_thread_turn("large-checkpoint-summary", 0);
    let to = checkpoint_ref_for_thread_turn("large-checkpoint-summary", 1);
    let line_count = 20_000;
    repo.write(
        "README.md",
        &format!("{}\n", "before".repeat(50)).repeat(line_count),
    );
    store.capture_checkpoint(repo.path(), &from).await.unwrap();
    repo.write(
        "README.md",
        &format!("{}\n", "after".repeat(60)).repeat(line_count),
    );
    store.capture_checkpoint(repo.path(), &to).await.unwrap();

    let numstat = store
        .diff_checkpoints(&diff_input(&repo, &from, &to, false, DiffFormat::Numstat))
        .await
        .unwrap();
    assert_eq!(
        parse_turn_diff_files_from_numstat(&numstat),
        [summary("README.md", line_count as u64, line_count as u64)]
    );
    assert!(numstat.len() < 100);
}

#[tokio::test]
async fn preserves_file_paths_and_turn_ranges_without_changing_the_user_index() {
    let repo = Repo::with_commit();
    repo.git(&["config", "diff.renames", "copies"]);
    let store = store();
    let baseline = checkpoint_ref_for_thread_turn("checkpoint-summary-paths", 0);
    let first_turn = checkpoint_ref_for_thread_turn("checkpoint-summary-paths", 1);
    let second_turn = checkpoint_ref_for_thread_turn("checkpoint-summary-paths", 2);
    let copied_text: String = (0..20)
        .map(|index| format!("copy line {index}\n"))
        .collect();
    let renamed_path = "renamed\tcafé\nname.txt";
    let added_path = "new\tfile\n名.txt";
    for (path, contents) in [
        ("copy-source.txt", copied_text.as_str()),
        ("deleted.txt", "delete me\n"),
        ("rename-old.txt", "before\nkeep one\nkeep two\nkeep three\n"),
        ("binary.bin", "\0before"),
    ] {
        repo.write(path, contents);
    }
    store
        .capture_checkpoint(repo.path(), &baseline)
        .await
        .unwrap();

    std::fs::rename(repo.join("rename-old.txt"), repo.join(renamed_path)).unwrap();
    repo.remove("deleted.txt");
    let copy_source = format!("{copied_text}one more\n");
    for (path, contents) in [
        ("copy-source.txt", copy_source.as_str()),
        ("copied.txt", copied_text.as_str()),
        (renamed_path, "after\nkeep one\nkeep two\nkeep three\n"),
        ("binary.bin", "\0after"),
        ("empty.txt", ""),
        (added_path, "first\nsecond\n"),
    ] {
        repo.write(path, contents);
    }
    store
        .capture_checkpoint(repo.path(), &first_turn)
        .await
        .unwrap();
    let user_index = repo.index_bytes();

    let numstat = |from: &str, to: &str| {
        let input = diff_input(&repo, from, to, false, DiffFormat::Numstat);
        let store = store.clone();
        async move { parse_turn_diff_files_from_numstat(&store.diff_checkpoints(&input).await.unwrap()) }
    };
    let mut expected_files = vec![
        summary("binary.bin", 0, 0),
        summary("copied.txt", 0, 0),
        summary("copy-source.txt", 1, 0),
        summary("deleted.txt", 0, 1),
        summary("empty.txt", 0, 0),
        summary(added_path, 2, 0),
        summary(renamed_path, 1, 1),
    ];
    let mut actual = numstat(&baseline, &first_turn).await;
    let key = |file: &TurnDiffFileSummary| file.path.clone();
    expected_files.sort_by_key(key);
    actual.sort_by_key(key);
    assert_eq!(actual, expected_files);

    repo.remove("empty.txt");
    repo.write("copy-source.txt", "replacement\n");
    store
        .capture_checkpoint(repo.path(), &second_turn)
        .await
        .unwrap();
    assert_eq!(
        numstat(&first_turn, &second_turn).await,
        [
            summary("copy-source.txt", 1, 21),
            summary("empty.txt", 0, 0)
        ]
    );

    let mut inclusive = numstat(&baseline, &second_turn).await;
    inclusive.sort_by_key(key);
    let mut expected_inclusive: Vec<_> = expected_files
        .into_iter()
        .filter(|file| file.path != "empty.txt")
        .map(|file| {
            if file.path == "copy-source.txt" {
                summary("copy-source.txt", 1, 20)
            } else {
                file
            }
        })
        .collect();
    expected_inclusive.sort_by_key(key);
    assert_eq!(inclusive, expected_inclusive);

    let same = store
        .diff_checkpoints(&diff_input(
            &repo,
            &baseline,
            &baseline,
            false,
            DiffFormat::Numstat,
        ))
        .await
        .unwrap();
    assert_eq!(same, "");
    assert_eq!(repo.index_bytes(), user_index);
}

#[tokio::test]
async fn uses_head_for_a_missing_baseline_only_when_requested() {
    let repo = Repo::with_commit();
    let store = store();
    let from = checkpoint_ref_for_thread_turn("checkpoint-summary-fallback", 0);
    let to = checkpoint_ref_for_thread_turn("checkpoint-summary-fallback", 1);
    repo.write("README.md", "changed\n");
    store.capture_checkpoint(repo.path(), &to).await.unwrap();

    let mut input = diff_input(&repo, &from, &to, false, DiffFormat::Numstat);
    let error = store.diff_checkpoints(&input).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
    input.fallback_from_to_head = true;
    let numstat = store.diff_checkpoints(&input).await.unwrap();
    assert_eq!(
        parse_turn_diff_files_from_numstat(&numstat),
        [summary("README.md", 1, 1)]
    );
}

#[tokio::test]
async fn the_head_fallback_fails_in_a_repository_without_commits() {
    let repo = Repo::init();
    let store = store();
    let to = checkpoint_ref_for_thread_turn("unborn", 1);
    repo.write("a.txt", "a\n");
    store.capture_checkpoint(repo.path(), &to).await.unwrap();
    let mut input = diff_input(
        &repo,
        "refs/t3/checkpoints/missing",
        &to,
        false,
        DiffFormat::Patch,
    );
    input.fallback_from_to_head = true;
    let error = store.diff_checkpoints(&input).await.unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("Checkpoint ref is unavailable for diff operation."),
        "{error}"
    );
}

#[tokio::test]
async fn restore_returns_the_workspace_to_a_checkpoint() {
    let repo = Repo::with_commit();
    let store = store();
    let checkpoint = checkpoint_ref_for_thread_turn("restore", 1);
    repo.write("kept.txt", "kept\n");
    repo.write("dir/nested.txt", "nested\n");
    store
        .capture_checkpoint(repo.path(), &checkpoint)
        .await
        .unwrap();

    repo.write("README.md", "edited after\n");
    repo.write("later.txt", "later\n");
    repo.write("dir/other.txt", "other\n");
    repo.remove("kept.txt");
    repo.git(&["add", "later.txt"]);

    assert!(
        store
            .restore_checkpoint(repo.path(), &checkpoint, false)
            .await
            .unwrap()
    );
    assert_eq!(repo.read("README.md"), "# test\n");
    assert_eq!(repo.read("kept.txt"), "kept\n");
    assert_eq!(repo.read("dir/nested.txt"), "nested\n");
    assert!(!repo.exists("later.txt"));
    assert!(!repo.exists("dir/other.txt"));
    // Restoring resets the index to HEAD: the checkpointed untracked files are untracked again.
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "");
    assert!(repo.git(&["status", "--porcelain"]).contains("?? kept.txt"));
}

#[tokio::test]
async fn restore_reports_a_missing_ref_and_can_fall_back_to_head() {
    let repo = Repo::with_commit();
    let store = store();
    let missing = "refs/t3/checkpoints/missing";
    repo.write("README.md", "edited\n");
    repo.write("scratch.txt", "scratch\n");

    assert!(
        !store
            .has_checkpoint_ref(repo.path(), missing)
            .await
            .unwrap()
    );
    assert!(
        !store
            .restore_checkpoint(repo.path(), missing, false)
            .await
            .unwrap()
    );
    assert_eq!(repo.read("README.md"), "edited\n");

    assert!(
        store
            .restore_checkpoint(repo.path(), missing, true)
            .await
            .unwrap()
    );
    assert_eq!(repo.read("README.md"), "# test\n");
    assert!(!repo.exists("scratch.txt"));
}

#[tokio::test]
async fn deleting_checkpoint_refs_tolerates_missing_refs() {
    let repo = Repo::with_commit();
    let store = store();
    let first = checkpoint_ref_for_thread_turn("delete", 1);
    let second = checkpoint_ref_for_thread_turn("delete", 2);
    store.capture_checkpoint(repo.path(), &first).await.unwrap();
    store
        .capture_checkpoint(repo.path(), &second)
        .await
        .unwrap();
    assert!(store.has_checkpoint_ref(repo.path(), &first).await.unwrap());

    store
        .delete_checkpoint_refs(
            repo.path(),
            &[first.clone(), "refs/t3/checkpoints/never-existed".into()],
        )
        .await
        .unwrap();
    assert!(!store.has_checkpoint_ref(repo.path(), &first).await.unwrap());
    assert!(
        store
            .has_checkpoint_ref(repo.path(), &second)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn captured_commits_use_the_documented_identity_message_and_ref_layout() {
    let repo = Repo::with_commit();
    let checkpoint = checkpoint_ref_for_thread_turn("thread-1", 3);
    assert_eq!(checkpoint, "refs/t3/checkpoints/dGhyZWFkLTE/turn/3");
    store()
        .capture_checkpoint(repo.path(), &checkpoint)
        .await
        .unwrap();
    assert_eq!(
        repo.git(&[
            "log",
            "-1",
            "--format=%an <%ae>|%cn <%ce>|%s|%P",
            &checkpoint
        ]),
        "T3 Code <t3code@users.noreply.github.com>|T3 Code <t3code@users.noreply.github.com>|t3 checkpoint ref=refs/t3/checkpoints/dGhyZWFkLTE/turn/3|\n"
    );
    // The private index lives in the common git dir only while the capture runs.
    let leftovers: Vec<_> = std::fs::read_dir(repo.join(".git"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("t3-checkpoint-index-")
        })
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn refs_written_by_the_typescript_server_are_readable() {
    // A checkpoint commit built the way the TypeScript server builds one, by hand.
    let repo = Repo::with_commit();
    let ref_name = checkpoint_ref_for_thread_turn("ts-written", 1);
    repo.write("ts.txt", "from typescript\n");
    repo.git(&["add", "ts.txt"]);
    let tree = repo.git(&["write-tree"]);
    let commit = repo.git(&[
        "commit-tree",
        tree.trim(),
        "-m",
        &format!("t3 checkpoint ref={ref_name}"),
    ]);
    repo.git(&["update-ref", &ref_name, commit.trim()]);
    repo.git(&["reset", "--quiet"]);
    repo.write("ts.txt", "changed since\n");

    let store = store();
    assert!(
        store
            .has_checkpoint_ref(repo.path(), &ref_name)
            .await
            .unwrap()
    );
    assert!(
        store
            .restore_checkpoint(repo.path(), &ref_name, false)
            .await
            .unwrap()
    );
    assert_eq!(repo.read("ts.txt"), "from typescript\n");
}
