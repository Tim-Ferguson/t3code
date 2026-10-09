//! Ports of the checkpoint cases of the original GitVcsDriver.test.ts, plus a replay of the
//! git objects the original capture produced for scripted repositories.
mod common;

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use common::{
    Live, Repo, RunResult, env_value, git_with, git_with_timing, has_arg, live_git, set_times,
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use t3_vcs::{
    GitCommand,
    process::{ProcessRunInput, ProcessRunOutput},
};

const REF: &str = "refs/t3/checkpoints/test";

fn exit_error(error: &t3_vcs::VcsError) -> bool {
    error.tag() == "VcsProcessExitError"
}

fn git_dir_entries(repo: &Repo, prefix: &str) -> Vec<String> {
    std::fs::read_dir(repo.join(".git"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Replay of the original capture's git objects
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn capture_produces_the_same_git_objects_as_the_original() {
    let scenarios: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/checkpoint-capture.json")).unwrap();
    assert_eq!(scenarios.len(), 5);
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let repo = Repo::bare_dir();
        for op in scenario["ops"].as_array().unwrap() {
            let op: Vec<&str> = op
                .as_array()
                .unwrap()
                .iter()
                .map(|part| part.as_str().unwrap())
                .collect();
            match op[0] {
                "git" => {
                    repo.git(&op[1..]);
                }
                "write" => repo.write(op[1], op[2]),
                "remove" => repo.remove(op[1]),
                other => panic!("unknown op {other}"),
            }
        }
        let checkpoint = scenario["checkpointRef"].as_str().unwrap();
        live_git()
            .capture_checkpoint(repo.path(), checkpoint)
            .await
            .unwrap();

        assert_eq!(
            repo.git(&["rev-parse", &format!("{checkpoint}^{{tree}}")])
                .trim(),
            scenario["tree"],
            "{name}"
        );
        assert_eq!(
            repo.git(&["ls-tree", "-r", "-z", checkpoint]),
            scenario["lsTree"],
            "{name}"
        );
        assert_eq!(
            repo.git(&["log", "-1", "--format=%an|%ae|%cn|%ce|%s|%P", checkpoint]),
            scenario["commit"],
            "{name}"
        );
        // The user's index and working tree are exactly as the scenario left them.
        assert_eq!(
            repo.git(&["status", "--porcelain=v1", "-z"]),
            scenario["status"],
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Nested repositories and recovery
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn capture_skips_untracked_nested_repositories_without_a_commit() {
    let repo = Repo::checkpoint_fixture();
    let nested = "scratch/empty [repo]";
    repo.git(&["init", nested]);
    repo.git(&["init", "another empty"]);
    repo.write(&format!("{nested}/private.txt"), "nested\n");
    repo.git(&["init", "committed"]);
    repo.git(&[
        "-C",
        "committed",
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@test.com",
        "commit",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    let nested_head = repo
        .git(&["-C", "committed", "rev-parse", "HEAD"])
        .trim()
        .to_string();
    repo.write("untracked.txt", "new\n");
    let original_index = repo.index_bytes();

    live_git()
        .capture_checkpoint(repo.path(), REF)
        .await
        .unwrap();

    assert_eq!(repo.show(REF, "file.txt"), "unstaged\n");
    assert_eq!(repo.show(REF, "untracked.txt"), "new\n");
    assert_eq!(repo.git(&["ls-tree", "-r", REF, "--", nested]), "");
    assert_eq!(repo.git(&["ls-tree", REF, "--", "another empty"]), "");
    assert_eq!(
        repo.git(&["ls-tree", REF, "--", "committed"]),
        format!("160000 commit {nested_head}\tcommitted\n")
    );
    assert_eq!(repo.index_bytes(), original_index);
    assert_eq!(repo.read(&format!("{nested}/private.txt")), "nested\n");
}

#[tokio::test]
async fn recovery_discovers_nested_head_independently_of_inherited_git_dir() {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["init", "empty"]);
    let original_index = repo.index_bytes();
    // The server's own environment binds git to this repository; the nested probe must not see it.
    let git_dir = repo.join(".git").to_string_lossy().into_owned();
    let git = git_with(move |mut input| {
        input
            .env
            .insert(0, ("GIT_DIR".into(), Some(git_dir.clone())));
        Live.run(input)
    });
    git.capture_checkpoint(repo.path(), REF).await.unwrap();
    assert_eq!(repo.show(REF, "file.txt"), "unstaged\n");
    assert_eq!(repo.git(&["ls-tree", "-r", REF, "--", "empty"]), "");
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn capture_still_fails_when_a_clean_filter_rejects_a_file() {
    let repo = Repo::checkpoint_fixture();
    repo.write(".gitattributes", "file.txt filter=reject\n");
    repo.git(&["config", "filter.reject.clean", "false"]);
    repo.git(&["config", "filter.reject.required", "true"]);
    let original_index = repo.index_bytes();

    assert!(
        live_git()
            .capture_checkpoint(repo.path(), REF)
            .await
            .is_err()
    );
    assert_eq!(repo.index_bytes(), original_index);
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn capture_refuses_a_truncated_nested_repository_listing() {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["init", "empty"]);
    let git = git_with(|input| {
        let truncate = has_arg(&input, "--others");
        let result = Live.run(input);
        Box::pin(async move {
            let mut output = result.await?;
            output.stdout_truncated |= truncate;
            Ok(output)
        })
    });
    assert!(git.capture_checkpoint(repo.path(), REF).await.is_err());
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn recovery_refuses_excessive_candidates_before_probing() {
    let repo = Repo::checkpoint_fixture();
    for index in 0..65 {
        repo.git(&["init", &format!("empty{index}")]);
    }
    let original_index = repo.index_bytes();
    let probes = Arc::new(AtomicUsize::new(0));
    let stages = Arc::new(AtomicUsize::new(0));
    let root = repo.path().to_string_lossy().into_owned();
    let git = git_with({
        let (probes, stages) = (probes.clone(), stages.clone());
        move |input| {
            if has_arg(&input, "rev-parse") && input.args.get(1).is_some_and(|dir| *dir != root) {
                probes.fetch_add(1, Ordering::SeqCst);
            }
            if has_arg(&input, "add") && has_arg(&input, "-A") {
                stages.fetch_add(1, Ordering::SeqCst);
            }
            Live.run(input)
        }
    });

    let error = git.capture_checkpoint(repo.path(), REF).await.unwrap_err();
    assert!(exit_error(&error));
    assert_eq!(probes.load(Ordering::SeqCst), 0);
    assert_eq!(stages.load(Ordering::SeqCst), 1);
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
    assert_eq!(repo.index_bytes(), original_index);
}

/// A git lock held by "another process" while one command of the capture runs.
async fn lock_race(phase: &'static str, nested_recovery: bool) {
    let repo = Repo::checkpoint_fixture();
    if nested_recovery {
        repo.git(&["init", "empty"]);
    }
    let original_index = repo.index_bytes();
    let ref_lock = repo.join(&format!(".git/{REF}.lock"));
    let raced = Arc::new(AtomicUsize::new(0));
    let discoveries = Arc::new(AtomicUsize::new(0));
    let private_indexes = Arc::new(Mutex::new(HashSet::new()));
    let git = git_with({
        let (raced, discoveries, private_indexes) =
            (raced.clone(), discoveries.clone(), private_indexes.clone());
        move |input| {
            if has_arg(&input, "--others") {
                discoveries.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(index) = env_value(&input, "GIT_INDEX_FILE") {
                private_indexes.lock().unwrap().insert(index);
            }
            let initial_stage = phase == "add"
                && nested_recovery
                && !input
                    .args
                    .iter()
                    .any(|arg| arg.starts_with(":(exclude,literal)"));
            let targeted = has_arg(&input, phase) && !initial_stage;
            if !targeted || raced.fetch_add(1, Ordering::SeqCst) != 0 {
                return Live.run(input);
            }
            let lock = if phase == "add" {
                std::path::PathBuf::from(format!(
                    "{}.lock",
                    env_value(&input, "GIT_INDEX_FILE").unwrap()
                ))
            } else {
                ref_lock.clone()
            };
            Box::pin(async move {
                std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
                std::fs::write(&lock, "concurrent ref writer").unwrap();
                let result = Live.run(input).await;
                std::fs::remove_file(&lock).unwrap();
                result
            })
        }
    });

    git.capture_checkpoint(repo.path(), REF).await.unwrap();

    // The injected failure plus the retry that succeeded.
    assert_eq!(raced.load(Ordering::SeqCst), 2);
    assert_eq!(
        discoveries.load(Ordering::SeqCst),
        usize::from(nested_recovery)
    );
    let private_indexes = private_indexes.lock().unwrap();
    assert_eq!(private_indexes.len(), 1);
    for index in private_indexes.iter() {
        assert!(!std::path::Path::new(index).exists());
        assert!(!std::path::Path::new(&format!("{index}.lock")).exists());
    }
    assert!(
        live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn capture_retries_an_index_lock_during_staging() {
    lock_race("add", false).await;
}

#[tokio::test]
async fn capture_retries_a_ref_lock_during_update_ref() {
    lock_race("update-ref", false).await;
}

#[tokio::test]
async fn recovery_then_a_ref_lock_is_retried() {
    lock_race("update-ref", true).await;
}

#[tokio::test]
async fn recovery_retries_a_lock_on_its_own_staging_pass() {
    lock_race("add", true).await;
}

/// Where the capture is blocked forever, to prove one deadline bounds all of recovery.
#[derive(Clone, Copy, Debug)]
enum Blocked {
    Discovery,
    Probe,
    RetryStage,
}

async fn recovery_deadline(blocked: Blocked) {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["init", "empty"]);
    let original_index = repo.index_bytes();
    let root = repo.path().to_string_lossy().into_owned();
    let entered = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let private_index = Arc::new(Mutex::new(None::<String>));
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let git = git_with_timing(
        {
            let (entered, interrupted, private_index) =
                (entered.clone(), interrupted.clone(), private_index.clone());
            move |input| {
                if has_arg(&input, "add") && has_arg(&input, "-A") {
                    *private_index.lock().unwrap() = env_value(&input, "GIT_INDEX_FILE");
                }
                let block = match blocked {
                    Blocked::Discovery => has_arg(&input, "--others"),
                    Blocked::Probe => {
                        has_arg(&input, "rev-parse")
                            && input.args.get(1).is_some_and(|dir| *dir != root)
                    }
                    Blocked::RetryStage => input
                        .args
                        .iter()
                        .any(|arg| arg.starts_with(":(exclude,literal)")),
                };
                if !block {
                    return Live.run(input);
                }
                let (entered, interrupted) = (entered.clone(), interrupted.clone());
                Box::pin(async move {
                    if matches!(blocked, Blocked::RetryStage) {
                        let index = env_value(&input, "GIT_INDEX_FILE").unwrap();
                        std::fs::write(format!("{index}.lock"), "interrupted index write").unwrap();
                    }
                    entered.store(true, Ordering::SeqCst);
                    let _flag = OnDrop(interrupted);
                    std::future::pending::<RunResult>().await
                })
            }
        },
        Duration::from_secs(3600),
        Duration::from_millis(600),
    );

    let error = git.capture_checkpoint(repo.path(), REF).await.unwrap_err();
    assert!(exit_error(&error));
    // Whatever was blocked was dropped, and nothing of the capture is left behind.
    assert_eq!(
        entered.load(Ordering::SeqCst),
        interrupted.load(Ordering::SeqCst),
        "{blocked:?}"
    );
    let index = private_index
        .lock()
        .unwrap()
        .clone()
        .expect("staging was attempted");
    assert!(!std::path::Path::new(&index).exists());
    assert!(!std::path::Path::new(&format!("{index}.lock")).exists());
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn recovery_has_one_deadline_including_discovery() {
    recovery_deadline(Blocked::Discovery).await;
}

#[tokio::test]
async fn recovery_has_one_deadline_including_probes() {
    recovery_deadline(Blocked::Probe).await;
}

#[tokio::test]
async fn recovery_has_one_deadline_including_the_staging_retry() {
    recovery_deadline(Blocked::RetryStage).await;
}

#[tokio::test]
async fn an_expired_recovery_deadline_reports_the_original_staging_error() {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["init", "empty"]);
    let original_index = repo.index_bytes();
    let raced = Arc::new(AtomicUsize::new(0));
    let git = git_with_timing(
        {
            let raced = raced.clone();
            move |input| {
                let exclusion_stage = input
                    .args
                    .iter()
                    .any(|arg| arg.starts_with(":(exclude,literal)"));
                if !(has_arg(&input, "add") && exclusion_stage) {
                    return Live.run(input);
                }
                raced.fetch_add(1, Ordering::SeqCst);
                let lock = format!("{}.lock", env_value(&input, "GIT_INDEX_FILE").unwrap());
                Box::pin(async move {
                    std::fs::write(&lock, "concurrent index writer").unwrap();
                    let result = Live.run(input).await;
                    std::fs::remove_file(&lock).unwrap();
                    result
                })
            }
        },
        // The lock failure is retryable, but the next attempt is an hour away.
        Duration::from_secs(3600),
        Duration::from_millis(600),
    );
    let error = git.capture_checkpoint(repo.path(), REF).await.unwrap_err();
    assert!(exit_error(&error));
    assert!(raced.load(Ordering::SeqCst) <= 1);
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn dropping_a_capture_removes_the_private_index() {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["init", "empty"]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let private_index = Arc::new(Mutex::new(None::<String>));
    let git = git_with({
        let (entered, private_index) = (entered.clone(), private_index.clone());
        move |input| {
            if has_arg(&input, "add") && has_arg(&input, "-A") {
                *private_index.lock().unwrap() = env_value(&input, "GIT_INDEX_FILE");
            }
            if has_arg(&input, "--others") {
                let entered = entered.clone();
                return Box::pin(async move {
                    entered.notify_one();
                    std::future::pending::<RunResult>().await
                });
            }
            Live.run(input)
        }
    });
    let capture = git.capture_checkpoint(repo.path(), REF);
    tokio::select! {
        _ = capture => panic!("the capture was blocked and cannot finish"),
        _ = entered.notified() => {}
    }
    let index = private_index
        .lock()
        .unwrap()
        .clone()
        .expect("staging was attempted");
    assert!(!std::path::Path::new(&index).exists());
    assert!(
        !live_git()
            .has_checkpoint_ref(repo.path(), REF)
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------------------------------------
// Index reuse
// ---------------------------------------------------------------------------------------------

fn clean_filter_probe(repo: &Repo) {
    repo.write(".git/filter.sh", "echo read >> .git/reads\ncat\n");
    repo.git(&["config", "filter.probe.clean", "sh .git/filter.sh"]);
}

#[tokio::test]
async fn capture_does_not_rerun_clean_filters_for_unchanged_indexed_files() {
    let repo = Repo::checkpoint_fixture();
    repo.write(".gitattributes", "stable.txt filter=probe\n");
    repo.write("stable.txt", "unchanged\n");
    clean_filter_probe(&repo);
    set_times(&repo.join("stable.txt"), 1_700_000_000.0);
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "record stable file"]);
    repo.write(".git/reads", "");
    repo.write("file.txt", "changed\n");
    let original_index = repo.index_bytes();

    live_git()
        .capture_checkpoint(repo.path(), REF)
        .await
        .unwrap();

    assert_eq!(repo.read(".git/reads"), "");
    assert_eq!(repo.show(REF, "file.txt"), "changed\n");
    assert_eq!(repo.show(REF, "stable.txt"), "unchanged\n");
    assert_eq!(repo.index_bytes(), original_index);
}

async fn index_inspection(index_mode: &'static str) {
    let repo = Repo::checkpoint_fixture();
    repo.write(".gitattributes", "stable filter=probe\n");
    repo.write("stable", "unchanged\n");
    repo.write("z-skipped", "original\n");
    repo.write("excluded/file", "absent\n");
    clean_filter_probe(&repo);
    set_times(&repo.join("stable"), 1_700_000_000.0);
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "inspection fixture"]);
    if index_mode == "flags" {
        repo.git(&["update-index", "--skip-worktree", "z-skipped"]);
    }
    if index_mode == "sparse" {
        repo.git(&[
            "sparse-checkout",
            "set",
            "--cone",
            "--sparse-index",
            "included",
        ]);
    }
    repo.write("z-skipped", "modified\n");
    repo.write(".git/reads", "");
    let original_index = repo.index_bytes();
    // Tiny output cap and one-byte chunks: inspection must handle records split anywhere.
    let git = git_with(|mut input| {
        if has_arg(&input, "ls-files") {
            input.max_output_bytes = 8;
            if let Some(inner) = input.on_stdout_chunk.take() {
                input.on_stdout_chunk = Some(Arc::new(move |chunk: &[u8]| {
                    for byte in chunk.chunks(1) {
                        inner(byte);
                    }
                }));
            }
        }
        Live.run(input)
    });

    git.capture_checkpoint(repo.path(), REF).await.unwrap();

    assert_eq!(repo.show(REF, "z-skipped"), "modified\n");
    if index_mode != "flags" {
        assert_eq!(repo.read(".git/reads"), "");
    }
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn index_inspection_handles_entries_beyond_the_output_cap_normal() {
    index_inspection("normal").await;
}

#[tokio::test]
async fn index_inspection_handles_entries_beyond_the_output_cap_flags() {
    index_inspection("flags").await;
}

#[tokio::test]
async fn index_inspection_handles_entries_beyond_the_output_cap_sparse() {
    index_inspection("sparse").await;
}

async fn racy_same_size_edit(timestamp: f64) {
    let repo = Repo::checkpoint_fixture();
    let index = repo.join(".git/index");
    repo.git(&["config", "core.trustctime", "false"]);
    repo.write("file.txt", "before\n");
    set_times(&repo.join("file.txt"), timestamp);
    repo.git(&["add", "file.txt"]);
    repo.git(&["commit", "-m", "record racy file"]);
    set_times(&index, timestamp);
    let original_index = std::fs::read(&index).unwrap();
    let original_mtime = std::fs::metadata(&index).unwrap().modified().unwrap();
    repo.write("file.txt", "after!\n");
    set_times(&repo.join("file.txt"), timestamp);

    live_git()
        .capture_checkpoint(repo.path(), REF)
        .await
        .unwrap();

    assert_eq!(repo.show(REF, "file.txt"), "after!\n");
    assert_eq!(std::fs::read(&index).unwrap(), original_index);
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        original_mtime
    );
}

#[tokio::test]
async fn capture_preserves_same_size_edits_with_a_whole_second_racy_timestamp() {
    racy_same_size_edit(1_700_000_000.0).await;
}

#[tokio::test]
async fn capture_preserves_same_size_edits_with_a_fractional_racy_timestamp() {
    racy_same_size_edit(1_700_000_000.9999).await;
}

#[tokio::test]
async fn capture_preserves_racy_edits_made_after_resetting_the_index() {
    let repo = Repo::checkpoint_fixture();
    let index = repo.join(".git/index");
    let timestamp = 1_700_000_000.0;
    repo.git(&["config", "core.trustctime", "false"]);
    repo.write("racy.txt", "before\n");
    set_times(&repo.join("racy.txt"), timestamp);
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "record racy file"]);
    repo.write("file.txt", "staged\n");
    repo.git(&["add", "file.txt"]);
    set_times(&index, timestamp);
    let original_index = std::fs::read(&index).unwrap();
    let original_mtime = std::fs::metadata(&index).unwrap().modified().unwrap();
    let racy = repo.join("racy.txt");
    let git = git_with(move |input| {
        let after_reset = has_arg(&input, "read-tree") && has_arg(&input, "--reset");
        let result = Live.run(input);
        let racy = racy.clone();
        Box::pin(async move {
            let result = result.await;
            if after_reset {
                std::fs::write(&racy, "after!\n").unwrap();
                set_times(&racy, timestamp);
            }
            result
        })
    });

    git.capture_checkpoint(repo.path(), REF).await.unwrap();

    assert_eq!(repo.show(REF, "racy.txt"), "after!\n");
    assert_eq!(repo.show(REF, "file.txt"), "staged\n");
    assert_eq!(std::fs::read(&index).unwrap(), original_index);
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        original_mtime
    );
}

async fn index_reuse_two_turns(nested: bool, index_mode: &'static str) {
    let repo = Repo::checkpoint_fixture();
    for name in [
        "scope/staged",
        "scope/deleted",
        "scope/assumed",
        "scope/skipped",
        "outside",
    ] {
        repo.write(name, "original\n");
    }
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial scoped files"]);
    repo.write("scope/staged", "staged\n");
    repo.write("scope/new-deleted", "staged then deleted\n");
    repo.write("outside", "staged outside\n");
    repo.git(&["add", "."]);
    if index_mode == "flags" {
        repo.git(&["update-index", "--assume-unchanged", "scope/assumed"]);
        repo.git(&["update-index", "--skip-worktree", "scope/skipped"]);
    }
    if index_mode == "split" {
        repo.git(&["update-index", "--split-index"]);
    }
    let original_index = repo.index_bytes();
    for name in ["scope/staged", "scope/assumed", "scope/skipped", "outside"] {
        repo.write(name, "working\n");
    }
    repo.write("scope/new", "first\n");
    repo.remove("scope/deleted");
    repo.remove("scope/new-deleted");
    let capture_cwd = if nested {
        repo.join("scope")
    } else {
        repo.path().to_path_buf()
    };
    let (first, second) = ("refs/t3/checkpoints/turns/1", "refs/t3/checkpoints/turns/2");
    let git = live_git();

    git.capture_checkpoint(&capture_cwd, first).await.unwrap();
    for name in ["scope/staged", "scope/assumed", "scope/skipped"] {
        assert_eq!(repo.show(first, name), "working\n", "{name}");
    }
    assert_eq!(
        repo.show(first, "outside"),
        if nested { "original\n" } else { "working\n" }
    );
    let files = repo.git(&["ls-tree", "-r", "--name-only", first]);
    let files: Vec<_> = files.lines().collect();
    assert!(!files.contains(&"scope/deleted") && !files.contains(&"scope/new-deleted"));
    assert!(files.contains(&"scope/new"));

    repo.write("scope/staged", "second\n");
    repo.remove("scope/new");
    repo.write("scope/second", "added in second turn\n");
    git.capture_checkpoint(&capture_cwd, second).await.unwrap();
    assert_eq!(
        repo.git(&["diff", "--name-only", first, second]),
        "scope/new\nscope/second\nscope/staged\n"
    );
    assert_eq!(repo.show(second, "scope/staged"), "second\n");
    assert_eq!(repo.show(second, "scope/second"), "added in second turn\n");
    assert_eq!(repo.index_bytes(), original_index);
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_normal() {
    index_reuse_two_turns(false, "normal").await;
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_flags() {
    index_reuse_two_turns(false, "flags").await;
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_split_index() {
    index_reuse_two_turns(false, "split").await;
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_from_a_nested_workspace_normal() {
    index_reuse_two_turns(true, "normal").await;
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_from_a_nested_workspace_flags() {
    index_reuse_two_turns(true, "flags").await;
}

#[tokio::test]
async fn index_reuse_preserves_two_turns_from_a_nested_workspace_split_index() {
    index_reuse_two_turns(true, "split").await;
}

async fn falls_back_when_the_user_index_is(state: &str) {
    let repo = Repo::checkpoint_fixture();
    let index = repo.join(".git/index");
    if state == "missing" {
        std::fs::remove_file(&index).unwrap();
    } else {
        std::fs::write(&index, "invalid index").unwrap();
    }
    live_git()
        .capture_checkpoint(repo.path(), REF)
        .await
        .unwrap();
    assert_eq!(repo.show(REF, "file.txt"), "unstaged\n");
    if state == "missing" {
        assert!(!index.exists());
    } else {
        assert_eq!(std::fs::read_to_string(&index).unwrap(), "invalid index");
    }
}

#[tokio::test]
async fn capture_falls_back_when_the_user_index_is_missing() {
    falls_back_when_the_user_index_is("missing").await;
}

#[tokio::test]
async fn capture_falls_back_when_the_user_index_is_invalid() {
    falls_back_when_the_user_index_is("invalid").await;
}

// ---------------------------------------------------------------------------------------------
// Sparse checkouts
// ---------------------------------------------------------------------------------------------

async fn sparse_two_captures(nested: bool, index_state: &'static str) {
    let repo = Repo::checkpoint_fixture();
    for name in [
        "scope/in/edit",
        "scope/in/delete",
        "scope/out/deep/absent",
        "scope/out/present",
        "elsewhere/file",
    ] {
        repo.write(name, "original\n");
    }
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "sparse fixture"]);
    repo.git(&[
        "sparse-checkout",
        "set",
        "--cone",
        "--sparse-index",
        "scope/in",
        "elsewhere",
    ]);
    if index_state == "non-cone-missing" {
        repo.git(&[
            "sparse-checkout",
            "set",
            "--no-cone",
            "/scope/in/",
            "/elsewhere/",
        ]);
    }
    repo.write("scope/in/edit", "staged\n");
    repo.write("elsewhere/file", "staged outside\n");
    repo.git(&["add", "."]);
    if index_state == "flags" {
        repo.git(&["update-index", "--assume-unchanged", "scope/in/delete"]);
    }
    if index_state == "manual-skip" {
        repo.git(&["update-index", "--skip-worktree", "scope/in/delete"]);
    }
    repo.git(&["config", "sparse.expectFilesOutsideOfPatterns", "true"]);
    repo.write("scope/in/edit", "working\n");
    repo.write("scope/out/present", "modified skipped\n");
    repo.write("scope/out/new file", "new outside cone\n");
    repo.write("elsewhere/file", "working outside\n");
    repo.remove("scope/in/delete");
    let index = repo.join(".git/index");
    if index_state.ends_with("missing") {
        std::fs::remove_file(&index).unwrap();
    }
    let original_index = repo.index_bytes();
    let capture_cwd = if nested {
        repo.join("scope")
    } else {
        repo.path().to_path_buf()
    };
    let git = live_git();

    for turn in [1, 2] {
        let reference = format!("refs/t3/checkpoints/sparse/{turn}");
        if turn == 2 {
            repo.write("scope/in/edit", "second\n");
            repo.remove("scope/out/new file");
            repo.write("scope/out/second", "second addition\n");
        }
        let capture = git.capture_checkpoint(&capture_cwd, &reference).await;
        if index_state == "non-cone-missing" {
            assert!(exit_error(&capture.unwrap_err()));
            assert!(
                !git.has_checkpoint_ref(&capture_cwd, &reference)
                    .await
                    .unwrap()
            );
            assert!(!index.exists());
            break;
        }
        capture.unwrap();
        let (new_name, new_content) = if turn == 1 {
            ("scope/out/new file", "new outside cone\n")
        } else {
            ("scope/out/second", "second addition\n")
        };
        for (name, content) in [
            ("scope/out/deep/absent", "original\n"),
            ("scope/out/present", "modified skipped\n"),
            (
                "scope/in/edit",
                if turn == 1 { "working\n" } else { "second\n" },
            ),
            (
                "elsewhere/file",
                if nested {
                    "original\n"
                } else {
                    "working outside\n"
                },
            ),
            (new_name, new_content),
        ] {
            assert_eq!(repo.show(&reference, name), content, "turn {turn}: {name}");
        }
        let files = repo.git(&["ls-tree", "-rz", "--name-only", &reference]);
        let files: Vec<_> = files.split('\0').collect();
        assert!(!files.contains(&"scope/in/delete"));
        if turn == 2 {
            assert!(!files.contains(&"scope/out/new file"));
        }
        assert_eq!(repo.index_bytes(), original_index);
        assert!(!repo.exists("scope/out/deep/absent"));
        assert_eq!(repo.read("elsewhere/file"), "working outside\n");
    }
}

macro_rules! sparse_cases {
    ($($name:ident: $nested:expr, $state:expr;)*) => {$(
        #[tokio::test]
        async fn $name() {
            sparse_two_captures($nested, $state).await;
        }
    )*};
}

sparse_cases! {
    sparse_captures_keep_two_turns_sparse: false, "sparse";
    sparse_captures_keep_two_turns_flags: false, "flags";
    sparse_captures_keep_two_turns_manual_skip: false, "manual-skip";
    sparse_captures_keep_two_turns_missing_index: false, "missing";
    sparse_captures_refuse_a_non_cone_rebuild: false, "non-cone-missing";
    nested_sparse_captures_keep_two_turns_sparse: true, "sparse";
    nested_sparse_captures_keep_two_turns_flags: true, "flags";
    nested_sparse_captures_keep_two_turns_manual_skip: true, "manual-skip";
    nested_sparse_captures_keep_two_turns_missing_index: true, "missing";
    nested_sparse_captures_refuse_a_non_cone_rebuild: true, "non-cone-missing";
}

#[tokio::test]
async fn capture_keeps_the_legacy_path_when_git_lacks_add_sparse() {
    let repo = Repo::checkpoint_fixture();
    repo.git(&["sparse-checkout", "set", "--cone", "included"]);
    let original_index = repo.index_bytes();
    let git = git_with(|mut input| {
        if has_arg(&input, "-h") {
            return Box::pin(async {
                Ok(ProcessRunOutput {
                    code: Some(129),
                    stdout: "usage: git add".into(),
                    ..ProcessRunOutput::default()
                })
            }) as BoxFuture<'static, RunResult>;
        }
        for arg in &mut input.args {
            if arg == "--sparse" {
                *arg = "--unsupported-sparse".into();
            }
        }
        Live.run(input)
    });
    git.capture_checkpoint(repo.path(), REF).await.unwrap();
    assert_eq!(repo.show(REF, "file.txt"), "unstaged\n");
    assert_eq!(repo.index_bytes(), original_index);
}

// ---------------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restores_empty_checkpoints_without_changing_paths_outside_the_workspace() {
    for nested in [false, true] {
        let root = Repo::init();
        if nested {
            root.write("outside.txt", "original\n");
            root.git(&["add", "."]);
        }
        root.git(&["commit", "--allow-empty", "-m", "initial"]);
        let cwd = if nested {
            root.join("nested")
        } else {
            root.path().to_path_buf()
        };
        std::fs::create_dir_all(&cwd).unwrap();
        let git = live_git();
        let checkpoint = "refs/t3/checkpoints/empty";
        git.capture_checkpoint(&cwd, checkpoint).await.unwrap();
        if nested {
            root.write("outside.txt", "changed\n");
            root.git(&["add", "outside.txt"]);
        }
        for staged in [false, true] {
            let added = cwd.join("added.txt");
            std::fs::write(&added, "new\n").unwrap();
            if staged {
                common::run_git(&cwd, &["add", "added.txt"]);
            }
            assert!(
                git.restore_checkpoint(&cwd, checkpoint, false)
                    .await
                    .unwrap()
            );
            assert!(!added.exists());
        }
        root.write(".git/info/exclude", "ignored.txt\n");
        std::fs::write(cwd.join("ignored.txt"), "keep\n").unwrap();
        std::fs::create_dir(cwd.join("untracked")).unwrap();
        std::fs::write(cwd.join("untracked/file.txt"), "remove\n").unwrap();
        assert!(
            git.restore_checkpoint(&cwd, checkpoint, false)
                .await
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("ignored.txt")).unwrap(),
            "keep\n"
        );
        assert!(!cwd.join("untracked").exists());
        if nested {
            assert_eq!(root.read("outside.txt"), "changed\n");
            assert_eq!(
                root.git(&["diff", "--cached", "--name-only"]).trim(),
                "outside.txt"
            );
        }
    }
}

#[tokio::test]
async fn restore_recreates_a_workspace_directory_that_restoring_removed() {
    let repo = Repo::init();
    repo.write("README.md", "# test\n");
    repo.write("nested/a.txt", "tracked\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);
    let cwd = repo.join("nested");
    // The checkpoint holds no tracked file under the workspace, which itself still exists.
    repo.remove("nested/a.txt");
    let git = live_git();
    git.capture_checkpoint(&cwd, REF).await.unwrap();
    repo.git(&["checkout", "--", "nested/a.txt"]);

    // Restoring drops a.txt from the index and worktree, and git removes the emptied directory.
    assert!(git.restore_checkpoint(&cwd, REF, false).await.unwrap());
    assert!(cwd.is_dir());
    assert!(!cwd.join("a.txt").exists());
}

// ---------------------------------------------------------------------------------------------
// Process-level behaviour
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn execute_forwards_env_limits_and_runs_from_the_servers_directory() {
    let seen = Arc::new(Mutex::new(None::<ProcessRunInput>));
    let git = git_with({
        let seen = seen.clone();
        move |input| {
            *seen.lock().unwrap() = Some(input);
            Box::pin(async {
                Ok(ProcessRunOutput {
                    code: Some(0),
                    ..ProcessRunOutput::default()
                })
            })
        }
    });
    let mut command = GitCommand::new("GitVcsDriver.test.env", "/repo", ["status"])
        .env(vec![(
            "GIT_INDEX_FILE".into(),
            Some("/tmp/t3-index".into()),
        )])
        .append_truncation_marker()
        .output_mode(t3_vcs::process::OutputMode::Error);
    command.timeout = Some(Duration::from_secs(3));
    git.execute(command).await.unwrap();

    let input = seen.lock().unwrap().take().unwrap();
    assert_eq!(input.command, "git");
    assert_eq!(input.args, ["-C", "/repo", "status"]);
    assert_eq!(
        input.env,
        [(
            "GIT_INDEX_FILE".to_string(),
            Some("/tmp/t3-index".to_string())
        )]
    );
    assert_eq!(input.truncated_marker, "\n\n[truncated]");
    assert_eq!(input.output_mode, t3_vcs::process::OutputMode::Error);
    assert_eq!(input.timeout, Duration::from_secs(3));
    assert_eq!(input.spawn_cwd, std::env::current_dir().ok());
}

#[tokio::test]
async fn capture_flushes_objects_and_refs_to_disk_before_publishing_them() {
    let seen = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let git = git_with({
        let seen = seen.clone();
        move |input| {
            seen.lock().unwrap().push(input.args.clone());
            let stdout = if has_arg(&input, "write-tree") {
                "tree0000\n"
            } else if has_arg(&input, "commit-tree") {
                "commit0000\n"
            } else if has_arg(&input, "--git-common-dir") {
                ".git\n"
            } else {
                ""
            };
            Box::pin(async move {
                Ok(ProcessRunOutput {
                    code: Some(0),
                    stdout: stdout.into(),
                    ..ProcessRunOutput::default()
                })
            })
        }
    });
    let checkpoint = "refs/t3/checkpoints/thread/turn/1";
    git.capture_checkpoint(std::path::Path::new("/repo"), checkpoint)
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    let write_commands = ["add", "write-tree", "commit-tree", "update-ref"];
    let writes: Vec<_> = seen
        .iter()
        .filter(|args| {
            args.iter()
                .any(|arg| write_commands.contains(&arg.as_str()))
        })
        .collect();
    assert_eq!(writes.len(), 4);
    for args in writes {
        let command = args
            .iter()
            .position(|arg| write_commands.contains(&arg.as_str()))
            .unwrap();
        for setting in ["core.fsync=objects,reference", "core.fsyncMethod=fsync"] {
            let index = args
                .iter()
                .position(|arg| arg == setting)
                .unwrap_or_else(|| panic!("{args:?}"));
            assert_eq!(args[index - 1], "-c", "{args:?}");
            assert!(index < command, "{args:?}");
        }
    }
    assert_eq!(
        seen.last().unwrap(),
        &[
            "-C",
            "/repo",
            "-c",
            "core.fsync=objects,reference",
            "-c",
            "core.fsyncMethod=fsync",
            "update-ref",
            checkpoint,
            "commit0000",
        ]
    );
}

#[tokio::test]
async fn capture_commands_carry_the_documented_environment() {
    let repo = Repo::checkpoint_fixture();
    let identity = Arc::new(Mutex::new(Vec::<(
        Vec<String>,
        Vec<(String, Option<String>)>,
    )>::new()));
    let git = git_with({
        let identity = identity.clone();
        move |input| {
            identity
                .lock()
                .unwrap()
                .push((input.args.clone(), input.env.clone()));
            Live.run(input)
        }
    });
    git.capture_checkpoint(repo.path(), REF).await.unwrap();
    let seen = identity.lock().unwrap();
    let env_of = |command: &str| {
        seen.iter()
            .find(|(args, _)| args.iter().any(|arg| arg == command))
            .map(|(_, env)| env.clone())
            .unwrap()
    };
    let commit_env = env_of("commit-tree");
    for (name, value) in [
        ("GIT_AUTHOR_NAME", "T3 Code"),
        ("GIT_AUTHOR_EMAIL", "t3code@users.noreply.github.com"),
        ("GIT_COMMITTER_NAME", "T3 Code"),
        ("GIT_COMMITTER_EMAIL", "t3code@users.noreply.github.com"),
    ] {
        assert!(
            commit_env.contains(&(name.to_string(), Some(value.to_string()))),
            "{name}"
        );
    }
    assert!(
        commit_env
            .iter()
            .any(|(name, value)| name == "GIT_INDEX_FILE" && value.is_some())
    );
    // The final ref update must see the real repository index, not the private one.
    assert!(
        env_of("update-ref")
            .iter()
            .all(|(name, _)| name != "GIT_INDEX_FILE")
    );
    drop(seen);
    assert!(git_dir_entries(&repo, "t3-checkpoint-index-").is_empty());
}
