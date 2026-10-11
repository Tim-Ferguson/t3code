//! Ports the real-process cases of the original VcsProcess.test.ts (run through `sh` instead
//! of `node`) and the concurrency bound.
#![cfg(unix)]
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

mod common;
use common::HANG_BOUND;
use futures_util::future::BoxFuture;
use t3_vcs::{
    VcsError, VcsProcess, VcsProcessInput,
    process::{OutputMode, ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner},
};

fn sh(script: &str) -> VcsProcessInput {
    VcsProcessInput::new(
        "test.sh",
        "sh",
        ["-c", script],
        std::env::current_dir().unwrap(),
    )
}

#[tokio::test]
async fn collects_stdout() {
    let result = VcsProcess::new().run(sh("printf hello")).await.unwrap();
    assert_eq!(result.stdout, "hello");
    assert_eq!(result.stderr, "");
    assert!(!result.stdout_truncated && !result.stderr_truncated);
}

#[tokio::test]
async fn writes_stdin_before_waiting_for_exit() {
    let mut input = sh("cat");
    input.stdin = Some("stdin payload".into());
    assert_eq!(
        VcsProcess::new().run(input).await.unwrap().stdout,
        "stdin payload"
    );
}

#[tokio::test]
async fn without_stdin_the_child_sees_end_of_input() {
    assert_eq!(
        VcsProcess::new()
            .run(sh("cat; printf done"))
            .await
            .unwrap()
            .stdout,
        "done"
    );
}

#[tokio::test]
async fn non_zero_exits_fail_without_leaking_arguments_or_stderr() {
    let secret_argument = "--token=super-secret-token";
    let secret_stderr = "remote rejected super-secret-token";
    let mut input = sh(r#"printf %s "$1" >&2; exit 2"#);
    input
        .args
        .extend(["sh".into(), secret_stderr.into(), secret_argument.into()]);
    let error = VcsProcess::new().run(input).await.unwrap_err();
    let VcsError::ProcessExit {
        context,
        exit_code,
        detail,
        failure_kind,
        stderr_length,
        stderr_truncated,
        ..
    } = &error
    else {
        panic!("expected an exit error, got {error:?}");
    };
    assert_eq!(context.operation, "test.sh");
    assert_eq!(context.argument_count, Some(5));
    assert_eq!(
        (*exit_code, detail.as_str()),
        (2, "Process exited with a non-zero status.")
    );
    assert_eq!(
        failure_kind.map(|kind| kind.as_str()),
        Some("command-failed")
    );
    assert_eq!(
        (*stderr_length, *stderr_truncated),
        (Some(secret_stderr.len()), Some(false))
    );
    assert!(!error.to_string().contains("super-secret-token"));
}

#[tokio::test]
async fn authentication_failures_are_classified_from_real_stderr() {
    let error = VcsProcess::new()
        .run(sh(
            "printf 'authentication failed for token super-secret-token' >&2; exit 1",
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, VcsError::ProcessExit { detail, .. } if detail == "Authentication failed.")
    );
    assert!(!error.to_string().contains("super-secret-token"));
}

#[tokio::test]
async fn a_missing_executable_is_a_spawn_error() {
    let secret = "--token=super-secret-token";
    let input = VcsProcessInput::new(
        "test.spawn",
        "definitely-not-a-t3code-executable",
        [secret],
        std::env::current_dir().unwrap(),
    );
    let error = VcsProcess::new().run(input).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessSpawnError");
    assert!(std::error::Error::source(&error).is_some());
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
async fn non_zero_exits_can_be_allowed() {
    let mut input = sh("printf boom >&2; exit 2");
    input.allow_non_zero_exit = true;
    let result = VcsProcess::new().run(input).await.unwrap();
    assert_eq!((result.exit_code, result.stderr.as_str()), (2, "boom"));
}

#[tokio::test]
async fn a_signalled_process_has_no_exit_code() {
    let error = VcsProcess::new().run(sh("kill -9 $$")).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessMissingExitCodeError");
}

#[tokio::test]
async fn truncation_keeps_the_prefix_and_can_append_a_marker() {
    let script = "head -c 2048 /dev/zero | tr '\\0' x";
    let mut input = sh(script);
    input.max_output_bytes = Some(128);
    input.append_truncation_marker = true;
    let result = VcsProcess::new().run(input).await.unwrap();
    assert!(result.stdout_truncated && !result.stderr_truncated);
    assert_eq!(result.stdout, format!("{}\n\n[truncated]", "x".repeat(128)));

    let mut input = sh(script);
    input.max_output_bytes = Some(128);
    let result = VcsProcess::new().run(input).await.unwrap();
    assert!(result.stdout_truncated);
    assert_eq!(result.stdout, "x".repeat(128));
}

#[tokio::test]
async fn output_exactly_at_the_limit_is_not_truncated() {
    let mut input = sh("head -c 128 /dev/zero | tr '\\0' x");
    input.max_output_bytes = Some(128);
    let result = VcsProcess::new().run(input).await.unwrap();
    assert!(!result.stdout_truncated);
    assert_eq!(result.stdout.len(), 128);
}

#[tokio::test]
async fn error_mode_fails_with_measured_byte_counts() {
    let mut input = sh("head -c 2048 /dev/zero | tr '\\0' x");
    input.max_output_bytes = Some(128);
    input.output_mode = Some(OutputMode::Error);
    let error = VcsProcess::new().run(input).await.unwrap_err();
    let VcsError::ProcessOutputLimit {
        stream,
        max_bytes,
        observed_bytes,
        ..
    } = error
    else {
        panic!("expected an output limit error, got {error:?}");
    };
    assert_eq!(stream.as_str(), "stdout");
    assert_eq!(max_bytes, 128);
    assert!(observed_bytes > max_bytes);
}

#[tokio::test]
async fn invalid_utf8_is_decoded_lossily_and_flagged() {
    let result = VcsProcess::new().run(sh(r"printf 'a\377b'")).await.unwrap();
    assert_eq!(result.stdout, "a\u{fffd}b");
    assert!(result.stdout_invalid_utf8 && !result.stderr_invalid_utf8);
}

#[tokio::test]
async fn streams_every_stdout_byte_beyond_the_buffered_cap() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let mut input = sh("head -c 131072 /dev/zero | tr '\\0' x; printf '\\0S final\\0'");
    input.max_output_bytes = Some(8);
    input.on_stdout_chunk = Some(Arc::new(move |chunk| {
        sink.lock().unwrap().extend_from_slice(chunk)
    }));
    let result = VcsProcess::new().run(input).await.unwrap();
    assert_eq!(result.stdout, "xxxxxxxx");
    assert!(result.stdout_truncated);
    let mut expected = vec![b'x'; 131_072];
    expected.extend_from_slice(b"\0S final\0");
    assert_eq!(*received.lock().unwrap(), expected);
}

#[tokio::test]
async fn a_timeout_fails_and_ends_the_child() {
    let marker = tempfile::tempdir().unwrap();
    let flag = marker.path().join("survived");
    let mut input = sh(&format!("sleep 5; touch {}", flag.display()));
    input.timeout = Some(Duration::from_millis(50));
    let error = VcsProcess::new().run(input).await.unwrap_err();
    assert!(
        matches!(&error, VcsError::ProcessTimeout { timeout_ms: 50, .. }),
        "{error:?}"
    );
    assert!(!flag.exists());
}

#[tokio::test]
async fn abandoning_a_run_ends_the_children_the_process_started() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("held-open");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: creates a named pipe at a fresh path.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    // Reading the pipe ends only when every writer is gone, so the read below finishes exactly
    // when the backgrounded `sleep` (a grandchild of the run) has died.
    let (opened, writer_exists) = tokio::sync::oneshot::channel();
    let reader = tokio::task::spawn_blocking({
        let fifo = fifo.clone();
        move || {
            let mut pipe = std::fs::File::open(fifo).unwrap();
            opened.send(()).unwrap();
            std::io::Read::read_to_end(&mut pipe, &mut Vec::new()).unwrap();
        }
    });
    let process = VcsProcess::new();
    let run = process.run(sh(&format!("sleep 3600 > {} & wait", fifo.display())));
    tokio::select! {
        _ = run => panic!("the script should still be running"),
        _ = writer_exists => {}
    }
    // The run was dropped; SIGTERM went to its whole process group.
    tokio::time::timeout(HANG_BOUND, reader)
        .await
        .expect("the grandchild outlived its process group")
        .unwrap();
}

