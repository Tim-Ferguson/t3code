//! Running the git binary (port of `vcs/VcsProcess.ts` and the parts of `processRunner.ts` it
//! uses). `ProcessRunner` is the spawning layer; `VcsProcess` adds the VCS policy on top of it:
//! default limits, bounded concurrency, error classification, and the checkpoint-capture retry.
use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::future::BoxFuture;
use regex::Regex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};

use crate::error::{ExitFailureKind, OutputStream, ProcessContext, ReadStream, VcsError};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(30_000);
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[truncated]";
const VCS_PROCESS_CONCURRENCY: usize = 8;
const GITHUB_PROCESS_CONCURRENCY: usize = 4;
const CHECKPOINT_RETRY_TIMES: usize = 2;
const CHECKPOINT_RETRY_DELAY: Duration = Duration::from_millis(75);
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// Operation name of every git command run by checkpoint capture. `VcsProcess` retries
/// transient lock failures only for this operation.
pub const CHECKPOINT_CAPTURE_OPERATION: &str = "GitVcsDriver.checkpoints.captureCheckpoint";

/// Environment changes applied on top of the server's own environment: `Some` sets a
/// variable, `None` removes it.
pub type EnvOverrides = Vec<(String, Option<String>)>;
/// Receives every stdout chunk, including bytes beyond the buffered output limit.
pub type StdoutCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Keep the first `max_output_bytes` and flag the rest as truncated.
    #[default]
    Truncate,
    /// Fail with an output-limit error as soon as the limit is exceeded.
    Error,
}

// ---------------------------------------------------------------------------------------------
// Spawning layer
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct ProcessRunInput {
    pub command: String,
    pub args: Vec<String>,
    /// Directory the child is spawned in; `None` inherits the server's.
    pub spawn_cwd: Option<PathBuf>,
    pub stdin: Option<String>,
    pub on_stdout_chunk: Option<StdoutCallback>,
    /// Like `on_stdout_chunk`, for stderr.
    pub on_stderr_chunk: Option<StdoutCallback>,
    pub env: EnvOverrides,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub output_mode: OutputMode,
    pub truncated_marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessRunOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process ended without an exit code (killed by a signal).
    pub code: Option<i32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_invalid_utf8: bool,
    pub stderr_invalid_utf8: bool,
}

#[derive(Debug, Clone)]
pub enum ProcessRunError {
    Spawn(Arc<std::io::Error>),
    Stdin {
        stdin_bytes: usize,
        cause: Arc<std::io::Error>,
    },
    OutputLimit {
        stream: OutputStream,
        max_bytes: usize,
        observed_bytes: usize,
    },
    Read {
        stream: ReadStream,
        cause: Arc<std::io::Error>,
    },
    Timeout {
        timeout_ms: u64,
    },
}

/// The spawning seam. Production uses `TokioProcessRunner`; tests wrap it to inject failures.
pub trait ProcessRunner: Send + Sync + 'static {
    fn run(
        &self,
        input: ProcessRunInput,
    ) -> BoxFuture<'_, Result<ProcessRunOutput, ProcessRunError>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokioProcessRunner;

impl ProcessRunner for TokioProcessRunner {
    fn run(
        &self,
        input: ProcessRunInput,
    ) -> BoxFuture<'_, Result<ProcessRunOutput, ProcessRunError>> {
        Box::pin(async move {
            let timeout = input.timeout;
            // Dropping the in-flight future drops the child, and `kill_on_drop` ends it.
            match tokio::time::timeout(timeout, run_process(input)).await {
                Ok(result) => result,
                Err(_) => Err(ProcessRunError::Timeout {
                    timeout_ms: timeout.as_millis() as u64,
                }),
            }
        })
    }
}

struct Collected {
    text: String,
    truncated: bool,
    invalid_utf8: bool,
}

