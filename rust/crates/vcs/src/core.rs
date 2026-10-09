//! Branch, worktree and ref operations of the git driver (the simpler half of
//! `vcs/GitVcsDriverCore.ts`). Failures are `GitCommandError`s, which carry a classified
//! `reason` instead of git's stderr, as on the wire.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use regex::Regex;
use tokio::sync::Semaphore;

use crate::{
    clock::{Clock, SystemClock},
    process::{OutputMode, ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner},
    progress::{GitProgress, LineStream, Trace2Monitor},
    state::{CoreState, GitTimings},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(30_000);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;
const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[truncated]";
const GIT_PROCESS_CONCURRENCY: usize = 8;
// `git worktree add` checks out the full tree, so on large repositories it can take well
// beyond the default 30s. Generous headroom while still bounding a genuinely hung git.
const WORKTREE_ADD_TIMEOUT: Duration = Duration::from_secs(300);
const WORKTREE_REMOVE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCommandFailureReason {
    AuthenticationFailed,
    BranchAlreadyExists,
    BranchCheckedOutInWorktree,
    HostKeyUnverified,
    NotARepository,
    PathAlreadyExists,
    RemoteUnreachable,
    TagWouldBeClobbered,
}

impl GitCommandFailureReason {
    /// The wire value (`reason`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthenticationFailed => "authentication_failed",
            Self::BranchAlreadyExists => "branch_already_exists",
            Self::BranchCheckedOutInWorktree => "branch_checked_out_in_worktree",
            Self::HostKeyUnverified => "host_key_unverified",
            Self::NotARepository => "not_a_repository",
            Self::PathAlreadyExists => "path_already_exists",
            Self::RemoteUnreachable => "remote_unreachable",
            Self::TagWouldBeClobbered => "tag_would_be_clobbered",
        }
    }
}

/// `GitCommandError` of `packages/contracts/src/git.ts`. Lengths are UTF-16 code units, as
/// JavaScript reports them; git's stderr itself never enters the error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Git command failed in {operation} ({cwd}): {detail}{}", .reason.map(|reason| format!(" ({})", reason.as_str())).unwrap_or_default())]
pub struct GitCommandError {
    pub operation: String,
    pub command: String,
    pub cwd: String,
    pub argument_count: Option<usize>,
    pub exit_code: Option<i32>,
    pub stdout_length: Option<usize>,
    pub stderr_length: Option<usize>,
    pub output_length: Option<usize>,
    pub reason: Option<GitCommandFailureReason>,
    pub detail: String,
    #[source]
    pub cause: Option<Arc<std::io::Error>>,
}

impl GitCommandError {
    pub fn new(operation: &str, cwd: &Path, args: &[String], detail: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            command: "git".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            argument_count: Some(args.len()),
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            output_length: None,
            reason: None,
            detail: detail.into(),
            cause: None,
        }
    }
}

pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

// Git states the actual cause on stderr, but stderr never leaves this module: it echoes argv
// and remote URLs, which can carry credentials. Matching it against fixed patterns yields a
// tag the caller can act on and a message that quotes none of the matched text. Patterns run
// in order, specific first.
fn failure_reason_patterns() -> &'static [(Regex, GitCommandFailureReason)] {
    use GitCommandFailureReason::*;
    static PATTERNS: OnceLock<Vec<(Regex, GitCommandFailureReason)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            (r"(?i)would clobber existing tag", TagWouldBeClobbered),
            (r"(?i)is already (?:used by worktree at|checked out at)", BranchCheckedOutInWorktree),
            (r"(?i)a branch named .+ already exists", BranchAlreadyExists),
            (
                r"(?i)(?:authentication failed|could not read Username|could not read Password|permission denied \([a-z-]+(?:,[a-z-]+)*\)|permission denied, please try again)",
                AuthenticationFailed,
            ),
            (r"(?i)host key verification failed", HostKeyUnverified),
            (
                r"(?i)(?:could not read from remote repository|does not appear to be a git repository|repository .+ not found)",
                RemoteUnreachable,
            ),
            (r"(?i)fatal: '[^']+' already exists|destination path .+ already exists", PathAlreadyExists),
        ]
        .into_iter()
        .map(|(pattern, reason)| (Regex::new(pattern).unwrap(), reason))
        .collect()
    })
}

