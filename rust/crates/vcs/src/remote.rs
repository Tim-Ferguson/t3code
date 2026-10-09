//! Remotes and history-changing operations of the git driver: upstream resolution, remote
//! status and its throttled background refresh, commit, push, pull and the fetch family
//! (ports of the matching parts of `vcs/GitVcsDriverCore.ts`). Every operation that moves
//! refs drops the ref and status caches afterwards.
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use regex::Regex;
use serde::Serialize;

use crate::{
    core::{
        GitCommandError, GitCore, GitOptions, GitTimeout, is_non_repository_stderr,
        is_unborn_head_stderr, parse_remote_names_in_git_order, parse_remote_ref_with_remote_names,
        sorted_longest_first, utf16_len,
    },
    error::OutputStream,
    git::parse_git_remote_verbose_output,
    progress::{GitProgress, HookFinished},
    remote_refs::{normalize_git_remote_url, sanitize_remote_name},
    state::StatusRemoteKey,
    status::GitRemoteStatusDetails,
    wire::{PullStatus, VcsPullResult},
};

const PREPARED_COMMIT_PATCH_MAX_OUTPUT_BYTES: usize = 49_000;
const RANGE_COMMIT_SUMMARY_MAX_OUTPUT_BYTES: usize = 19_000;
const RANGE_DIFF_SUMMARY_MAX_OUTPUT_BYTES: usize = 19_000;
const RANGE_DIFF_PATCH_MAX_OUTPUT_BYTES: usize = 59_000;

/// Keeps a background or scripted fetch from ever waiting on a person.
fn non_interactive_env() -> Vec<(String, Option<String>)> {
    [
        ("GCM_INTERACTIVE", "never"),
        ("GIT_ASKPASS", ""),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("SSH_ASKPASS", ""),
        ("SSH_ASKPASS_REQUIRE", "never"),
    ]
    .map(|(name, value)| (name.to_string(), Some(value.to_string())))
    .into()
}

fn c_locale() -> (String, Option<String>) {
    ("LC_ALL".into(), Some("C".into()))
}

fn args_of<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.map(String::from).into()
}

/// The branch a local branch tracks, split into remote and branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRef {
    pub upstream_ref: String,
    pub remote_name: String,
    pub branch_name: String,
}

/// Splits `origin/feature` into remote and branch: by the longest configured remote name that
/// prefixes it, else at the first `/`.
pub fn split_upstream_ref(upstream_ref: &str, remote_names: &[String]) -> Option<UpstreamRef> {
    let (remote_name, branch_name) = parse_remote_ref_with_remote_names(upstream_ref, remote_names)
        .or_else(|| {
            let separator = upstream_ref.find('/')?;
            if separator == 0 || separator == upstream_ref.len() - 1 {
                return None;
            }
            let (remote, branch) = (
                upstream_ref[..separator].trim(),
                upstream_ref[separator + 1..].trim(),
            );
            (!remote.is_empty() && !branch.is_empty())
                .then(|| (remote.to_string(), branch.to_string()))
        })?;
    Some(UpstreamRef {
        upstream_ref: upstream_ref.to_string(),
        remote_name,
        branch_name,
    })
}