async fn collect<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: OutputStream,
    input: &ProcessRunInput,
    callback: Option<&StdoutCallback>,
) -> Result<Collected, ProcessRunError> {
    let max = input.max_output_bytes;
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|cause| ProcessRunError::Read {
                stream: stream.into(),
                cause: Arc::new(cause),
            })?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        if let Some(callback) = callback {
            callback(chunk);
        }
        let remaining = max.saturating_sub(bytes.len());
        match input.output_mode {
            OutputMode::Error => {
                if chunk.len() > remaining {
                    return Err(ProcessRunError::OutputLimit {
                        stream,
                        max_bytes: max,
                        observed_bytes: bytes.len() + chunk.len(),
                    });
                }
                bytes.extend_from_slice(chunk);
            }
            // Keep draining after truncation so the child can exit normally.
            OutputMode::Truncate => {
                if truncated {
                    continue;
                }
                if remaining == 0 {
                    truncated = true;
                    continue;
                }
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                truncated = chunk.len() > remaining;
            }
        }
    }
    let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(&input.truncated_marker);
    }
    Ok(Collected {
        text,
        truncated,
        invalid_utf8,
    })
}

/// The child leads its own process group, and the group is sent SIGTERM when the run is
/// abandoned (timeout, output limit, cancellation), so hooks and filters git started end with
/// it. Disarmed once the child has exited normally.
struct GroupKill {
    #[cfg(unix)]
    pid: Option<i32>,
}

impl GroupKill {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(unix)]
        return Self {
            pid: pid.and_then(|pid| i32::try_from(pid).ok()),
        };
        #[cfg(not(unix))]
        {
            let _ = pid;
            Self {}
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pid = None;
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: plain signal delivery to the process group this run created.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
    }
}