/// Only git's own diagnostic lines (and the local ssh's refusals) are classified; hooks and
/// remote output share the stream and must not be mistaken for git's verdict.
pub fn classify_git_failure(stderr: &str) -> Option<GitCommandFailureReason> {
    static LINES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [diagnostic, ssh_refusal, remote_prefix] = LINES.get_or_init(|| {
        [
            Regex::new(r"^(?:fatal|error):|^!\s|^\s+!\s").unwrap(),
            Regex::new(
                r"(?i)Permission denied \((?:publickey|password|keyboard-interactive)|Permission denied, please try again|Host key verification failed",
            )
            .unwrap(),
            Regex::new(r"^remote:").unwrap(),
        ]
    });
    let diagnostics: Vec<&str> = stderr
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| {
            diagnostic.is_match(line)
                || (!remote_prefix.is_match(line) && ssh_refusal.is_match(line))
        })
        .collect();
    if diagnostics.is_empty() {
        return None;
    }
    let diagnostics = diagnostics.join("\n");
    if is_non_repository_stderr(&diagnostics) {
        return Some(GitCommandFailureReason::NotARepository);
    }
    failure_reason_patterns()
        .iter()
        .find(|(pattern, _)| pattern.is_match(&diagnostics))
        .map(|(_, reason)| *reason)
}

pub fn is_non_repository_stderr(stderr: &str) -> bool {
    stderr.to_lowercase().contains("not a git repository")
}

pub fn is_unborn_head_stderr(stderr: &str) -> bool {
    let normalized = stderr.to_lowercase();
    normalized.contains("bad revision 'head'")
        || (normalized.contains("unknown revision")
            && normalized.contains("path not in the working tree"))
}

/// `git worktree remove` on a path git no longer tracks: "is not a working tree" when the
/// registration is gone, "cannot remove working tree" when older gits fail validation on a
/// registered-but-deleted directory.
pub fn is_missing_worktree_stderr(stderr: &str) -> bool {
    let normalized = stderr.to_lowercase();
    normalized.contains("is not a working tree")
        || normalized.contains("cannot remove working tree")
}

/// The local branch tracking `upstream_ref`, from `%(refname:short)\t%(upstream:short)` lines.
pub fn parse_tracking_branch_by_upstream_ref(stdout: &str, upstream_ref: &str) -> Option<String> {
    for line in stdout.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let branch = fields.next().unwrap_or("").trim();
        let candidate = fields.next().unwrap_or("").trim();
        if branch.is_empty() || candidate.is_empty() {
            continue;
        }
        if candidate == upstream_ref {
            return Some(branch.to_string());
        }
    }
    None
}

/// `origin/feature` -> `feature`; `None` when there is no remote part or no branch part.
pub fn derive_local_branch_name_from_remote_ref(branch_name: &str) -> Option<String> {
    let separator = branch_name.find('/')?;
    if separator == 0 || separator == branch_name.len() - 1 {
        return None;
    }
    let local = branch_name[separator + 1..].trim();
    (!local.is_empty()).then(|| local.to_string())
}

