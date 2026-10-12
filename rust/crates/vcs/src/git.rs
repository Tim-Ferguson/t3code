//! The git driver's process plumbing and repository queries (port of `vcs/GitVcsDriver.ts`
//! outside checkpoints, which live in `checkpoint.rs`).
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    error::{VcsDriverKind, VcsError},
    process::{EnvOverrides, OutputMode, VcsProcess, VcsProcessInput, VcsProcessOutput},
    wire,
};

pub const WORKSPACE_FILES_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const GIT_CHECK_IGNORE_MAX_STDIN_BYTES: usize = 256 * 1024;
/// Keeps untracked-file scans from consulting a filesystem monitor or its cache.
pub const WORKSPACE_GIT_HARDENED_CONFIG_ARGS: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
];
/// Patch headers keep `a/` and `b/` even when the repository sets `diff.noprefix`.
pub const PATCH_RENDER_PREFIX_ARGS: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];

/// One git invocation, run as `git -C <cwd> <args>` from the server's own directory so a
/// missing `cwd` is reported by git instead of as a failure to spawn.
#[derive(Clone)]
pub struct GitCommand {
    pub operation: String,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: EnvOverrides,
    pub allow_non_zero_exit: bool,
    pub timeout: Option<Duration>,
    pub max_output_bytes: Option<usize>,
    pub output_mode: Option<OutputMode>,
    pub append_truncation_marker: bool,
}