async fn run_process(input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
    let mut command = Command::new(&input.command);
    command
        .args(&input.args)
        .stdin(if input.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    if let Some(cwd) = &input.spawn_cwd {
        command.current_dir(cwd);
    }
    for (name, value) in &input.env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    let mut child = command
        .spawn()
        .map_err(|cause| ProcessRunError::Spawn(Arc::new(cause)))?;
    let mut group = GroupKill::new(child.id());
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let write_stdin = async {
        if let (Some(mut pipe), Some(text)) = (stdin, input.stdin.as_deref()) {
            let stdin_error = |cause| ProcessRunError::Stdin {
                stdin_bytes: text.len(),
                cause: Arc::new(cause),
            };
            pipe.write_all(text.as_bytes()).await.map_err(stdin_error)?;
            pipe.shutdown().await.map_err(stdin_error)?;
        }
        Ok(())
    };
    let (stdout, stderr, ()) = tokio::try_join!(
        collect(
            stdout,
            OutputStream::Stdout,
            &input,
            input.on_stdout_chunk.as_ref()
        ),
        collect(
            stderr,
            OutputStream::Stderr,
            &input,
            input.on_stderr_chunk.as_ref()
        ),
        write_stdin,
    )?;
    let status = child.wait().await.map_err(|cause| ProcessRunError::Read {
        stream: ReadStream::ExitCode,
        cause: Arc::new(cause),
    })?;
    group.disarm();
    Ok(ProcessRunOutput {
        stdout: stdout.text,
        stderr: stderr.text,
        code: status.code(),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_invalid_utf8: stdout.invalid_utf8,
        stderr_invalid_utf8: stderr.invalid_utf8,
    })
}

// ---------------------------------------------------------------------------------------------
// VCS policy layer
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct VcsProcessInput {
    pub operation: String,
    pub command: String,
    pub args: Vec<String>,
    /// Reported in errors. Also the spawn directory unless `spawn_cwd` is set.
    pub cwd: PathBuf,
    pub spawn_cwd: Option<PathBuf>,
    pub stdin: Option<String>,
    pub on_stdout_chunk: Option<StdoutCallback>,
    pub env: EnvOverrides,
    pub allow_non_zero_exit: bool,
    pub timeout: Option<Duration>,
    pub max_output_bytes: Option<usize>,
    pub output_mode: Option<OutputMode>,
    pub append_truncation_marker: bool,
}

impl VcsProcessInput {
    pub fn new(
        operation: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            operation: operation.into(),
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            spawn_cwd: None,
            stdin: None,
            on_stdout_chunk: None,
            env: Vec::new(),
            allow_non_zero_exit: false,
            timeout: None,
            max_output_bytes: None,
            output_mode: None,
            append_truncation_marker: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VcsProcessOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_invalid_utf8: bool,
    pub stderr_invalid_utf8: bool,
}

struct Shared {
    runner: Arc<dyn ProcessRunner>,
    vcs: Semaphore,
    github: Semaphore,
    retry_delay: Duration,
}

/// Cheap to clone; clones share one concurrency budget.
#[derive(Clone)]
pub struct VcsProcess {
    shared: Arc<Shared>,
}

impl Default for VcsProcess {
    fn default() -> Self {
        Self::with_runner(Arc::new(TokioProcessRunner))
    }
}

impl VcsProcess {
    pub fn new() -> Self {
        Self::default()
    }

    /// The spawning layer this process runs on, for drivers that share it.
    pub fn runner(&self) -> Arc<dyn ProcessRunner> {
        self.shared.runner.clone()
    }

    pub fn with_runner(runner: Arc<dyn ProcessRunner>) -> Self {
        Self::build(runner, CHECKPOINT_RETRY_DELAY)
    }

    /// Replaces the pause between checkpoint-capture retries (75ms by default).
    pub fn with_retry_delay(self, retry_delay: Duration) -> Self {
        Self::build(self.shared.runner.clone(), retry_delay)
    }

    fn build(runner: Arc<dyn ProcessRunner>, retry_delay: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                runner,
                vcs: Semaphore::new(VCS_PROCESS_CONCURRENCY),
                github: Semaphore::new(GITHUB_PROCESS_CONCURRENCY),
                retry_delay,
            }),
        }
    }

    pub async fn run(&self, input: VcsProcessInput) -> Result<VcsProcessOutput, VcsError> {
        if input.command == "git"
            && input.operation == CHECKPOINT_CAPTURE_OPERATION
            && input.on_stdout_chunk.is_none()
        {
            // Retry the failed command, keeping the private index and the caller's outer deadline.
            let mut attempt = 0;
            loop {
                match self.run_bounded(&input).await {
                    Err(error) if attempt < CHECKPOINT_RETRY_TIMES && error.is_retryable_exit() => {
                        attempt += 1;
                        if !self.shared.retry_delay.is_zero() {
                            tokio::time::sleep(self.shared.retry_delay).await;
                        }
                    }
                    result => return result,
                }
            }
        }
        // The permit is held for the whole call so GitHub processes are also VCS-bounded.
        let _github = if input.command == "gh" {
            Some(
                self.shared
                    .github
                    .acquire()
                    .await
                    .expect("semaphore is never closed"),
            )
        } else {
            None
        };
        self.run_bounded(&input).await
    }

    async fn run_bounded(&self, input: &VcsProcessInput) -> Result<VcsProcessOutput, VcsError> {
        let _permit = self
            .shared
            .vcs
            .acquire()
            .await
            .expect("semaphore is never closed");
        self.run_unbounded(input).await
    }

    async fn run_unbounded(&self, input: &VcsProcessInput) -> Result<VcsProcessOutput, VcsError> {
        let context = ProcessContext {
            operation: input.operation.clone(),
            command: input.command.clone(),
            cwd: input.cwd.to_string_lossy().into_owned(),
            argument_count: Some(input.args.len()),
        };
        let spawn_cwd = input.spawn_cwd.clone().unwrap_or_else(|| input.cwd.clone());
        let result = self
            .shared
            .runner
            .run(ProcessRunInput {
                command: input.command.clone(),
                args: input.args.clone(),
                spawn_cwd: Some(spawn_cwd),
                stdin: input.stdin.clone(),
                on_stdout_chunk: input.on_stdout_chunk.clone(),
                on_stderr_chunk: None,
                env: input.env.clone(),
                timeout: input.timeout.unwrap_or(DEFAULT_TIMEOUT),
                max_output_bytes: input.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
                output_mode: input.output_mode.unwrap_or_default(),
                truncated_marker: if input.append_truncation_marker {
                    OUTPUT_TRUNCATED_MARKER.into()
                } else {
                    String::new()
                },
            })
            .await
            .map_err(|error| map_run_error(context.clone(), error))?;

        let Some(code) = result.code else {
            return Err(VcsError::ProcessMissingExitCode { context });
        };
        if !input.allow_non_zero_exit && code != 0 {
            let failure_kind = classify_non_zero_exit(&input.command, &result.stderr);
            let retryable = input.command == "git"
                && failure_kind == ExitFailureKind::CommandFailed
                && is_transient_git_exit(&result.stderr);
            return Err(exit_error(
                context,
                code,
                &result.stderr,
                result.stderr_truncated,
                failure_kind,
                retryable,
            ));
        }
        Ok(VcsProcessOutput {
            exit_code: code,
            stdout: result.stdout,
            stderr: result.stderr,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
            stdout_invalid_utf8: result.stdout_invalid_utf8,
            stderr_invalid_utf8: result.stderr_invalid_utf8,
        })
    }
}