/// Percent and counts from `Updating files:  78% (2104/2700)` on git's progress stream.
pub fn parse_git_checkout_progress_line(line: &str) -> Option<(u32, u64, u64)> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let captures = PATTERN
        .get_or_init(|| Regex::new(r"Updating files:\s+(\d+)%\s+\((\d+)/(\d+)\)").unwrap())
        .captures(line)?;
    let percent: u64 = captures[1].parse().ok()?;
    Some((
        percent.min(100) as u32,
        captures[2].parse().ok()?,
        captures[3].parse().ok()?,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitTimeout {
    /// 30 seconds, and a place in the shared process queue.
    #[default]
    Default,
    /// For long-running commands; runs outside the queue.
    Unlimited,
    After(Duration),
}

#[derive(Clone, Default)]
pub struct ExecuteGitInput {
    pub operation: String,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, Option<String>)>,
    pub allow_non_zero_exit: bool,
    pub timeout: GitTimeout,
    pub max_output_bytes: Option<usize>,
    /// Past `max_output_bytes`, keep what fits and flag the output truncated, instead of
    /// failing with an `outputLength` error.
    pub append_truncation_marker: bool,
    pub progress: Option<GitProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteGitResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Options of the helpers below. `fallback_error_detail` replaces the generic non-zero-exit
/// detail of the error they raise.
#[derive(Clone, Default)]
pub struct GitOptions {
    pub stdin: Option<String>,
    pub env: Vec<(String, Option<String>)>,
    pub timeout: GitTimeout,
    pub allow_non_zero_exit: bool,
    pub fallback_error_detail: Option<String>,
    pub max_output_bytes: Option<usize>,
    pub append_truncation_marker: bool,
    pub progress: Option<GitProgress>,
}

impl GitOptions {
    pub(crate) fn timeout(timeout: Duration) -> Self {
        Self {
            timeout: GitTimeout::After(timeout),
            ..Self::default()
        }
    }

    pub(crate) fn allow_failure(timeout: Duration) -> Self {
        Self {
            allow_non_zero_exit: true,
            ..Self::timeout(timeout)
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.fallback_error_detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct VcsCreateWorktreeInput {
    pub cwd: PathBuf,
    pub ref_name: String,
    pub new_ref_name: Option<String>,
    pub base_ref_name: Option<String>,
    pub path: Option<PathBuf>,
}

/// What `createWorktree` does after the checkout when the new checkout has `.gitmodules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeSubmodules {
    None,
    TopLevel,
    Recursive,
}

/// Git's `Updating files: 78% (2104/2700)` during checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckoutProgress {
    pub percent: u32,
    pub completed: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmodulesFinished {
    pub ok: bool,
    pub detail: Option<String>,
}

/// Progress callbacks of `create_worktree` (`CreateWorktreeProgress`), for the worktree setup
/// card. They run on the task reading git's output and must not block.
#[derive(Clone, Default)]
pub struct CreateWorktreeProgress {
    /// Fires once `git worktree add` has created and registered the directory, before the
    /// (possibly long) submodule step. Git refuses an existing path, so a path reported here
    /// belongs to this call and is safe to remove on cancel.
    pub on_worktree_claimed: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    pub on_checkout_progress: Option<Arc<dyn Fn(CheckoutProgress) + Send + Sync>>,
    pub on_submodules_started: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_submodule_line: Option<crate::progress::LineCallback>,
    pub on_submodules_finished: Option<Arc<dyn Fn(&SubmodulesFinished) + Send + Sync>>,
}

#[derive(Clone)]
pub struct CreateWorktreeOptions {
    /// Parent of `<repo name>/<branch>` when the input has no explicit path. Resolved by the
    /// caller from the `worktreesDirectory` setting or the server's default.
    pub worktrees_directory: PathBuf,
    /// Already resolved by the caller from settings and the checkout's `t3.json`.
    pub submodules: WorktreeSubmodules,
    pub progress: Option<CreateWorktreeProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsWorktree {
    pub path: PathBuf,
    pub ref_name: String,
}

/// Git operations that report `GitCommandError`. Cheap to clone; clones share one queue and
/// one set of caches (see `GitTimings`).
#[derive(Clone)]
pub struct GitCore {
    runner: Arc<dyn ProcessRunner>,
    queue: Arc<Semaphore>,
    pub(crate) state: Arc<CoreState>,
}

impl GitCore {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self::with_clock(runner, Arc::new(SystemClock), GitTimings::default())
    }

    /// A core whose caches and throttles run on `clock` with the given lifetimes.
    pub fn with_clock(
        runner: Arc<dyn ProcessRunner>,
        clock: Arc<dyn Clock>,
        timings: GitTimings,
    ) -> Self {
        Self {
            runner,
            queue: Arc::new(Semaphore::new(GIT_PROCESS_CONCURRENCY)),
            state: Arc::new(CoreState::new(clock, timings)),
        }
    }

    /// Runs git in `cwd`. Non-zero exits are returned, not raised, unless `allow_non_zero_exit`
    /// is unset, in which case they become a classified `GitCommandError`.
    pub async fn execute(
        &self,
        input: ExecuteGitInput,
    ) -> Result<ExecuteGitResult, GitCommandError> {
        let timeout = match input.timeout {
            GitTimeout::Default => Some(DEFAULT_TIMEOUT),
            GitTimeout::Unlimited => None,
            GitTimeout::After(timeout) => Some(timeout),
        };
        // Only commands within the default budget queue; long ones must not starve it.
        let queued = timeout.is_some_and(|timeout| timeout <= DEFAULT_TIMEOUT);
        let _permit = if queued {
            Some(
                self.queue
                    .acquire()
                    .await
                    .expect("semaphore is never closed"),
            )
        } else {
            None
        };

        let fail =
            |detail: &str| GitCommandError::new(&input.operation, &input.cwd, &input.args, detail);
        let max_output_bytes = input.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        // Status polling runs beside the user's own git commands; without this,
        // `git status` takes index.lock to save its refreshed index.
        let mut env = vec![("GIT_OPTIONAL_LOCKS".to_string(), Some("0".to_string()))];
        env.extend(input.env.clone());
        let progress = input.progress.clone().unwrap_or_default();
        let keep_after_cap = progress.keep_line_callbacks_after_truncation;
        let stdout_lines = progress
            .on_stdout_line
            .clone()
            .map(|callback| LineStream::new(callback, max_output_bytes, keep_after_cap));
        let stderr_lines = progress
            .on_stderr_line
            .clone()
            .map(|callback| LineStream::new(callback, max_output_bytes, keep_after_cap));
        let trace = progress
            .wants_hooks()
            .then(|| Trace2Monitor::start(progress));
        if let Some(trace) = &trace {
            env.push((
                "GIT_TRACE2_EVENT".into(),
                Some(trace.path().to_string_lossy().into_owned()),
            ));
        }
        let run = self.runner.run(ProcessRunInput {
            command: "git".into(),
            args: input.args.clone(),
            spawn_cwd: Some(input.cwd.clone()),
            stdin: input.stdin.clone(),
            on_stdout_chunk: stdout_lines.as_ref().map(LineStream::chunk_callback),
            on_stderr_chunk: stderr_lines.as_ref().map(LineStream::chunk_callback),
            env,
            timeout: timeout.unwrap_or(Duration::from_secs(60 * 60 * 24 * 365)),
            max_output_bytes,
            output_mode: if input.append_truncation_marker {
                OutputMode::Truncate
            } else {
                OutputMode::Error
            },
            truncated_marker: String::new(),
        });
        let result = run.await;
        for lines in [&stdout_lines, &stderr_lines].into_iter().flatten() {
            lines.finish();
        }
        if let Some(trace) = &trace {
            trace.finish();
        }
        let output: ProcessRunOutput = result.map_err(|error| match error {
            ProcessRunError::Spawn(cause) => GitCommandError {
                cause: Some(cause),
                ..fail("Failed to spawn Git process.")
            },
            ProcessRunError::Timeout { .. } => fail("Git command timed out."),
            ProcessRunError::Stdin { cause, .. } => GitCommandError {
                cause: Some(cause),
                ..fail("Failed to write Git process input.")
            },
            ProcessRunError::Read { stream, cause } => GitCommandError {
                cause: Some(cause),
                ..fail(&format!("Failed to read Git process {}.", stream.as_str()))
            },
            ProcessRunError::OutputLimit {
                max_bytes,
                observed_bytes,
                ..
            } => GitCommandError {
                output_length: Some(observed_bytes),
                ..fail(&format!(
                    "Git output exceeded {max_bytes} bytes and was truncated."
                ))
            },
        })?;
        let Some(exit_code) = output.code else {
            return Err(fail("Failed to read Git process exit code."));
        };
        if !input.allow_non_zero_exit && exit_code != 0 {
            return Err(GitCommandError {
                reason: classify_git_failure(&output.stderr),
                exit_code: Some(exit_code),
                stdout_length: Some(utf16_len(&output.stdout)),
                stderr_length: Some(utf16_len(&output.stderr)),
                ..fail("Git command exited with a non-zero status.")
            });
        }
        Ok(ExecuteGitResult {
            exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
        })
    }

    /// Runs git with the helpers' error mapping; see `GitOptions`. Boxed so that the many
    /// commands an operation awaits do not each widen its future.
    pub(crate) fn run<'a>(
        &'a self,
        operation: &'a str,
        cwd: &'a Path,
        args: impl IntoIterator<Item = impl Into<String>>,
        options: GitOptions,
    ) -> futures_util::future::BoxFuture<'a, Result<ExecuteGitResult, GitCommandError>> {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        Box::pin(self.git(operation, cwd, args, options))
    }

    async fn git(
        &self,
        operation: &str,
        cwd: &Path,
        args: impl IntoIterator<Item = impl Into<String>>,
        options: GitOptions,
    ) -> Result<ExecuteGitResult, GitCommandError> {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let result = self
            .execute(ExecuteGitInput {
                operation: operation.into(),
                cwd: cwd.to_path_buf(),
                args: args.clone(),
                stdin: options.stdin,
                env: options.env,
                allow_non_zero_exit: true,
                timeout: options.timeout,
                max_output_bytes: options.max_output_bytes,
                append_truncation_marker: options.append_truncation_marker,
                progress: options.progress,
            })
            .await?;
        if options.allow_non_zero_exit || result.exit_code == 0 {
            return Ok(result);
        }
        Err(GitCommandError {
            reason: classify_git_failure(&result.stderr),
            exit_code: Some(result.exit_code),
            stdout_length: Some(utf16_len(&result.stdout)),
            stderr_length: Some(utf16_len(&result.stderr)),
            ..GitCommandError::new(
                operation,
                cwd,
                &args,
                options
                    .fallback_error_detail
                    .as_deref()
                    .unwrap_or("Git command exited with a non-zero status."),
            )
        })
    }

    /// Stdout, with the truncation marker appended when it was cut at the output cap
    /// (`runGitStdoutWithOptions`).
    pub(crate) async fn stdout_with_marker(
        &self,
        operation: &str,
        cwd: &Path,
        args: impl IntoIterator<Item = impl Into<String>>,
        options: GitOptions,
    ) -> Result<String, GitCommandError> {
        let result = self.run(operation, cwd, args, options).await?;
        Ok(if result.stdout_truncated {
            format!("{}{OUTPUT_TRUNCATED_MARKER}", result.stdout)
        } else {
            result.stdout
        })
    }

    pub(crate) async fn stdout(
        &self,
        operation: &str,
        cwd: &Path,
        args: &[&str],
        allow_failure: bool,
    ) -> Result<String, GitCommandError> {
        let options = GitOptions {
            allow_non_zero_exit: allow_failure,
            ..GitOptions::default()
        };
        Ok(self
            .run(operation, cwd, args.iter().copied(), options)
            .await?
            .stdout)
    }

    /// `git config --get <key>`: `None` when unset.
    pub async fn read_config_value(
        &self,
        cwd: &Path,
        key: &str,
    ) -> Result<Option<String>, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.readConfigValue",
                cwd,
                &["config", "--get", key],
                true,
            )
            .await?;
        let value = stdout.trim();
        Ok((!value.is_empty()).then(|| value.to_string()))
    }

    pub async fn list_local_branch_names(
        &self,
        cwd: &Path,
    ) -> Result<Vec<String>, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.listLocalBranchNames",
                cwd,
                &[
                    "branch",
                    "--list",
                    "--no-column",
                    "--format=%(refname:short)",
                ],
                false,
            )
            .await?;
        Ok(stdout
            .split('\n')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect())
    }

    /// Absolute paths of every live worktree of the repository at `cwd`, the main checkout
    /// included. Worktrees whose directory is gone (`prunable`) are left out.
    pub async fn list_worktree_paths(&self, cwd: &Path) -> Result<Vec<PathBuf>, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.listWorktreePaths",
                cwd,
                &["worktree", "list", "--porcelain", "-z"],
                false,
            )
            .await?;
        // One record per worktree, each ended by an empty field. A `prunable` record's
        // directory is gone, and another checkout may now sit there.
        Ok(stdout
            .split("\0\0")
            .filter_map(|record| {
                let fields: Vec<&str> = record.split('\0').collect();
                let worktree = fields
                    .iter()
                    .find_map(|field| field.strip_prefix("worktree "))?;
                if fields.iter().any(|field| field.starts_with("prunable")) {
                    return None;
                }
                Some(cwd.join(worktree))
            })
            .collect())
    }

    pub async fn resolve_commit(
        &self,
        cwd: &Path,
        revision: &str,
    ) -> Result<String, GitCommandError> {
        let revision = format!("{revision}^{{commit}}");
        let stdout = self
            .stdout(
                "GitVcsDriver.resolveCommit",
                cwd,
                &["rev-parse", "--verify", &revision],
                false,
            )
            .await?;
        Ok(stdout.trim().to_string())
    }

    async fn ref_exists(
        &self,
        operation: &str,
        cwd: &Path,
        full_ref: &str,
    ) -> Result<bool, GitCommandError> {
        let result = self
            .git(
                operation,
                cwd,
                ["show-ref", "--verify", "--quiet", full_ref],
                GitOptions::allow_failure(Duration::from_secs(5)),
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    pub async fn branch_exists(&self, cwd: &Path, ref_name: &str) -> Result<bool, GitCommandError> {
        self.ref_exists(
            "GitVcsDriver.branchExists",
            cwd,
            &format!("refs/heads/{ref_name}"),
        )
        .await
    }

    /// `desired`, or `desired-1` .. `desired-100` when taken.
    async fn resolve_available_branch_name(
        &self,
        cwd: &Path,
        desired: &str,
    ) -> Result<String, GitCommandError> {
        if !self.branch_exists(cwd, desired).await? {
            return Ok(desired.to_string());
        }
        for suffix in 1..=100 {
            let candidate = format!("{desired}-{suffix}");
            if !self.branch_exists(cwd, &candidate).await? {
                return Ok(candidate);
            }
        }
        let args = ["branch", "-m", "--", desired].map(String::from);
        Err(GitCommandError::new(
            "GitVcsDriver.renameBranch",
            cwd,
            &args,
            format!("Could not find an available branch name for '{desired}'."),
        ))
    }

    /// Renames a branch. Unless `exact_name`, a name collision gets a numeric suffix.
    async fn rename_branch_inner(
        &self,
        cwd: &Path,
        old_branch: &str,
        new_branch: &str,
        exact_name: bool,
    ) -> Result<String, GitCommandError> {
        if old_branch == new_branch {
            return Ok(new_branch.to_string());
        }
        let target = if exact_name {
            new_branch.to_string()
        } else {
            self.resolve_available_branch_name(cwd, new_branch).await?
        };
        self.run(
            "GitVcsDriver.renameBranch",
            cwd,
            ["branch", "-m", "--", old_branch, &target],
            GitOptions::timeout(Duration::from_secs(10)).detail("git branch rename failed"),
        )
        .await?;
        Ok(target)
    }

    async fn delete_local_branch_inner(
        &self,
        cwd: &Path,
        ref_name: &str,
        force: bool,
    ) -> Result<(), GitCommandError> {
        self.run(
            "GitVcsDriver.deleteLocalBranch",
            cwd,
            ["branch", if force { "-D" } else { "-d" }, "--", ref_name],
            GitOptions::timeout(Duration::from_secs(10)).detail("git branch delete failed"),
        )
        .await
        .map(|_| ())
    }

    async fn init_repo_inner(&self, cwd: &Path) -> Result<(), GitCommandError> {
        self.run(
            "GitVcsDriver.initRepo",
            cwd,
            ["init"],
            GitOptions::timeout(Duration::from_secs(10)).detail("git init failed"),
        )
        .await
        .map(|_| ())
    }

    async fn create_ref_inner(
        &self,
        cwd: &Path,
        ref_name: &str,
        switch_ref: bool,
    ) -> Result<String, GitCommandError> {
        self.run(
            "GitVcsDriver.createRef",
            cwd,
            ["branch", ref_name],
            GitOptions::timeout(Duration::from_secs(10)).detail("git branch create failed"),
        )
        .await?;
        if switch_ref {
            self.switch_ref_inner(cwd, ref_name).await?;
        }
        Ok(ref_name.to_string())
    }

    /// Checks out a local branch, or a remote one (creating a tracking branch unless one
    /// already tracks it). Returns the branch now checked out, `None` when HEAD is detached.
    async fn switch_ref_inner(
        &self,
        cwd: &Path,
        ref_name: &str,
    ) -> Result<Option<String>, GitCommandError> {
        let (local_ref, remote_ref) = (
            format!("refs/heads/{ref_name}"),
            format!("refs/remotes/{ref_name}"),
        );
        let (local_input_exists, remote_exists) = tokio::try_join!(
            self.ref_exists("GitVcsDriver.switchRef.localInputExists", cwd, &local_ref),
            self.ref_exists("GitVcsDriver.switchRef.remoteExists", cwd, &remote_ref),
        )?;
        let local_tracking_branch = if remote_exists {
            let result = self
                .git(
                    "GitVcsDriver.switchRef.localTrackingBranch",
                    cwd,
                    [
                        "for-each-ref",
                        "--format=%(refname:short)\t%(upstream:short)",
                        "refs/heads",
                    ],
                    GitOptions::allow_failure(Duration::from_secs(5)),
                )
                .await?;
            (result.exit_code == 0)
                .then(|| parse_tracking_branch_by_upstream_ref(&result.stdout, ref_name))
                .flatten()
        } else {
            None
        };
        let tracked_candidate = derive_local_branch_name_from_remote_ref(ref_name);
        let tracked_target_exists = match (&tracked_candidate, remote_exists) {
            (Some(candidate), true) => {
                self.ref_exists(
                    "GitVcsDriver.switchRef.localTrackedBranchTargetExists",
                    cwd,
                    &format!("refs/heads/{candidate}"),
                )
                .await?
            }
            _ => false,
        };

        let mut checkout: Vec<String> = vec!["checkout".into()];
        if local_input_exists {
            checkout.push(ref_name.into());
        } else if remote_exists && local_tracking_branch.is_none() && tracked_target_exists {
            checkout.push(ref_name.into());
        } else if remote_exists && local_tracking_branch.is_none() {
            checkout.extend(["--track".into(), ref_name.into()]);
        } else if let (true, Some(branch)) = (remote_exists, &local_tracking_branch) {
            checkout.push(branch.clone());
        } else {
            checkout.push(ref_name.into());
        }
        // A stale ref must not turn into a path checkout that discards local edits.
        checkout.push("--".into());
        self.run(
            "GitVcsDriver.switchRef.checkout",
            cwd,
            checkout,
            GitOptions::timeout(Duration::from_secs(10)).detail("git checkout failed"),
        )
        .await?;

        let current = self
            .stdout(
                "GitVcsDriver.switchRef.currentBranch",
                cwd,
                &["branch", "--show-current"],
                false,
            )
            .await?;
        let current = current.trim();
        Ok((!current.is_empty()).then(|| current.to_string()))
    }

    async fn prune_worktrees_inner(&self, cwd: &Path) -> Result<(), GitCommandError> {
        self.run(
            "GitVcsDriver.pruneWorktrees",
            cwd,
            ["worktree", "prune"],
            GitOptions::timeout(Duration::from_secs(15)).detail("git worktree prune failed"),
        )
        .await
        .map(|_| ())
    }

    /// Removes a worktree. One that is already gone is a no-op (after pruning its stale
    /// registration), because threads can share a worktree path and worktrees get removed
    /// outside the app.
    async fn remove_worktree_inner(
        &self,
        cwd: &Path,
        path: &Path,
        force: bool,
    ) -> Result<(), GitCommandError> {
        // Git refuses to remove a worktree with untracked files unless forced, but its check
        // honors `status.showUntrackedFiles=no` and would delete them.
        let mut args: Vec<String> = [
            "-c",
            "status.showUntrackedFiles=normal",
            "worktree",
            "remove",
        ]
        .map(String::from)
        .into();
        if force {
            args.push("--force".into());
        }
        args.push(path.to_string_lossy().into_owned());
        let result = self
            .git(
                "GitVcsDriver.removeWorktree",
                cwd,
                args.clone(),
                GitOptions {
                    // Removing dependency-heavy worktrees is filesystem-bound and can take
                    // minutes. Keep it bounded without interrupting git midway through cleanup.
                    env: vec![("LC_ALL".into(), Some("C".into()))],
                    ..GitOptions::allow_failure(WORKTREE_REMOVE_TIMEOUT)
                },
            )
            .await?;
        if result.exit_code == 0 {
            return Ok(());
        }
        let gone =
            is_missing_worktree_stderr(&result.stderr) && !path.try_exists().unwrap_or(false);
        if gone {
            return self.prune_worktrees_inner(cwd).await;
        }
        // Raw stderr stays out of the error (it can carry secrets).
        Err(GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(utf16_len(&result.stdout)),
            stderr_length: Some(utf16_len(&result.stderr)),
            ..GitCommandError::new(
                "GitVcsDriver.removeWorktree",
                cwd,
                &args,
                "git worktree remove failed",
            )
        })
    }

    /// `git worktree add`, then submodule initialisation (best effort) and the `gh-merge-base`
    /// config for a new branch.
    async fn create_worktree_inner(
        &self,
        input: &VcsCreateWorktreeInput,
        options: &CreateWorktreeOptions,
    ) -> Result<VcsWorktree, GitCommandError> {
        let target_branch = input.new_ref_name.as_deref().unwrap_or(&input.ref_name);
        let worktree_path = match &input.path {
            Some(path) => path.clone(),
            None => {
                let repo_name = input.cwd.file_name().map(PathBuf::from).unwrap_or_default();
                options
                    .worktrees_directory
                    .join(repo_name)
                    .join(target_branch.replace('/', "-"))
            }
        };
        let worktree_text = worktree_path.to_string_lossy().into_owned();
        let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
        match &input.new_ref_name {
            Some(new_ref) => args.extend([
                "-b".into(),
                new_ref.clone(),
                worktree_text,
                input.ref_name.clone(),
            ]),
            None => args.extend([worktree_text, input.ref_name.clone()]),
        }
        let checkout_workers = self
            .read_config_value(&input.cwd, "checkout.workers")
            .await?
            .unwrap_or_else(|| "0".into());
        let mut full_args = vec![
            "-c".to_string(),
            format!("checkout.workers={checkout_workers}"),
        ];
        full_args.extend(args);
        let progress = options.progress.clone().unwrap_or_default();
        let mut add_options =
            GitOptions::timeout(WORKTREE_ADD_TIMEOUT).detail("git worktree add failed");
        if let Some(on_progress) = progress.on_checkout_progress.clone() {
            // Git only prints checkout progress when stderr is a tty or the delay elapsed.
            // GIT_PROGRESS_DELAY=0 forces it through the pipe.
            add_options.env = vec![
                ("GIT_PROGRESS_DELAY".into(), Some("0".into())),
                ("LC_ALL".into(), Some("C".into())),
            ];
            add_options.progress = Some(GitProgress {
                on_stderr_line: Some(Arc::new(move |line: &str| {
                    if let Some((percent, completed, total)) =
                        parse_git_checkout_progress_line(line)
                    {
                        on_progress(CheckoutProgress {
                            percent,
                            completed,
                            total,
                        });
                    }
                })),
                ..GitProgress::default()
            });
        }
        self.run(
            "GitVcsDriver.createWorktree",
            &input.cwd,
            full_args,
            add_options,
        )
        .await?;
        if let Some(on_claimed) = &progress.on_worktree_claimed {
            on_claimed(&worktree_path);
        }

        // `git worktree add` leaves submodules empty. Best effort: failing to populate one
        // must not roll back the caller's thread.
        if options.submodules != WorktreeSubmodules::None
            && worktree_path
                .join(".gitmodules")
                .try_exists()
                .unwrap_or(false)
        {
            let mut submodule_args = vec!["submodule", "update", "--init"];
            if options.submodules == WorktreeSubmodules::Recursive {
                submodule_args.push("--recursive");
            }
            let _ = self
                .git(
                    "GitVcsDriver.createWorktree.updateSubmodules",
                    &worktree_path,
                    submodule_args,
                    GitOptions::default(),
                )
                .await;
        }

        if let (Some(new_ref), Some(base_ref)) = (&input.new_ref_name, &input.base_ref_name) {
            let remote_names = self
                .stdout(
                    "GitVcsDriver.listRemoteNames",
                    &input.cwd,
                    &["remote"],
                    false,
                )
                .await
                .map(|stdout| parse_remote_names_in_git_order(&stdout))
                .unwrap_or_default();
            let base_branch =
                parse_remote_ref_with_remote_names(base_ref, &sorted_longest_first(remote_names))
                    .map_or_else(|| base_ref.clone(), |(_, branch)| branch);
            self.run(
                "GitVcsDriver.createWorktree.configureBaseRef",
                &input.cwd,
                [
                    "config".to_string(),
                    format!("branch.{new_ref}.gh-merge-base"),
                    base_branch,
                ],
                GitOptions::default(),
            )
            .await?;
        }

        Ok(VcsWorktree {
            path: worktree_path,
            ref_name: target_branch.to_string(),
        })
    }

    // The mutating operations below drop the cached ref snapshots and per-repository status
    // facts afterwards, whether or not the git command succeeded.

    /// Renames a branch. Unless `exact_name`, a name collision gets a numeric suffix.
    pub async fn rename_branch(
        &self,
        cwd: &Path,
        old_branch: &str,
        new_branch: &str,
        exact_name: bool,
    ) -> Result<String, GitCommandError> {
        let result = self
            .rename_branch_inner(cwd, old_branch, new_branch, exact_name)
            .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    pub async fn delete_local_branch(
        &self,
        cwd: &Path,
        ref_name: &str,
        force: bool,
    ) -> Result<(), GitCommandError> {
        let result = self.delete_local_branch_inner(cwd, ref_name, force).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    pub async fn init_repo(&self, cwd: &Path) -> Result<(), GitCommandError> {
        let result = self.init_repo_inner(cwd).await;
        // A directory that was not a repository now is: forget that it was not.
        self.invalidate_repository_paths(cwd);
        self.invalidate_after_mutation(cwd).await;
        result
    }

    pub async fn create_ref(
        &self,
        cwd: &Path,
        ref_name: &str,
        switch_ref: bool,
    ) -> Result<String, GitCommandError> {
        let result = self.create_ref_inner(cwd, ref_name, switch_ref).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// Checks out a local branch, or a remote one (creating a tracking branch unless one
    /// already tracks it). Returns the branch now checked out, `None` when HEAD is detached.
    pub async fn switch_ref(
        &self,
        cwd: &Path,
        ref_name: &str,
    ) -> Result<Option<String>, GitCommandError> {
        let result = self.switch_ref_inner(cwd, ref_name).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    pub async fn prune_worktrees(&self, cwd: &Path) -> Result<(), GitCommandError> {
        let result = self.prune_worktrees_inner(cwd).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// Removes a worktree. One that is already gone is a no-op (after pruning its stale
    /// registration), because threads can share a worktree path and worktrees get removed
    /// outside the app.
    pub async fn remove_worktree(
        &self,
        cwd: &Path,
        path: &Path,
        force: bool,
    ) -> Result<(), GitCommandError> {
        let result = self.remove_worktree_inner(cwd, path, force).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// `git worktree add`, then submodule initialisation (best effort) and the `gh-merge-base`
    /// config for a new branch.
    pub async fn create_worktree(
        &self,
        input: &VcsCreateWorktreeInput,
        options: &CreateWorktreeOptions,
    ) -> Result<VcsWorktree, GitCommandError> {
        let result = self.create_worktree_inner(input, options).await;
        self.invalidate_after_mutation(&input.cwd).await;
        result
    }
}

pub fn parse_remote_names_in_git_order(stdout: &str) -> Vec<String> {
    stdout
        .split('\n')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(String::from)
        .collect()
}

/// Longest names first, so `team/fork` wins over `team` when matching a ref prefix.
pub fn sorted_longest_first(mut names: Vec<String>) -> Vec<String> {
    // JavaScript's sort is stable and compares UTF-16 lengths.
    names.sort_by_key(|name| std::cmp::Reverse(utf16_len(name)));
    names
}

/// Splits `remote/branch` using the first listed remote that prefixes the ref.
pub fn parse_remote_ref_with_remote_names(
    reference: &str,
    remote_names: &[String],
) -> Option<(String, String)> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    for remote in remote_names {
        let Some(branch) = reference.strip_prefix(&format!("{remote}/")) else {
            continue;
        };
        let branch = branch.trim();
        return (!branch.is_empty()).then(|| (remote.clone(), branch.to_string()));
    }
    None
}