/// Fetch stderr can contain remote credentials. Only fixed diagnoses may enter an error;
/// anything else keeps the generic message.
pub fn fetch_failure_detail(stderr: &str) -> Option<&'static str> {
    static PATTERNS: OnceLock<[Regex; 4]> = OnceLock::new();
    let [authentication, unreachable, repository, lock] = PATTERNS.get_or_init(|| {
        [
            r"(?i)^(?:fatal: (?:Authentication failed|could not read (?:Username|Password))\b|\S+: Permission denied \(publickey)",
            r"(?i)^(?:(?:fatal: |ssh: )?Could not resolve host(?:name)?\b|fatal: unable to access .+: (?:Could not resolve host|Failed to connect)\b|ssh: connect to host \S+ port \d+: (?:Connection timed out|Connection refused|Network is unreachable)\b)",
            r"(?i)^(?:remote: Repository not found\.?$|fatal: repository .+ not found$|fatal: .+ does not appear to be a git repository$)",
            r#"(?i)^(?:(?:error|fatal): cannot lock ref\b|fatal: Unable to create ['"].+\.lock['"]:)"#,
        ]
        .map(|pattern| Regex::new(pattern).unwrap())
    });
    let lines: Vec<&str> = stderr
        .split('\n')
        .map(|line| line.trim_end_matches('\r').trim())
        .collect();
    let any = |pattern: &Regex| lines.iter().any(|line| pattern.is_match(line));
    if any(authentication) {
        Some(
            "Git could not authenticate with the remote. Check Git credentials or SSH access on the server, then retry.",
        )
    } else if any(unreachable) {
        Some(
            "Git could not reach the remote. Check the server's network connection and remote host, then retry.",
        )
    } else if any(repository) {
        Some(
            "Git could not access the remote repository. Check the remote URL and repository permissions on the server.",
        )
    } else if any(lock) {
        Some(
            "Git could not update a local reference. Another Git operation or a stale lock may be blocking the fetch; check the repository on the server, then retry.",
        )
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PushStatus {
    Pushed,
    SkippedUpToDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPushResult {
    pub status: PushStatus,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_upstream: Option<bool>,
}

impl GitPushResult {
    fn pushed(branch: &str, upstream_branch: Option<String>, set_upstream: bool) -> Self {
        Self {
            status: PushStatus::Pushed,
            branch: branch.into(),
            upstream_branch,
            set_upstream: Some(set_upstream),
        }
    }

    fn up_to_date(branch: &str, upstream_branch: Option<String>) -> Self {
        Self {
            status: PushStatus::SkippedUpToDate,
            branch: branch.into(),
            upstream_branch,
            set_upstream: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRefreshCheckedOutBranchResult {
    pub head_commit: String,
    pub moved: bool,
    pub on_target: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPreparedCommitContext {
    pub staged_summary: String,
    pub staged_patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRangeContext {
    pub commit_summary: String,
    pub diff_summary: String,
    pub diff_patch: String,
}

pub type OutputLineCallback = Arc<dyn Fn(OutputStream, &str) + Send + Sync>;

/// `GitCommitProgress`: git's output lines, and the commit hooks as they run.
#[derive(Clone, Default)]
pub struct GitCommitProgress {
    pub on_output_line: Option<OutputLineCallback>,
    pub on_hook_started: Option<crate::progress::LineCallback>,
    pub on_hook_finished: Option<Arc<dyn Fn(&HookFinished) + Send + Sync>>,
}

/// Stage the working tree immediately before committing; `file_paths: None` stages everything.
#[derive(Clone, Default)]
pub struct CommitStage {
    pub file_paths: Option<Vec<String>>,
}

#[derive(Clone, Default)]
pub struct GitCommitOptions {
    pub timeout: Option<Duration>,
    pub progress: Option<GitCommitProgress>,
    pub stage: Option<CommitStage>,
}

impl GitCore {
    pub(crate) async fn list_remote_names(
        &self,
        cwd: &Path,
    ) -> Result<Vec<String>, GitCommandError> {
        let stdout = self
            .stdout("GitVcsDriver.listRemoteNames", cwd, &["remote"], false)
            .await?;
        Ok(parse_remote_names_in_git_order(&stdout))
    }

    /// The upstream of the checked-out branch, or `None` without one.
    pub async fn resolve_current_upstream(
        &self,
        cwd: &Path,
    ) -> Result<Option<UpstreamRef>, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.resolveCurrentUpstream",
                cwd,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                true,
            )
            .await?;
        let upstream_ref = stdout.trim();
        if upstream_ref.is_empty() || upstream_ref == "@{upstream}" {
            return Ok(None);
        }
        let remote_names =
            sorted_longest_first(self.list_remote_names(cwd).await.unwrap_or_default());
        Ok(split_upstream_ref(upstream_ref, &remote_names))
    }

    /// Fetches the upstream's remote for status. `--no-auto-gc` keeps this poll from starting
    /// `git gc --auto`: when that gc fails (a repository with missing objects, say), git retries
    /// it on every fetch and leaves a full-size `tmp_pack_*` behind each time.
    async fn fetch_remote_for_status(
        &self,
        git_common_dir: &str,
        remote_name: &str,
    ) -> Result<(), GitCommandError> {
        self.run(
            "GitVcsDriver.fetchRemoteForStatus",
            &crate::refs::fetch_cwd(git_common_dir),
            [
                "--git-dir",
                git_common_dir,
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-auto-gc",
                remote_name,
            ],
            GitOptions {
                env: non_interactive_env(),
                timeout: GitTimeout::After(self.state.timings.status_upstream_refresh_timeout),
                ..GitOptions::default()
            }
            .detail("Background Git fetch exited with a non-zero status."),
        )
        .await
        .map(|_| ())
    }

    /// Fetches the current upstream's remote unless that happened within the refresh interval.
    /// A failed fetch is remembered too, with a back-off that doubles per consecutive failure,
    /// so a status poll cannot become a fetch storm against a slow or unavailable remote.
    /// Failures never reach the caller: status falls back to the last fetched refs.
    pub(crate) async fn refresh_status_upstream_if_stale(&self, cwd: &Path) {
        let Ok(Some(upstream)) = self.resolve_current_upstream(cwd).await else {
            return;
        };
        let Ok(Some(paths)) = self.resolve_repository_paths(cwd, false).await else {
            return;
        };
        let state = &self.state;
        let key = StatusRemoteKey {
            git_common_dir: paths.git_common_dir,
            remote_name: upstream.remote_name,
        };
        let _ = state
            .status_remote_refresh
            .get(
                key.clone(),
                async {
                    match self
                        .fetch_remote_for_status(&key.git_common_dir, &key.remote_name)
                        .await
                    {
                        Ok(()) => {
                            state.clear_refresh_failures(&key);
                            Ok(true)
                        }
                        Err(error) => {
                            state.record_refresh_failure(&key);
                            Err(error)
                        }
                    }
                },
                |result| match result {
                    Ok(_) => state.timings.status_upstream_refresh_interval,
                    Err(_) => state.refresh_failure_cooldown(&key),
                },
            )
            .await;
    }

    /// Upstream divergence without reading the working tree. `refresh_upstream` first runs the
    /// throttled background fetch.
    pub async fn status_details_remote(
        &self,
        cwd: &Path,
        refresh_upstream: bool,
    ) -> Result<GitRemoteStatusDetails, GitCommandError> {
        if refresh_upstream {
            self.refresh_status_upstream_if_stale(cwd).await;
        }
        let branch_args = ["rev-parse", "--abbrev-ref", "HEAD"];
        let stable = || GitOptions {
            env: vec![c_locale()],
            allow_non_zero_exit: true,
            ..GitOptions::default()
        };
        let branch_result = match self
            .run(
                "GitVcsDriver.statusDetailsRemote.branch",
                cwd,
                branch_args,
                stable(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) if self.is_missing_cwd(&error, cwd) => {
                return Ok(GitRemoteStatusDetails::default());
            }
            Err(error) => return Err(error),
        };
        let branch = if branch_result.exit_code != 0 {
            if is_non_repository_stderr(&branch_result.stderr) {
                return Ok(GitRemoteStatusDetails::default());
            }
            if !is_unborn_head_stderr(&branch_result.stderr) {
                return Err(GitCommandError {
                    exit_code: Some(branch_result.exit_code),
                    stdout_length: Some(utf16_len(&branch_result.stdout)),
                    stderr_length: Some(utf16_len(&branch_result.stderr)),
                    ..GitCommandError::new(
                        "GitVcsDriver.statusDetailsRemote.branch",
                        cwd,
                        &args_of(branch_args),
                        "Git branch lookup failed.",
                    )
                });
            }
            let unborn = self
                .stdout(
                    "GitVcsDriver.statusDetailsRemote.unbornBranch",
                    cwd,
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                    false,
                )
                .await?;
            Some(unborn.trim().to_string()).filter(|name| !name.is_empty())
        } else {
            let value = branch_result.stdout.trim();
            (!value.is_empty() && value != "HEAD").then(|| value.to_string())
        };

        let upstream = self.resolve_current_upstream(cwd).await?;
        let upstream_ref = upstream.map(|upstream| upstream.upstream_ref);
        let (mut ahead_count, mut behind_count) = (0, 0);
        if let Some(upstream_ref) = &upstream_ref {
            let divergence = self
                .run(
                    "GitVcsDriver.statusDetailsRemote.divergence",
                    cwd,
                    args_of(["rev-list", "--left-right", "--count"])
                        .into_iter()
                        .chain([format!("HEAD...{upstream_ref}")]),
                    GitOptions {
                        allow_non_zero_exit: true,
                        ..GitOptions::default()
                    },
                )
                .await?;
            if divergence.exit_code == 0 {
                let mut counts = divergence.stdout.trim().split_whitespace();
                let mut count = || {
                    counts
                        .next()
                        .and_then(crate::refs::parse_leading_int)
                        .map_or(0, |value| value.max(0) as u64)
                };
                (ahead_count, behind_count) = (count(), count());
            }
        } else if let Some(branch) = &branch {
            ahead_count = self
                .compute_ahead_count_against_base(cwd, branch)
                .await
                .unwrap_or(0);
        }

        let default_branch = self.resolve_default_branch_name(cwd, "origin").await?;
        let is_default_branch = branch.as_deref().is_some_and(|branch| {
            default_branch.as_deref() == Some(branch)
                || (default_branch.is_none() && (branch == "main" || branch == "master"))
        });
        let ahead_of_default_count = match &branch {
            Some(branch) if !is_default_branch => {
                if upstream_ref.is_none() {
                    ahead_count
                } else {
                    self.compute_ahead_count_against_base(cwd, branch)
                        .await
                        .unwrap_or(0)
                }
            }
            _ => 0,
        };
        Ok(GitRemoteStatusDetails {
            is_repo: true,
            default_branch,
            is_default_branch,
            branch,
            has_upstream: upstream_ref.is_some(),
            upstream_ref,
            ahead_count,
            behind_count,
            ahead_of_default_count,
        })
    }

    async fn resolve_publish_branch_name(&self, cwd: &Path, branch_name: &str) -> String {
        let remote_names = self.list_remote_names(cwd).await.unwrap_or_default();
        parse_remote_ref_with_remote_names(branch_name, &remote_names)
            .map_or_else(|| branch_name.to_string(), |(_, branch)| branch)
    }

    /// `branch.<name>.pushRemote`, then `remote.pushDefault`, then the primary remote.
    async fn resolve_push_remote_name(
        &self,
        cwd: &Path,
        ref_name: &str,
    ) -> Result<Option<String>, GitCommandError> {
        for (operation, key) in [
            (
                "GitVcsDriver.resolvePushRemoteName.branchPushRemote",
                format!("branch.{ref_name}.pushRemote"),
            ),
            (
                "GitVcsDriver.resolvePushRemoteName.remotePushDefault",
                "remote.pushDefault".to_string(),
            ),
        ] {
            let configured = self
                .stdout(operation, cwd, &["config", "--get", &key], true)
                .await?;
            if !configured.trim().is_empty() {
                return Ok(Some(configured.trim().to_string()));
            }
        }
        Ok(self.resolve_primary_remote_name(cwd).await.ok())
    }

    /// Returns the name of a remote with `url` (matched across ssh/https spellings), adding
    /// one under `preferred_name` (suffixed when taken) if none exists.
    pub async fn ensure_remote(
        &self,
        cwd: &Path,
        preferred_name: &str,
        url: &str,
    ) -> Result<String, GitCommandError> {
        let result = self.ensure_remote_inner(cwd, preferred_name, url).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn ensure_remote_inner(
        &self,
        cwd: &Path,
        preferred_name: &str,
        url: &str,
    ) -> Result<String, GitCommandError> {
        let preferred = sanitize_remote_name(preferred_name);
        let target = normalize_git_remote_url(url);
        let listing = self
            .stdout(
                "GitVcsDriver.ensureRemote.listRemoteUrls",
                cwd,
                &["remote", "-v"],
                false,
            )
            .await?;
        let fetch_urls: Vec<(String, String)> = parse_git_remote_verbose_output(&listing)
            .into_iter()
            .filter_map(|(name, (fetch, _))| fetch.map(|fetch| (name, fetch)))
            .filter(|(name, fetch)| !name.is_empty() && !fetch.is_empty())
            .collect();
        if let Some((name, _)) = fetch_urls
            .iter()
            .find(|(_, fetch)| normalize_git_remote_url(fetch) == target)
        {
            return Ok(name.clone());
        }
        let mut name = preferred.clone();
        let mut suffix = 1;
        while fetch_urls.iter().any(|(existing, _)| *existing == name) {
            name = format!("{preferred}-{suffix}");
            suffix += 1;
        }
        self.run(
            "GitVcsDriver.ensureRemote.add",
            cwd,
            ["remote", "add", &name, url],
            GitOptions::default(),
        )
        .await?;
        Ok(name)
    }

    pub async fn set_branch_upstream(
        &self,
        cwd: &Path,
        branch: &str,
        remote_name: &str,
        remote_branch: &str,
    ) -> Result<(), GitCommandError> {
        let result = self
            .run(
                "GitVcsDriver.setBranchUpstream",
                cwd,
                [
                    "branch",
                    "--set-upstream-to",
                    &format!("{remote_name}/{remote_branch}"),
                    branch,
                ],
                GitOptions::default(),
            )
            .await
            .map(|_| ());
        self.invalidate_after_mutation(cwd).await;
        result
    }

    // -----------------------------------------------------------------------------------------
    // Fetch
    // -----------------------------------------------------------------------------------------

    async fn fetch_refspec(
        &self,
        operation: &str,
        cwd: &Path,
        args: Vec<String>,
        detail: String,
    ) -> Result<(), GitCommandError> {
        self.run(operation, cwd, args, GitOptions::default().detail(detail))
            .await
            .map(|_| ())
    }

    /// `git fetch <remote>`, or only `ref_name` from it when the remote has that branch.
    /// Failures name a fixed diagnosis (see `fetch_failure_detail`), never git's stderr.
    pub async fn fetch_remote(
        &self,
        cwd: &Path,
        remote_name: &str,
        ref_name: Option<&str>,
    ) -> Result<(), GitCommandError> {
        let result = self.fetch_remote_inner(cwd, remote_name, ref_name).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn fetch_remote_inner(
        &self,
        cwd: &Path,
        remote_name: &str,
        ref_name: Option<&str>,
    ) -> Result<(), GitCommandError> {
        let fallback = format!("git fetch {remote_name} failed");
        let attempt = |args: Vec<String>| {
            let fallback = fallback.clone();
            async move {
                let result = self
                    .run(
                        "GitVcsDriver.fetchRemote",
                        cwd,
                        args.clone(),
                        GitOptions {
                            env: non_interactive_env()
                                .into_iter()
                                .chain([c_locale()])
                                .collect(),
                            allow_non_zero_exit: true,
                            ..GitOptions::default()
                        },
                    )
                    .await?;
                if result.exit_code == 0 {
                    return Ok(None);
                }
                Ok::<_, GitCommandError>(Some((
                    GitCommandError {
                        exit_code: Some(result.exit_code),
                        stdout_length: Some(utf16_len(&result.stdout)),
                        stderr_length: Some(utf16_len(&result.stderr)),
                        ..GitCommandError::new(
                            "GitVcsDriver.fetchRemote",
                            cwd,
                            &args,
                            fetch_failure_detail(&result.stderr).unwrap_or(&fallback),
                        )
                    },
                    result.stderr,
                )))
            }
        };
        let all_args = args_of(["fetch", "--quiet"])
            .into_iter()
            .chain([remote_name.to_string()])
            .collect::<Vec<_>>();
        let Some(ref_name) = ref_name else {
            return attempt(all_args)
                .await?
                .map_or(Ok(()), |(error, _)| Err(error));
        };
        let branch = parse_remote_ref_with_remote_names(ref_name, &[remote_name.to_string()])
            .map_or_else(|| ref_name.to_string(), |(_, branch)| branch);
        let mut scoped = all_args.clone();
        scoped.push(format!(
            "+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}"
        ));
        let Some((error, stderr)) = attempt(scoped).await? else {
            return Ok(());
        };
        let missing = format!("fatal: couldn't find remote ref refs/heads/{branch}");
        if stderr
            .split('\n')
            .any(|line| line.trim_end_matches('\r') == missing)
        {
            return attempt(all_args)
                .await?
                .map_or(Ok(()), |(error, _)| Err(error));
        }
        Err(error)
    }

    pub async fn fetch_remote_tracking_branch(
        &self,
        cwd: &Path,
        remote_name: &str,
        remote_branch: &str,
    ) -> Result<(), GitCommandError> {
        let result = self
            .fetch_refspec(
                "GitVcsDriver.fetchRemoteTrackingBranch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote_name.into(),
                    format!(
                        "+refs/heads/{remote_branch}:refs/remotes/{remote_name}/{remote_branch}"
                    ),
                ],
                "Git command exited with a non-zero status.".into(),
            )
            .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// Fetches `remote_branch` and points the local `local_branch` at it, replacing the branch
    /// if it exists.
    pub async fn fetch_remote_branch(
        &self,
        cwd: &Path,
        remote_name: &str,
        remote_branch: &str,
        local_branch: &str,
    ) -> Result<(), GitCommandError> {
        let result = async {
            self.fetch_refspec(
                "GitVcsDriver.fetchRemoteBranch.fetch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote_name.into(),
                    format!(
                        "+refs/heads/{remote_branch}:refs/remotes/{remote_name}/{remote_branch}"
                    ),
                ],
                "Git command exited with a non-zero status.".into(),
            )
            .await?;
            let exists = self.branch_exists(cwd, local_branch).await?;
            let target = format!("{remote_name}/{remote_branch}");
            let args: Vec<&str> = if exists {
                vec!["branch", "--force", local_branch, &target]
            } else {
                vec!["branch", local_branch, &target]
            };
            self.run(
                "GitVcsDriver.fetchRemoteBranch.materialize",
                cwd,
                args,
                GitOptions::default(),
            )
            .await
            .map(|_| ())
        }
        .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// `refs/pull/<n>/head` into the local branch `branch`.
    pub async fn fetch_pull_request_branch(
        &self,
        cwd: &Path,
        pr_number: u64,
        branch: &str,
    ) -> Result<(), GitCommandError> {
        let result = async {
            let remote = self.resolve_primary_remote_name(cwd).await?;
            self.fetch_refspec(
                "GitVcsDriver.fetchPullRequestBranch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote,
                    format!("+refs/pull/{pr_number}/head:refs/heads/{branch}"),
                ],
                "git fetch pull request branch failed".into(),
            )
            .await
        }
        .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    /// Fetches `refs/pull/<n>/head` without writing a branch (the commit lands in `FETCH_HEAD`,
    /// the only way to read it while that branch is checked out elsewhere).
    pub async fn fetch_pull_request_head_commit(
        &self,
        cwd: &Path,
        pr_number: u64,
    ) -> Result<String, GitCommandError> {
        let remote = self.resolve_primary_remote_name(cwd).await?;
        self.fetch_refspec(
            "GitVcsDriver.fetchPullRequestHeadCommit",
            cwd,
            vec![
                "fetch".into(),
                "--quiet".into(),
                "--no-tags".into(),
                remote,
                format!("refs/pull/{pr_number}/head"),
            ],
            "git fetch pull request head failed".into(),
        )
        .await?;
        self.resolve_commit(cwd, "FETCH_HEAD").await
    }

    /// The commit of a remote-tracking ref, and the ref's name. A name without a configured
    /// remote prefix is looked up under `fallback_remote_name`.
    pub async fn resolve_remote_tracking_commit(
        &self,
        cwd: &Path,
        ref_name: &str,
        fallback_remote_name: &str,
    ) -> Result<(String, String), GitCommandError> {
        let remote_names = sorted_longest_first(self.list_remote_names(cwd).await?);
        let remote_ref = parse_remote_ref_with_remote_names(ref_name, &remote_names).map_or_else(
            || format!("{fallback_remote_name}/{ref_name}"),
            |(remote, branch)| format!("{remote}/{branch}"),
        );
        let stdout = self
            .stdout(
                "GitVcsDriver.resolveRemoteTrackingCommit",
                cwd,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/remotes/{remote_ref}^{{commit}}"),
                ],
                false,
            )
            .await?;
        Ok((stdout.trim().to_string(), remote_ref))
    }

    /// Moves the branch checked out in `cwd` onto `target_commit`, from inside that worktree.
    /// Untouched while the tree is dirty. Fast-forwards when it can; a rewritten head is taken
    /// by resetting only when HEAD still sits on `reset_when_head_commit` (the upstream commit
    /// read before the fetch), which is lossless exactly when the checkout holds no work of
    /// its own.
    pub async fn refresh_checked_out_branch(
        &self,
        cwd: &Path,
        target_commit: &str,
        reset_when_head_commit: Option<&str>,
    ) -> Result<GitRefreshCheckedOutBranchResult, GitCommandError> {
        let result = self
            .refresh_checked_out_branch_inner(cwd, target_commit, reset_when_head_commit)
            .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn refresh_checked_out_branch_inner(
        &self,
        cwd: &Path,
        target_commit: &str,
        reset_when_head_commit: Option<&str>,
    ) -> Result<GitRefreshCheckedOutBranchResult, GitCommandError> {
        let head_commit = self.resolve_commit(cwd, "HEAD").await?;
        let unmoved = |on_target| GitRefreshCheckedOutBranchResult {
            head_commit: head_commit.clone(),
            moved: false,
            on_target,
        };
        if head_commit == target_commit {
            return Ok(unmoved(true));
        }
        let changes = self
            .stdout(
                "GitVcsDriver.refreshCheckedOutBranch.status",
                cwd,
                &["status", "--porcelain"],
                false,
            )
            .await?;
        if !changes.trim().is_empty() {
            return Ok(unmoved(false));
        }
        let is_ancestor = self
            .run(
                "GitVcsDriver.refreshCheckedOutBranch.isAncestor",
                cwd,
                ["merge-base", "--is-ancestor", &head_commit, target_commit],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?
            .exit_code
            == 0;
        if !is_ancestor && reset_when_head_commit != Some(head_commit.as_str()) {
            return Ok(unmoved(false));
        }
        if !is_ancestor {
            // The commit being reset away is about to be reachable from nothing. It is only
            // ever a commit the remote already held, but "the remote held it" stops being a
            // way back once the head it belonged to has been rewritten, so a ref keeps it
            // findable.
            self.run(
                "GitVcsDriver.refreshCheckedOutBranch.keepPrevious",
                cwd,
                ["update-ref", "refs/t3code/pre-refresh", &head_commit],
                GitOptions::default().detail("git failed to record the previous checkout commit"),
            )
            .await?;
        }
        // `--merge` rather than `--hard`: the cleanliness check above is a snapshot, and
        // another thread may edit a tracked file between it and this move. Git itself refuses
        // a `--merge` reset that would overwrite such an edit, the same guarantee `--ff-only`
        // gives the other branch, so a race loses nothing; the refresh fails and is reported.
        let args = if is_ancestor {
            ["merge", "--ff-only", target_commit]
        } else {
            ["reset", "--merge", target_commit]
        };
        self.run(
            "GitVcsDriver.refreshCheckedOutBranch.move",
            cwd,
            args,
            GitOptions::default()
                .detail("git failed to move the checkout onto the pull request head"),
        )
        .await?;
        Ok(GitRefreshCheckedOutBranchResult {
            head_commit: target_commit.into(),
            moved: true,
            on_target: true,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Commit
    // -----------------------------------------------------------------------------------------

    async fn stage_commit_changes(
        &self,
        cwd: &Path,
        file_paths: Option<&[String]>,
        index_env: Option<&[(String, Option<String>)]>,
    ) -> Result<(), GitCommandError> {
        let config: Vec<String> = if index_env.is_some() {
            args_of([
                "-c",
                "core.splitIndex=false",
                "-c",
                "splitIndex.sharedIndexExpire=never",
            ])
            .into()
        } else {
            Vec::new()
        };
        let env = || GitOptions {
            env: index_env.map(<[_]>::to_vec).unwrap_or_default(),
            ..GitOptions::default()
        };
        let with_config = |rest: &[&str]| -> Vec<String> {
            config
                .iter()
                .cloned()
                .chain(rest.iter().map(|arg| arg.to_string()))
                .collect()
        };
        match file_paths.filter(|paths| !paths.is_empty()) {
            Some(paths) => {
                if index_env.is_some() {
                    let head = self
                        .run(
                            "GitVcsDriver.prepareCommitContext.head",
                            cwd,
                            ["rev-parse", "--verify", "--quiet", "HEAD"],
                            GitOptions {
                                allow_non_zero_exit: true,
                                ..GitOptions::default()
                            },
                        )
                        .await?;
                    // Even with GIT_INDEX_FILE, reset clears the repository's merge state.
                    self.run(
                        "GitVcsDriver.prepareCommitContext.readTree",
                        cwd,
                        with_config(&[
                            "read-tree",
                            if head.exit_code == 0 {
                                "HEAD"
                            } else {
                                "--empty"
                            },
                        ]),
                        env(),
                    )
                    .await?;
                } else {
                    let _ = self
                        .run(
                            "GitVcsDriver.commit.reset",
                            cwd,
                            ["reset"],
                            GitOptions::default(),
                        )
                        .await;
                }
                let mut args = with_config(&["--literal-pathspecs", "add", "-A", "--"]);
                args.extend(paths.iter().cloned());
                self.run("GitVcsDriver.commit.addSelected", cwd, args, env())
                    .await?;
            }
            None => {
                self.run(
                    "GitVcsDriver.commit.addAll",
                    cwd,
                    with_config(&["add", "-A"]),
                    env(),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// What a commit of `file_paths` (or everything) would contain, computed in a temporary
    /// index so the repository's own index and merge state are untouched. `None` when nothing
    /// would be committed.
    pub async fn prepare_commit_context(
        &self,
        cwd: &Path,
        file_paths: Option<&[String]>,
    ) -> Result<Option<GitPreparedCommitContext>, GitCommandError> {
        let index_value = self
            .stdout(
                "GitVcsDriver.prepareCommitContext.indexPath",
                cwd,
                &["rev-parse", "--git-path", "index"],
                false,
            )
            .await?;
        let index_path = crate::refs::resolve_path(cwd, Path::new(index_value.trim()));
        let io_error = |cause: std::io::Error| GitCommandError {
            cause: Some(Arc::new(cause)),
            argument_count: None,
            ..GitCommandError::new(
                "GitVcsDriver.prepareCommitContext",
                cwd,
                &[],
                "Failed to prepare the temporary commit index.",
            )
        };
        let directory =
            crate::temp::TempPath::directory("t3code-commit-index-").map_err(io_error)?;
        let temp_index = directory.path().join("index");
        let env = vec![(
            "GIT_INDEX_FILE".to_string(),
            Some(temp_index.to_string_lossy().into_owned()),
        )];
        let config = [
            "-c",
            "core.splitIndex=false",
            "-c",
            "splitIndex.sharedIndexExpire=never",
        ];
        if index_path.try_exists().map_err(io_error)? {
            crate::temp::copy_index_with_whole_second_mtime(&index_path, &temp_index)
                .map_err(io_error)?;
            self.run(
                "GitVcsDriver.prepareCommitContext.expandSplitIndex",
                cwd,
                config
                    .iter()
                    .copied()
                    .chain(["update-index", "--no-split-index"]),
                GitOptions {
                    env: env.clone(),
                    ..GitOptions::default()
                },
            )
            .await?;
        } else {
            self.run(
                "GitVcsDriver.prepareCommitContext.emptyIndex",
                cwd,
                config.iter().copied().chain(["read-tree", "--empty"]),
                GitOptions {
                    env: env.clone(),
                    ..GitOptions::default()
                },
            )
            .await?;
        }
        self.stage_commit_changes(cwd, file_paths, Some(&env))
            .await?;

        let summary = self
            .stdout_with_marker(
                "GitVcsDriver.prepareCommitContext.stagedSummary",
                cwd,
                ["diff", "--cached", "--name-status"],
                GitOptions {
                    env: env.clone(),
                    ..GitOptions::default()
                },
            )
            .await?;
        let summary = summary.trim();
        if summary.is_empty() {
            return Ok(None);
        }
        let patch = self
            .stdout_with_marker(
                "GitVcsDriver.prepareCommitContext.stagedPatch",
                cwd,
                ["diff", "--no-ext-diff", "--cached", "--patch", "--minimal"],
                GitOptions {
                    env,
                    max_output_bytes: Some(PREPARED_COMMIT_PATCH_MAX_OUTPUT_BYTES),
                    append_truncation_marker: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        Ok(Some(GitPreparedCommitContext {
            staged_summary: summary.to_string(),
            staged_patch: patch,
        }))
    }

    /// Commits the index (after staging when `options.stage` is set) and returns the new
    /// commit's sha.
    pub async fn commit(
        &self,
        cwd: &Path,
        subject: &str,
        body: &str,
        options: GitCommitOptions,
    ) -> Result<String, GitCommandError> {
        let result = self.commit_inner(cwd, subject, body, options).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn commit_inner(
        &self,
        cwd: &Path,
        subject: &str,
        body: &str,
        options: GitCommitOptions,
    ) -> Result<String, GitCommandError> {
        if let Some(stage) = &options.stage {
            self.stage_commit_changes(cwd, stage.file_paths.as_deref(), None)
                .await?;
        }
        let mut args = args_of(["commit", "-m"]).to_vec();
        args.push(subject.into());
        if !body.trim().is_empty() {
            args.extend(["-m".into(), body.trim().into()]);
        }
        let progress = options.progress.map(|progress| {
            let output = progress.on_output_line;
            let line_callback = |stream| {
                output
                    .clone()
                    .map(|callback| -> crate::progress::LineCallback {
                        Arc::new(move |line: &str| callback(stream, line))
                    })
            };
            GitProgress {
                on_stdout_line: line_callback(OutputStream::Stdout),
                on_stderr_line: line_callback(OutputStream::Stderr),
                on_hook_started: progress.on_hook_started,
                on_hook_finished: progress.on_hook_finished,
                keep_line_callbacks_after_truncation: false,
            }
        });
        self.run(
            "GitVcsDriver.commit.commit",
            cwd,
            args,
            GitOptions {
                timeout: options
                    .timeout
                    .map_or(GitTimeout::Default, GitTimeout::After),
                progress,
                ..GitOptions::default()
            },
        )
        .await?;
        let sha = self
            .stdout(
                "GitVcsDriver.commit.revParseHead",
                cwd,
                &["rev-parse", "HEAD"],
                false,
            )
            .await?;
        Ok(sha.trim().to_string())
    }

    // -----------------------------------------------------------------------------------------
    // Push and pull
    // -----------------------------------------------------------------------------------------

    async fn push(
        &self,
        operation: &str,
        cwd: &Path,
        args: &[&str],
    ) -> Result<(), GitCommandError> {
        self.run(
            operation,
            cwd,
            args.iter().copied(),
            GitOptions {
                timeout: GitTimeout::Unlimited,
                ..GitOptions::default()
            },
        )
        .await
        .map(|_| ())
    }

    /// Pushes the checked-out branch (`fallback_branch` when HEAD is detached but a branch is
    /// known). An explicit `remote_name` publishes the branch there and tracks it. Otherwise a
    /// branch without upstream is published to its push remote; a branch tracking a
    /// differently named ref (cut from it, as `git checkout -b feature origin/dev` leaves it)
    /// is published under its own name, because that upstream is its base and not its target.
    pub async fn push_current_branch(
        &self,
        cwd: &Path,
        fallback_branch: Option<&str>,
        remote_name: Option<&str>,
    ) -> Result<GitPushResult, GitCommandError> {
        let result = self
            .push_current_branch_inner(cwd, fallback_branch, remote_name)
            .await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn push_current_branch_inner(
        &self,
        cwd: &Path,
        fallback_branch: Option<&str>,
        remote_name: Option<&str>,
    ) -> Result<GitPushResult, GitCommandError> {
        let details = self.status_details(cwd).await?;
        let Some(branch) = details
            .branch
            .clone()
            .or_else(|| fallback_branch.map(String::from))
        else {
            return Err(GitCommandError::new(
                "GitVcsDriver.pushCurrentBranch",
                cwd,
                &args_of(["push"]),
                "Cannot push from detached HEAD.",
            ));
        };
        let branch = branch.as_str();

        if let Some(requested) = remote_name.map(str::trim).filter(|name| !name.is_empty()) {
            let publish = self.resolve_publish_branch_name(cwd, branch).await;
            self.push(
                "GitVcsDriver.pushCurrentBranch.pushWithRequestedRemote",
                cwd,
                &[
                    "push",
                    "-u",
                    requested,
                    &format!("HEAD:refs/heads/{publish}"),
                ],
            )
            .await?;
            return Ok(GitPushResult::pushed(
                branch,
                Some(format!("{requested}/{publish}")),
                true,
            ));
        }

        if details.ahead_count == 0 && details.behind_count == 0 {
            if details.has_upstream {
                return Ok(GitPushResult::up_to_date(
                    branch,
                    details.upstream_ref.clone(),
                ));
            }
            let base = self
                .resolve_base_branch_for_no_upstream(cwd, branch, false)
                .await
                .ok()
                .flatten();
            if base.is_some() {
                let Some(publish_remote) = self
                    .resolve_push_remote_name(cwd, branch)
                    .await
                    .ok()
                    .flatten()
                else {
                    return Ok(GitPushResult::up_to_date(branch, None));
                };
                if self
                    .remote_branch_exists(cwd, &publish_remote, branch)
                    .await
                    .unwrap_or(false)
                {
                    return Ok(GitPushResult::up_to_date(branch, None));
                }
            }
        }

        if !details.has_upstream {
            let Some(publish_remote) = self.resolve_push_remote_name(cwd, branch).await? else {
                return Err(GitCommandError::new(
                    "GitVcsDriver.pushCurrentBranch",
                    cwd,
                    &args_of(["push"]),
                    "Cannot push because no git remote is configured for this repository.",
                ));
            };
            let publish = self.resolve_publish_branch_name(cwd, branch).await;
            self.push(
                "GitVcsDriver.pushCurrentBranch.pushWithUpstream",
                cwd,
                &[
                    "push",
                    "-u",
                    &publish_remote,
                    &format!("HEAD:refs/heads/{publish}"),
                ],
            )
            .await?;
            return Ok(GitPushResult::pushed(
                branch,
                Some(format!("{publish_remote}/{publish}")),
                true,
            ));
        }

        if let Some(upstream) = self.resolve_current_upstream(cwd).await.ok().flatten() {
            // A git-mangled alias, such as local `upstream/effect-atom` tracking `effect-atom`
            // of the `my-org/upstream` remote, is the one same-repo setup where the names
            // legitimately differ: the branch name ends in the upstream head while the upstream
            // ref ends in the branch name.
            let is_alias_of_upstream_head = branch == upstream.branch_name
                || (branch.ends_with(&format!("/{}", upstream.branch_name))
                    && upstream.upstream_ref.ends_with(&format!("/{branch}")));
            if !is_alias_of_upstream_head {
                let publish_remote = self
                    .resolve_push_remote_name(cwd, branch)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| upstream.remote_name.clone());
                let publish = self.resolve_publish_branch_name(cwd, branch).await;
                // `-u` retargets the upstream to the published branch, so keep the base
                // recorded first; base resolution reads gh-merge-base before the upstream ref.
                let merge_base_key = format!("branch.{branch}.gh-merge-base");
                let configured = self
                    .stdout(
                        "GitVcsDriver.pushCurrentBranch.readMergeBase",
                        cwd,
                        &["config", "--get", &merge_base_key],
                        true,
                    )
                    .await?;
                if configured.trim().is_empty() {
                    self.run(
                        "GitVcsDriver.pushCurrentBranch.recordMergeBase",
                        cwd,
                        ["config", &merge_base_key, &upstream.branch_name],
                        GitOptions::default(),
                    )
                    .await?;
                }
                self.push(
                    "GitVcsDriver.pushCurrentBranch.pushOwnBranch",
                    cwd,
                    &[
                        "push",
                        "-u",
                        &publish_remote,
                        &format!("HEAD:refs/heads/{publish}"),
                    ],
                )
                .await?;
                return Ok(GitPushResult::pushed(
                    branch,
                    Some(format!("{publish_remote}/{publish}")),
                    true,
                ));
            }
            self.push(
                "GitVcsDriver.pushCurrentBranch.pushUpstream",
                cwd,
                &[
                    "push",
                    &upstream.remote_name,
                    &format!("HEAD:refs/heads/{}", upstream.branch_name),
                ],
            )
            .await?;
            return Ok(GitPushResult::pushed(
                branch,
                Some(upstream.upstream_ref),
                false,
            ));
        }

        self.push("GitVcsDriver.pushCurrentBranch.push", cwd, &["push"])
            .await?;
        Ok(GitPushResult::pushed(branch, details.upstream_ref, false))
    }

    /// Fast-forwards the checked-out branch from its upstream.
    pub async fn pull_current_branch(&self, cwd: &Path) -> Result<VcsPullResult, GitCommandError> {
        let result = self.pull_current_branch_inner(cwd).await;
        self.invalidate_after_mutation(cwd).await;
        result
    }

    async fn pull_current_branch_inner(
        &self,
        cwd: &Path,
    ) -> Result<VcsPullResult, GitCommandError> {
        let pull_args = args_of(["pull", "--ff-only"]);
        let details = self.status_details(cwd).await?;
        let Some(ref_name) = details.branch else {
            return Err(GitCommandError::new(
                "GitVcsDriver.pullCurrentBranch",
                cwd,
                &pull_args,
                "Cannot pull from detached HEAD.",
            ));
        };
        if !details.has_upstream {
            return Err(GitCommandError::new(
                "GitVcsDriver.pullCurrentBranch",
                cwd,
                &pull_args,
                "Current branch has no upstream configured. Push with upstream first.",
            ));
        }
        let before = self
            .stdout(
                "GitVcsDriver.pullCurrentBranch.beforeSha",
                cwd,
                &["rev-parse", "HEAD"],
                true,
            )
            .await?;
        self.run(
            "GitVcsDriver.pullCurrentBranch.pull",
            cwd,
            ["pull", "--ff-only"],
            GitOptions::timeout(Duration::from_secs(30)).detail("git pull failed"),
        )
        .await?;
        let after = self
            .stdout(
                "GitVcsDriver.pullCurrentBranch.afterSha",
                cwd,
                &["rev-parse", "HEAD"],
                true,
            )
            .await?;
        let refreshed = self.status_details(cwd).await?;
        let (before, after) = (before.trim(), after.trim());
        Ok(VcsPullResult {
            status: if !before.is_empty() && before == after {
                PullStatus::SkippedUpToDate
            } else {
                PullStatus::Pulled
            },
            ref_name,
            upstream_ref: refreshed.upstream_ref,
        })
    }

    /// The log, diffstat and patch of `base_ref..HEAD` (the patch from the merge base, as pull
    /// request diffs are), each capped, for generating descriptions.
    pub async fn read_range_context(
        &self,
        cwd: &Path,
        base_ref: &str,
    ) -> Result<GitRangeContext, GitCommandError> {
        let commit_range = format!("{base_ref}..HEAD");
        let diff_range = format!("{base_ref}...HEAD");
        let capped = |max| GitOptions {
            max_output_bytes: Some(max),
            append_truncation_marker: true,
            ..GitOptions::default()
        };
        let (commit_summary, diff_summary, diff_patch) = tokio::try_join!(
            self.stdout_with_marker(
                "GitVcsDriver.readRangeContext.log",
                cwd,
                ["log", "--oneline", &commit_range],
                capped(RANGE_COMMIT_SUMMARY_MAX_OUTPUT_BYTES),
            ),
            self.stdout_with_marker(
                "GitVcsDriver.readRangeContext.diffStat",
                cwd,
                ["diff", "--stat", &diff_range],
                capped(RANGE_DIFF_SUMMARY_MAX_OUTPUT_BYTES),
            ),
            self.stdout_with_marker(
                "GitVcsDriver.readRangeContext.diffPatch",
                cwd,
                ["diff", "--no-ext-diff", "--patch", "--minimal", &diff_range],
                capped(RANGE_DIFF_PATCH_MAX_OUTPUT_BYTES),
            ),
        )?;
        Ok(GitRangeContext {
            commit_summary,
            diff_summary,
            diff_patch,
        })
    }
}