fn map_run_error(context: ProcessContext, error: ProcessRunError) -> VcsError {
    match error {
        ProcessRunError::Spawn(cause) => VcsError::ProcessSpawn { context, cause },
        ProcessRunError::OutputLimit {
            stream,
            max_bytes,
            observed_bytes,
        } => VcsError::ProcessOutputLimit {
            context,
            stream,
            max_bytes,
            observed_bytes,
        },
        ProcessRunError::Timeout { timeout_ms } => VcsError::ProcessTimeout {
            context,
            timeout_ms,
        },
        ProcessRunError::Stdin { stdin_bytes, cause } => VcsError::ProcessStdinWrite {
            context,
            stdin_bytes,
            cause,
        },
        ProcessRunError::Read { stream, cause } => VcsError::ProcessOutputRead {
            context,
            stream,
            cause,
        },
    }
}

/// Mirrors `VcsProcessExitError.fromProcessExit`: the detail is a fixed sentence per failure
/// kind, so stderr (which may carry paths or credentials) never reaches the error.
fn exit_error(
    context: ProcessContext,
    exit_code: i32,
    stderr: &str,
    stderr_truncated: bool,
    failure_kind: ExitFailureKind,
    retryable: bool,
) -> VcsError {
    let detail = match failure_kind {
        ExitFailureKind::Authentication => "Authentication failed.",
        ExitFailureKind::RateLimited => "API rate limit exceeded.",
        ExitFailureKind::NotFound => match context.command.as_str() {
            "glab" => "Merge request not found.",
            "gh" | "az" => "Pull request not found.",
            _ => "VCS resource not found.",
        },
        ExitFailureKind::CommandFailed => "Process exited with a non-zero status.",
    };
    VcsError::ProcessExit {
        context,
        exit_code,
        detail: detail.into(),
        failure_kind: Some(failure_kind),
        retryable,
        stderr_length: Some(stderr.encode_utf16().count()),
        stderr_truncated: Some(stderr_truncated),
    }
}

/// Exposed for fixture parity tests. `command` is the executable name as given.
pub fn classify_non_zero_exit(command: &str, stderr: &str) -> ExitFailureKind {
    let normalized = stderr.to_lowercase();
    let has = |needle: &str| normalized.contains(needle);

    if has("authentication failed")
        || has("not logged in")
        || has("gh auth login")
        || has("glab auth login")
        || has("az devops login")
        || has("please run az login")
        || has("no oauth token")
        || has("unauthorized")
    {
        return ExitFailureKind::Authentication;
    }
    if has("api rate limit")
        || has("rate limit exceeded")
        || has("secondary rate limit")
        || has("too many requests")
        || has("http 429")
    {
        return ExitFailureKind::RateLimited;
    }
    if (command == "gh"
        && (has("could not resolve to a pullrequest")
            || has("repository.pullrequest")
            || has("no pull requests found for branch")
            || has("pull request not found")))
        || (command == "glab" && (has("merge request not found") || has("not found") || has("404")))
        || (command == "az" && has("pull request") && (has("not found") || has("does not exist")))
    {
        return ExitFailureKind::NotFound;
    }
    ExitFailureKind::CommandFailed
}

/// A recognized transient git failure (a lock another git process holds, or a file that
/// vanished mid-scan). Classified before stderr is discarded.
pub fn is_transient_git_exit(stderr: &str) -> bool {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<[Regex; 2]> = OnceLock::new();
    let [lock, vanished] = PATTERNS.get_or_init(|| {
        [
            Regex::new(r#"(?i)unable to create [^\n]*\.lock['"]?: file exists"#).unwrap(),
            Regex::new(
                r"(?i)(?:unable to stat|lstat\(|error: open\()[^\n]+: no such file or directory",
            )
            .unwrap(),
        ]
    });
    lock.is_match(stderr) || vanished.is_match(stderr)
}