impl GitCommand {
    pub fn new(
        operation: impl Into<String>,
        cwd: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            operation: operation.into(),
            cwd: cwd.into(),
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            env: Vec::new(),
            allow_non_zero_exit: false,
            timeout: None,
            max_output_bytes: None,
            output_mode: None,
            append_truncation_marker: false,
        }
    }

    pub fn stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    pub fn env(mut self, env: EnvOverrides) -> Self {
        self.env = env;
        self
    }

    pub fn allow_non_zero_exit(mut self) -> Self {
        self.allow_non_zero_exit = true;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = Some(max_output_bytes);
        self
    }

    pub fn output_mode(mut self, output_mode: OutputMode) -> Self {
        self.output_mode = Some(output_mode);
        self
    }

    pub fn append_truncation_marker(mut self) -> Self {
        self.append_truncation_marker = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VcsFreshnessSource {
    LiveLocal,
    CachedLocal,
    CachedRemote,
    ExplicitRemote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsFreshness {
    pub source: VcsFreshnessSource,
    #[serde(with = "wire::iso_millis")]
    pub observed_at: DateTime<Utc>,
    #[serde(with = "wire::effect_option_datetime")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl VcsFreshness {
    fn live_local() -> Self {
        Self {
            source: VcsFreshnessSource::LiveLocal,
            observed_at: Utc::now(),
            expires_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsDriverCapabilities {
    pub kind: VcsDriverKind,
    pub supports_worktrees: bool,
    pub supports_bookmarks: bool,
    pub supports_atomic_snapshot: bool,
    pub supports_push_default_remote: bool,
    /// `true` is the wire's `"native"`; `false` is `"git-compatible-fallback"`. Serialized as
    /// `ignoreClassifier`.
    #[serde(
        rename = "ignoreClassifier",
        serialize_with = "wire::ignore_classifier",
        deserialize_with = "wire::deserialize_ignore_classifier"
    )]
    pub native_ignore_classifier: bool,
}

pub const GIT_CAPABILITIES: VcsDriverCapabilities = VcsDriverCapabilities {
    kind: VcsDriverKind::Git,
    supports_worktrees: true,
    supports_bookmarks: false,
    supports_atomic_snapshot: false,
    supports_push_default_remote: true,
    native_ignore_classifier: true,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRepositoryIdentity {
    pub kind: VcsDriverKind,
    pub root_path: String,
    pub metadata_path: Option<String>,
    pub freshness: VcsFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListWorkspaceFilesResult {
    pub paths: Vec<String>,
    pub truncated: bool,
    pub freshness: VcsFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRemote {
    pub name: String,
    pub url: String,
    #[serde(with = "wire::effect_option")]
    pub push_url: Option<String>,
    pub is_primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListRemotesResult {
    pub remotes: Vec<VcsRemote>,
    pub freshness: VcsFreshness,
}

/// Git driver over a `VcsProcess`. Cheap to clone.
#[derive(Clone)]
pub struct GitVcs {
    pub(crate) process: VcsProcess,
    pub(crate) checkpoint_recovery_timeout: Duration,
    core: crate::core::GitCore,
}

impl GitVcs {
    pub fn new(process: VcsProcess) -> Self {
        let core = crate::core::GitCore::new(process.runner());
        Self {
            process,
            checkpoint_recovery_timeout: Duration::from_secs(5),
            core,
        }
    }

    /// Runs the driver's caches and throttles (see `GitTimings`) on `clock`.
    pub fn with_clock(
        mut self,
        clock: std::sync::Arc<dyn crate::clock::Clock>,
        timings: crate::state::GitTimings,
    ) -> Self {
        self.core = crate::core::GitCore::with_clock(self.process.runner(), clock, timings);
        self
    }

    /// Replaces the single deadline that bounds nested-repository recovery during checkpoint
    /// capture (5 seconds by default).
    pub fn with_checkpoint_recovery_timeout(mut self, timeout: Duration) -> Self {
        self.checkpoint_recovery_timeout = timeout;
        self
    }

    /// Branch, worktree, status and remote operations sharing this driver's process layer.
    /// Every call returns a handle on the same caches.
    pub fn core(&self) -> crate::core::GitCore {
        self.core.clone()
    }

    pub fn process(&self) -> &VcsProcess {
        &self.process
    }

    pub fn capabilities(&self) -> &'static VcsDriverCapabilities {
        &GIT_CAPABILITIES
    }

    /// Runs one git command (the driver's `execute`).
    pub async fn execute(&self, command: GitCommand) -> Result<VcsProcessOutput, VcsError> {
        let mut args = vec!["-C".to_string(), command.cwd.to_string_lossy().into_owned()];
        args.extend(command.args);
        let mut input = VcsProcessInput::new(command.operation, "git", args, command.cwd);
        input.spawn_cwd = std::env::current_dir().ok();
        input.stdin = command.stdin;
        input.env = command.env;
        input.allow_non_zero_exit = command.allow_non_zero_exit;
        input.timeout = command.timeout;
        input.max_output_bytes = command.max_output_bytes;
        input.output_mode = command.output_mode;
        input.append_truncation_marker = command.append_truncation_marker;
        let observation = crate::observe::start(&input.operation);
        let result = self.process.run(input).await;
        observation.finish(result.is_ok());
        result
    }

    pub async fn is_inside_work_tree(&self, cwd: &Path) -> Result<bool, VcsError> {
        let result = self
            .execute(
                GitCommand::new(
                    "GitVcsDriver.isInsideWorkTree",
                    cwd,
                    ["rev-parse", "--is-inside-work-tree"],
                )
                .allow_non_zero_exit()
                .timeout(Duration::from_secs(5))
                .max_output_bytes(4_096),
            )
            .await?;
        Ok(result.exit_code == 0 && result.stdout.trim() == "true")
    }

    /// `None` when `cwd` is not inside a git work tree.
    pub async fn detect_repository(
        &self,
        cwd: &Path,
    ) -> Result<Option<VcsRepositoryIdentity>, VcsError> {
        if !self.is_inside_work_tree(cwd).await? {
            return Ok(None);
        }
        let root = self
            .execute(GitCommand::new(
                "GitVcsDriver.detectRepository.root",
                cwd,
                ["rev-parse", "--show-toplevel"],
            ))
            .await?;
        let common_dir = self
            .execute(GitCommand::new(
                "GitVcsDriver.detectRepository.commonDir",
                cwd,
                ["rev-parse", "--git-common-dir"],
            ))
            .await
            .ok();
        Ok(Some(VcsRepositoryIdentity {
            kind: VcsDriverKind::Git,
            root_path: root.stdout.trim().to_string(),
            metadata_path: common_dir
                .map(|result| result.stdout.trim().to_string())
                .filter(|path| !path.is_empty()),
            freshness: VcsFreshness::live_local(),
        }))
    }

    pub async fn is_git_repository(&self, cwd: &Path) -> Result<bool, VcsError> {
        Ok(self.detect_repository(cwd).await?.is_some())
    }

    /// Tracked files plus untracked files git does not ignore, relative to the repository.
    pub async fn list_workspace_files(
        &self,
        cwd: &Path,
    ) -> Result<VcsListWorkspaceFilesResult, VcsError> {
        let operation = "GitVcsDriver.listWorkspaceFiles";
        let mut args: Vec<String> = WORKSPACE_GIT_HARDENED_CONFIG_ARGS.map(String::from).into();
        args.extend(
            [
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ]
            .map(String::from),
        );
        let result = self
            .execute(
                GitCommand::new(operation, cwd, args)
                    .allow_non_zero_exit()
                    .timeout(Duration::from_secs(20))
                    .max_output_bytes(WORKSPACE_FILES_MAX_OUTPUT_BYTES)
                    .append_truncation_marker(),
            )
            .await?;
        if result.exit_code != 0 {
            return Err(failed_command(operation, "git ls-files", cwd, &result));
        }
        Ok(VcsListWorkspaceFilesResult {
            paths: split_null_separated_paths(&result.stdout, result.stdout_truncated),
            truncated: result.stdout_truncated,
            freshness: VcsFreshness::live_local(),
        })
    }

    pub async fn list_remotes(&self, cwd: &Path) -> Result<VcsListRemotesResult, VcsError> {
        let operation = "GitVcsDriver.listRemotes";
        let result = self
            .execute(
                GitCommand::new(operation, cwd, ["remote", "-v"])
                    .allow_non_zero_exit()
                    .timeout(Duration::from_secs(5))
                    .max_output_bytes(64 * 1024),
            )
            .await?;
        if result.exit_code != 0 {
            return Err(failed_command(operation, "git remote -v", cwd, &result));
        }
        let remotes = parse_git_remote_verbose_output(&result.stdout)
            .into_iter()
            .filter_map(|(name, (url, push_url))| {
                let url = url.filter(|url| !url.is_empty())?;
                Some(VcsRemote {
                    is_primary: name == "origin",
                    name,
                    url,
                    push_url: push_url.filter(|url| !url.is_empty()),
                })
            })
            .collect();
        Ok(VcsListRemotesResult {
            remotes,
            freshness: VcsFreshness::live_local(),
        })
    }

    /// Drops the paths git ignores, keeping the caller's order.
    pub async fn filter_ignored_paths(
        &self,
        cwd: &Path,
        relative_paths: &[String],
    ) -> Result<Vec<String>, VcsError> {
        let operation = "GitVcsDriver.filterIgnoredPaths";
        if relative_paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut ignored = HashSet::new();
        for chunk in chunk_paths_for_git_check_ignore(relative_paths) {
            let mut args: Vec<String> = WORKSPACE_GIT_HARDENED_CONFIG_ARGS.map(String::from).into();
            args.extend(["check-ignore", "--no-index", "-z", "--stdin"].map(String::from));
            let mut stdin = chunk.join("\0");
            stdin.push('\0');
            let result = self
                .execute(
                    GitCommand::new(operation, cwd, args)
                        .stdin(stdin)
                        .allow_non_zero_exit()
                        .timeout(Duration::from_secs(20))
                        .max_output_bytes(WORKSPACE_FILES_MAX_OUTPUT_BYTES)
                        .append_truncation_marker(),
                )
                .await?;
            // 1 means "none of these paths is ignored".
            if result.exit_code != 0 && result.exit_code != 1 {
                return Err(failed_command(operation, "git check-ignore", cwd, &result));
            }
            ignored.extend(split_null_separated_paths(
                &result.stdout,
                result.stdout_truncated,
            ));
        }
        Ok(relative_paths
            .iter()
            .filter(|path| !ignored.contains(*path))
            .cloned()
            .collect())
    }

    pub async fn init_repository(&self, cwd: &Path) -> Result<(), VcsError> {
        self.execute(
            GitCommand::new("GitVcsDriver.initRepository", cwd, ["init"])
                .timeout(Duration::from_secs(10))
                .max_output_bytes(64 * 1024),
        )
        .await
        .map(|_| ())
    }
}

/// A `VcsProcessExitError` for a command that was allowed to fail and did.
fn failed_command(
    operation: &str,
    command: &str,
    cwd: &Path,
    result: &VcsProcessOutput,
) -> VcsError {
    let stderr = result.stderr.trim();
    VcsError::exit(
        operation,
        command,
        &cwd.to_string_lossy(),
        result.exit_code,
        if stderr.is_empty() {
            format!("{command} failed")
        } else {
            stderr.to_string()
        },
    )
}

/// Splits NUL-delimited git output. A truncated listing loses its final, partial entry.
pub fn split_null_separated_paths(input: &str, truncated: bool) -> Vec<String> {
    let mut parts: Vec<&str> = input.split('\0').collect();
    if truncated && parts.last().is_some_and(|last| !last.is_empty()) {
        parts.pop();
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect()
}

/// Splits paths into `git check-ignore --stdin` batches of at most 256 KiB, counting each path
/// as its UTF-8 length plus one NUL.
pub fn chunk_paths_for_git_check_ignore(relative_paths: &[String]) -> Vec<Vec<String>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_bytes = 0;
    for path in relative_paths {
        let path_bytes = path.len() + 1;
        if !chunk.is_empty() && chunk_bytes + path_bytes > GIT_CHECK_IGNORE_MAX_STDIN_BYTES {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 0;
        }
        chunk.push(path.clone());
        chunk_bytes += path_bytes;
        if chunk_bytes >= GIT_CHECK_IGNORE_MAX_STDIN_BYTES {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 0;
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Parses `git remote -v` into `name -> (fetch url, push url)`, in first-seen order.
pub fn parse_git_remote_verbose_output(
    output: &str,
) -> Vec<(String, (Option<String>, Option<String>))> {
    use std::sync::OnceLock;
    static LINE: OnceLock<regex::Regex> = OnceLock::new();
    let line_pattern =
        LINE.get_or_init(|| regex::Regex::new(r"^(\S+)\s+(\S+)\s+\((fetch|push)\)$").unwrap());
    let mut order: Vec<String> = Vec::new();
    let mut remotes: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    for line in output.split('\n') {
        let Some(captures) = line_pattern.captures(line.trim()) else {
            continue;
        };
        let name = captures[1].to_string();
        let url = captures[2].to_string();
        if !remotes.contains_key(&name) {
            order.push(name.clone());
        }
        let remote = remotes.entry(name).or_default();
        if &captures[3] == "fetch" {
            remote.0 = Some(url);
        } else {
            remote.1 = Some(url);
        }
    }
    order
        .into_iter()
        .map(|name| {
            let remote = remotes.remove(&name).unwrap_or_default();
            (name, remote)
        })
        .collect()
}