#[tokio::test]
async fn env_overrides_set_and_remove_variables() {
    let mut input = sh(r#"printf '%s|%s' "$T3_VCS_TEST_SET" "${HOME-unset}""#);
    input.env = vec![
        ("T3_VCS_TEST_SET".into(), Some("value".into())),
        ("HOME".into(), None),
    ];
    assert_eq!(
        VcsProcess::new().run(input).await.unwrap().stdout,
        "value|unset"
    );
}

#[tokio::test]
async fn spawn_cwd_decides_where_the_child_runs_while_cwd_is_only_reported() {
    let spawn_dir = tempfile::tempdir().unwrap();
    let mut input = sh("pwd -P");
    input.cwd = "/reported/only".into();
    input.spawn_cwd = Some(spawn_dir.path().to_path_buf());
    let result = VcsProcess::new().run(input).await.unwrap();
    assert_eq!(
        result.stdout.trim(),
        spawn_dir.path().canonicalize().unwrap().to_str().unwrap()
    );
}

/// Holds every run until `gate` opens, recording how many ran at once.
struct Gated {
    active: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    gate: tokio::sync::watch::Receiver<bool>,
}

impl ProcessRunner for Gated {
    fn run(&self, _: ProcessRunInput) -> BoxFuture<'_, Result<ProcessRunOutput, ProcessRunError>> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            self.total.fetch_add(1, Ordering::SeqCst);
            self.started.send(()).unwrap();
            let mut gate = self.gate.clone();
            gate.wait_for(|open| *open).await.unwrap();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ProcessRunOutput {
                code: Some(0),
                ..ProcessRunOutput::default()
            })
        })
    }
}

async fn burst(command: &str, bound: usize) {
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let (open, gate) = tokio::sync::watch::channel(false);
    let runner = Arc::new(Gated {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        total: AtomicUsize::new(0),
        started,
        gate,
    });
    let process = VcsProcess::with_runner(runner.clone());
    let tasks: Vec<_> = (0..32)
        .map(|index| {
            let process = process.clone();
            let command = command.to_string();
            tokio::spawn(async move {
                process
                    .run(VcsProcessInput::new(
                        format!("synthetic.{index}"),
                        command,
                        ["api", "user"],
                        "/workspace",
                    ))
                    .await
            })
        })
        .collect();
    for _ in 0..bound {
        starts.recv().await.unwrap();
    }
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        starts.try_recv().is_err(),
        "more than {bound} {command} processes started"
    );
    assert_eq!(runner.peak.load(Ordering::SeqCst), bound);
    open.send(true).unwrap();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(runner.total.load(Ordering::SeqCst), 32);
    assert_eq!(runner.peak.load(Ordering::SeqCst), bound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_processes_are_bounded_to_four() {
    burst("gh", 4).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_vcs_processes_are_bounded_to_eight() {
    burst("git", 8).await;
}
