//! Repository status (ports of `readStatusDetailsLocal`, `readStatusDetailsRemote` and the
//! background upstream refresh in `vcs/GitVcsDriverCore.ts`).
use std::{collections::HashMap, path::Path, sync::OnceLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    checkpoint::locale_compare,
    core::{
        GitCommandError, GitCore, GitOptions, is_non_repository_stderr, is_unborn_head_stderr,
        utf16_len,
    },
};

const DEFAULT_BASE_BRANCH_CANDIDATES: [&str; 2] = ["main", "master"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkingTree {
    pub files: Vec<ChangedFile>,
    pub insertions: u64,
    pub deletions: u64,
}

/// Totals for the diff panel's Changes view: merge-base with the base branch to the working
/// tree, untracked files included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchChanges {
    pub base_ref: Option<String>,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatusDetails {
    pub is_repo: bool,
    pub has_origin_remote: bool,
    pub is_default_branch: bool,
    pub branch: Option<String>,
    pub upstream_ref: Option<String>,
    pub has_working_tree_changes: bool,
    pub working_tree: WorkingTree,
    pub branch_changes: Option<BranchChanges>,
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    pub ahead_of_default_count: u64,
}

/// `GitRemoteStatusDetails`: what the upstream says, without reading the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitRemoteStatusDetails {
    pub is_repo: bool,
    pub default_branch: Option<String>,
    pub is_default_branch: bool,
    pub branch: Option<String>,
    pub upstream_ref: Option<String>,
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    pub ahead_of_default_count: u64,
}

/// `VcsStatusResult`. The pull request and source control provider are the server's to fill.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatus {
    pub is_repo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_control_provider: Option<serde_json::Value>,
    pub has_primary_remote: bool,
    pub is_default_ref: bool,
    pub ref_name: Option<String>,
    pub has_working_tree_changes: bool,
    pub working_tree: WorkingTree,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_changes: Option<BranchChanges>,
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead_of_default_count: Option<u64>,
    pub pr: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy)]
pub struct LocalStatusOptions {
    /// Skip revision walks and report zero divergence, for local-only consumers.
    pub include_divergence: bool,
    /// Also read the Changes totals. A failure leaves them out.
    pub include_branch_changes: bool,
}

impl Default for LocalStatusOptions {
    fn default() -> Self {
        Self {
            include_divergence: true,
            include_branch_changes: false,
        }
    }
}

/// JavaScript's `Number.parseInt(text, 10)`, with NaN as `None`.
fn js_parse_int(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let value: i64 = digits[..end].parse().ok()?;
    Some(if negative { -value } else { value })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

/// Parses plain (non `-z`) numstat lines. A rename shown as `old => new` keeps the new name.
///
/// TODO: git abbreviates a rename as `src/{util.ts => helpers.ts}`, which this reports as
/// `helpers.ts}`. The original has the same bug (`GitVcsDriverCore.ts`) and this copies it on
/// purpose; `tests/git_rename_parity.rs` pins it. Fix both by reading `--numstat -z`, as
/// `review::parse_review_numstat` does.
pub fn parse_numstat_entries(stdout: &str) -> Vec<NumstatEntry> {
    let mut entries = Vec::new();
    for line in stdout
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
    {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        let path_parts = parts.get(2..).unwrap_or(&[]);
        let raw_path = if path_parts.len() > 1 {
            path_parts.last().unwrap().trim().to_string()
        } else {
            path_parts.join("\t").trim().to_string()
        };
        if raw_path.is_empty() {
            continue;
        }
        let count = |text: Option<&&str>| {
            js_parse_int(text.copied().unwrap_or("0"))
                .unwrap_or(0)
                .max(0) as u64
        };
        let normalized = match raw_path.find(" => ") {
            Some(arrow) => raw_path[arrow + " => ".len()..].trim().to_string(),
            None => raw_path.clone(),
        };
        entries.push(NumstatEntry {
            path: if normalized.is_empty() {
                raw_path
            } else {
                normalized
            },
            insertions: count(parts.first()),
            deletions: count(parts.get(1)),
        });
    }
    entries
}

/// `# branch.ab +3 -4` payload.
pub fn parse_branch_ab(value: &str) -> (u64, u64) {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"^\+(\d+)\s+-(\d+)$").unwrap());
    match pattern.captures(value) {
        Some(captures) => (
            captures[1].parse().unwrap_or(0),
            captures[2].parse().unwrap_or(0),
        ),
        None => (0, 0),
    }
}

/// The path on one `git status --porcelain=2` entry line.
pub fn parse_porcelain_path(line: &str) -> Option<String> {
    if line.starts_with("? ") || line.starts_with("! ") {
        let simple = line[2..].trim();
        return (!simple.is_empty()).then(|| simple.to_string());
    }
    if !(line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ")) {
        return None;
    }
    if let Some(tab) = line.find('\t') {
        let path = line[tab + 1..].split('\t').next().unwrap_or("").trim();
        return (!path.is_empty()).then(|| path.to_string());
    }
    line.trim().split_whitespace().last().map(String::from)
}

pub fn parse_default_branch_from_remote_head_ref(value: &str, remote_name: &str) -> Option<String> {
    let prefix = format!("refs/remotes/{remote_name}/");
    let branch = value.trim().strip_prefix(&prefix)?.trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

fn stable() -> GitOptions {
    GitOptions {
        env: vec![("LC_ALL".into(), Some("C".into()))],
        allow_non_zero_exit: true,
        ..GitOptions::default()
    }
}

impl GitCore {
    /// The default branch recorded by `refs/remotes/<remote>/HEAD`, if any.
    pub async fn resolve_default_branch_name(
        &self,
        cwd: &Path,
        remote_name: &str,
    ) -> Result<Option<String>, GitCommandError> {
        let result = self
            .run(
                "GitVcsDriver.resolveDefaultBranchName",
                cwd,
                ["symbolic-ref", &format!("refs/remotes/{remote_name}/HEAD")],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        Ok(if result.exit_code == 0 {
            parse_default_branch_from_remote_head_ref(&result.stdout, remote_name)
        } else {
            None
        })
    }

    pub async fn remote_exists(
        &self,
        cwd: &Path,
        remote_name: &str,
    ) -> Result<bool, GitCommandError> {
        let result = self
            .run(
                "GitVcsDriver.remoteExists",
                cwd,
                ["remote", "get-url", remote_name],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    pub async fn remote_branch_exists(
        &self,
        cwd: &Path,
        remote_name: &str,
        ref_name: &str,
    ) -> Result<bool, GitCommandError> {
        let result = self
            .run(
                "GitVcsDriver.remoteBranchExists",
                cwd,
                [
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/{remote_name}/{ref_name}"),
                ],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    /// `origin` when it exists, otherwise the first configured remote.
    pub async fn resolve_primary_remote_name(&self, cwd: &Path) -> Result<String, GitCommandError> {
        if self.remote_exists(cwd, "origin").await? {
            return Ok("origin".into());
        }
        let remotes = self
            .run(
                "GitVcsDriver.listRemoteNames",
                cwd,
                ["remote"],
                GitOptions::default(),
            )
            .await?;
        match crate::core::parse_remote_names_in_git_order(&remotes.stdout)
            .into_iter()
            .next()
        {
            Some(first) => Ok(first),
            None => Err(GitCommandError::new(
                "GitVcsDriver.resolvePrimaryRemoteName",
                cwd,
                &["remote".to_string()],
                "No git remote is configured for this repository.",
            )),
        }
    }

    /// The branch a branch without an upstream is compared against: its configured
    /// `gh-merge-base`, the remote's default branch, then `main` and `master`. Prefers the
    /// remote-tracking ref over a local branch of the same name.
    ///
    /// `allow_remote_of_current` lets the review diff compare the default branch with its
    /// remote copy.
    pub(crate) async fn resolve_base_branch_for_no_upstream(
        &self,
        cwd: &Path,
        ref_name: &str,
        allow_remote_of_current: bool,
    ) -> Result<Option<String>, GitCommandError> {
        let configured = self
            .run(
                "GitVcsDriver.resolveBaseBranchForNoUpstream.config",
                cwd,
                [
                    "config",
                    "--get",
                    &format!("branch.{ref_name}.gh-merge-base"),
                ],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        let configured = configured.stdout.trim().to_string();
        let primary = self.resolve_primary_remote_name(cwd).await.ok();
        let default_branch = match &primary {
            Some(remote) => self.resolve_default_branch_name(cwd, remote).await?,
            None => None,
        };
        let candidates = [
            (!configured.is_empty()).then_some(configured),
            default_branch,
            Some(DEFAULT_BASE_BRANCH_CANDIDATES[0].to_string()),
            Some(DEFAULT_BASE_BRANCH_CANDIDATES[1].to_string()),
        ];
        let non_origin_prefix = primary
            .as_deref()
            .filter(|remote| *remote != "origin")
            .map(|remote| format!("{remote}/"));
        for candidate in candidates.into_iter().flatten() {
            let normalized = if let Some(rest) = candidate.strip_prefix("origin/") {
                rest.to_string()
            } else if let Some(rest) = non_origin_prefix
                .as_deref()
                .and_then(|prefix| candidate.strip_prefix(prefix))
            {
                rest.to_string()
            } else {
                candidate
            };
            if normalized.is_empty() {
                continue;
            }
            if normalized == ref_name {
                if allow_remote_of_current
                    && let Some(remote) = &primary
                    && self.remote_branch_exists(cwd, remote, &normalized).await?
                {
                    return Ok(Some(format!("{remote}/{normalized}")));
                }
                continue;
            }
            if let Some(remote) = &primary
                && self.remote_branch_exists(cwd, remote, &normalized).await?
            {
                return Ok(Some(format!("{remote}/{normalized}")));
            }
            if self.branch_exists(cwd, &normalized).await? {
                return Ok(Some(normalized));
            }
        }
        Ok(None)
    }

    pub(crate) async fn compute_ahead_count_against_base(
        &self,
        cwd: &Path,
        ref_name: &str,
    ) -> Result<u64, GitCommandError> {
        let Some(base) = self
            .resolve_base_branch_for_no_upstream(cwd, ref_name, false)
            .await?
        else {
            return Ok(0);
        };
        let result = self
            .run(
                "GitVcsDriver.computeAheadCountAgainstBase",
                cwd,
                ["rev-list", "--count", &format!("{base}..HEAD")],
                GitOptions {
                    allow_non_zero_exit: true,
                    ..GitOptions::default()
                },
            )
            .await?;
        if result.exit_code != 0 {
            return Ok(0);
        }
        Ok(js_parse_int(result.stdout.trim()).unwrap_or(0).max(0) as u64)
    }

    /// Branch, upstream, divergence and per-file working-tree change counts. A directory that
    /// is not a repository (or does not exist) yields `is_repo: false`, not an error.
    pub async fn status_details_local(
        &self,
        cwd: &Path,
        options: LocalStatusOptions,
    ) -> Result<GitStatusDetails, GitCommandError> {
        let missing_cwd = |error: &GitCommandError| error.cause.is_some() && !cwd.is_dir();
        let mut status_args = vec!["status", "--porcelain=2", "--branch"];
        if !options.include_divergence {
            status_args.push("--no-ahead-behind");
        }

        let index = match self
            .run(
                "GitVcsDriver.statusDetails.indexPath",
                cwd,
                ["rev-parse", "--git-path", "index"],
                stable(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) if missing_cwd(&error) => return Ok(GitStatusDetails::default()),
            Err(error) => return Err(error),
        };
        if index.exit_code == 0 {
            let lock = format!("{}.lock", cwd.join(index.stdout.trim()).display());
            let locked = Path::new(&lock)
                .try_exists()
                .map_err(|cause| GitCommandError {
                    cause: Some(std::sync::Arc::new(cause)),
                    ..GitCommandError::new(
                        "GitVcsDriver.statusDetails.indexPath",
                        cwd,
                        &[],
                        "Failed to check the Git index lock.",
                    )
                })?;
            if locked {
                return Err(GitCommandError {
                    argument_count: None,
                    ..GitCommandError::new(
                        "GitVcsDriver.statusDetails.indexPath",
                        cwd,
                        &[],
                        "Git index is locked. Status will resume when the index lock is removed.",
                    )
                });
            }
        }
        let status = match self
            .run(
                "GitVcsDriver.statusDetails.status",
                cwd,
                status_args.clone(),
                stable(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) if missing_cwd(&error) => return Ok(GitStatusDetails::default()),
            Err(error) => return Err(error),
        };
        if status.exit_code != 0 {
            if is_non_repository_stderr(&status.stderr) {
                return Ok(GitStatusDetails::default());
            }
            let args: Vec<String> = status_args.iter().map(|arg| arg.to_string()).collect();
            return Err(GitCommandError {
                exit_code: Some(status.exit_code),
                stdout_length: Some(utf16_len(&status.stdout)),
                stderr_length: Some(utf16_len(&status.stderr)),
                ..GitCommandError::new(
                    "GitVcsDriver.statusDetails.status",
                    cwd,
                    &args,
                    "Git status failed.",
                )
            });
        }

        let repository_paths = self
            .resolve_repository_paths(cwd, false)
            .await
            .ok()
            .flatten();
        let (numstat, default_branch, has_primary_remote) = tokio::join!(
            self.read_numstat(cwd),
            self.default_branch_cached(cwd, repository_paths.as_ref()),
            self.origin_exists_cached(cwd, repository_paths.as_ref()),
        );
        let numstat = numstat?;

        let (mut ref_name, mut upstream_ref) = (None, None);
        let (mut ahead, mut behind, mut ahead_of_default) = (0, 0, 0);
        let mut has_changes = false;
        let mut changed_paths: Vec<String> = Vec::new();
        for line in status
            .stdout
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
        {
            if let Some(value) = line.strip_prefix("# branch.head ") {
                let value = value.trim();
                ref_name = (!value.starts_with('(')).then(|| value.to_string());
            } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
                let value = value.trim();
                upstream_ref = (!value.is_empty()).then(|| value.to_string());
            } else if let Some(value) = line
                .strip_prefix("# branch.ab ")
                .filter(|_| options.include_divergence)
            {
                (ahead, behind) = parse_branch_ab(value.trim());
            } else if !line.trim().is_empty() && !line.starts_with('#') {
                has_changes = true;
                if let Some(path) = parse_porcelain_path(line) {
                    changed_paths.push(path);
                }
            }
        }

        let fallback_ahead = match (&upstream_ref, &ref_name) {
            (None, Some(branch)) if options.include_divergence => Some(
                self.compute_ahead_count_against_base(cwd, branch)
                    .await
                    .unwrap_or(0),
            ),
            _ => None,
        };
        if let Some(count) = fallback_ahead {
            ahead = count;
            behind = 0;
        }
        let is_default_branch = ref_name.as_deref().is_some_and(|branch| {
            default_branch.as_deref() == Some(branch)
                || (default_branch.is_none() && (branch == "main" || branch == "master"))
        });
        if options.include_divergence
            && !is_default_branch
            && let Some(branch) = &ref_name
        {
            ahead_of_default = match fallback_ahead {
                Some(count) => count,
                None => self
                    .compute_ahead_count_against_base(cwd, branch)
                    .await
                    .unwrap_or(0),
            };
        }

        // Later entries for a path replace earlier ones, as in a `Map`.
        let mut by_path: HashMap<String, (u64, u64)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for entry in parse_numstat_entries(&numstat) {
            if by_path
                .insert(entry.path.clone(), (entry.insertions, entry.deletions))
                .is_none()
            {
                order.push(entry.path);
            }
        }
        let (mut insertions, mut deletions) = (0, 0);
        let mut files: Vec<ChangedFile> = order
            .into_iter()
            .map(|path| {
                let (added, removed) = by_path[&path];
                insertions += added;
                deletions += removed;
                ChangedFile {
                    path,
                    insertions: added,
                    deletions: removed,
                }
            })
            .collect();
        files.sort_by(|left, right| locale_compare(&left.path, &right.path));
        for path in changed_paths {
            if !by_path.contains_key(&path) && !files.iter().any(|file| file.path == path) {
                files.push(ChangedFile {
                    path,
                    insertions: 0,
                    deletions: 0,
                });
            }
        }
        files.sort_by(|left, right| locale_compare(&left.path, &right.path));

        let branch_changes = if options.include_branch_changes {
            let root = repository_paths
                .as_ref()
                .and_then(|paths| paths.worktree_root.as_deref().map(Path::new))
                .unwrap_or(cwd);
            self.read_branch_change_totals(root, ref_name.as_deref())
                .await
                .ok()
        } else {
            None
        };

        Ok(GitStatusDetails {
            is_repo: true,
            has_origin_remote: has_primary_remote,
            is_default_branch,
            branch: ref_name,
            has_upstream: upstream_ref.is_some(),
            upstream_ref,
            has_working_tree_changes: has_changes,
            working_tree: WorkingTree {
                files,
                insertions,
                deletions,
            },
            branch_changes,
            ahead_count: ahead,
            behind_count: behind,
            ahead_of_default_count: ahead_of_default,
        })
    }

    /// The default branch of `origin`, cached per repository for five minutes.
    async fn default_branch_cached(
        &self,
        cwd: &Path,
        paths: Option<&crate::state::RepositoryPaths>,
    ) -> Option<String> {
        let Some(paths) = paths else {
            return self
                .resolve_default_branch_name(cwd, "origin")
                .await
                .unwrap_or(None);
        };
        let git_common_dir = paths.git_common_dir.clone();
        let fetch_cwd = crate::refs::fetch_cwd(&git_common_dir);
        let ttl = self.state.timings.status_default_branch_ttl;
        self.state
            .default_branch
            .get(
                git_common_dir.clone(),
                async {
                    let result = self
                        .run(
                            "GitVcsDriver.statusDetails.defaultBranch",
                            &fetch_cwd,
                            [
                                "--git-dir",
                                &git_common_dir,
                                "symbolic-ref",
                                "refs/remotes/origin/HEAD",
                            ],
                            GitOptions {
                                allow_non_zero_exit: true,
                                ..GitOptions::default()
                            },
                        )
                        .await?;
                    Ok(if result.exit_code == 0 {
                        parse_default_branch_from_remote_head_ref(&result.stdout, "origin")
                    } else {
                        None
                    })
                },
                |result| {
                    if result.is_ok() {
                        ttl
                    } else {
                        std::time::Duration::ZERO
                    }
                },
            )
            .await
            .unwrap_or(None)
    }

    /// Whether `origin` exists, cached per repository for five minutes.
    async fn origin_exists_cached(
        &self,
        cwd: &Path,
        paths: Option<&crate::state::RepositoryPaths>,
    ) -> bool {
        let Some(paths) = paths else {
            return self.remote_exists(cwd, "origin").await.unwrap_or(false);
        };
        let git_common_dir = paths.git_common_dir.clone();
        let fetch_cwd = crate::refs::fetch_cwd(&git_common_dir);
        let ttl = self.state.timings.status_origin_exists_ttl;
        self.state
            .origin_exists
            .get(
                git_common_dir.clone(),
                async {
                    let result = self
                        .run(
                            "GitVcsDriver.statusDetails.originExists",
                            &fetch_cwd,
                            ["--git-dir", &git_common_dir, "remote", "get-url", "origin"],
                            GitOptions {
                                allow_non_zero_exit: true,
                                ..GitOptions::default()
                            },
                        )
                        .await?;
                    Ok(result.exit_code == 0)
                },
                |result| {
                    if result.is_ok() {
                        ttl
                    } else {
                        std::time::Duration::ZERO
                    }
                },
            )
            .await
            .unwrap_or(false)
    }

    /// Changed-line counts against HEAD, by plumbing: porcelain `git diff` rewrites the index
    /// even with GIT_OPTIONAL_LOCKS=0. `-M` keeps porcelain's rename detection.
    async fn read_numstat(&self, cwd: &Path) -> Result<String, GitCommandError> {
        let args = ["diff-index", "-M", "--numstat", "HEAD", "--"];
        let result = self
            .run("GitVcsDriver.statusDetails.numstat", cwd, args, stable())
            .await?;
        if result.exit_code == 0 {
            return Ok(result.stdout);
        }
        if is_unborn_head_stderr(&result.stderr) {
            // No HEAD yet: staged and unstaged counts are summed per path.
            let unstaged = self
                .run(
                    "GitVcsDriver.statusDetails.numstat.unborn",
                    cwd,
                    ["diff-files", "--numstat"],
                    GitOptions::default(),
                )
                .await?;
            let staged = self
                .run(
                    "GitVcsDriver.statusDetails.numstat.unborn.staged",
                    cwd,
                    ["diff", "--cached", "--numstat"],
                    GitOptions::default(),
                )
                .await?;
            let mut totals: Vec<(String, u64, u64)> = Vec::new();
            for entry in parse_numstat_entries(&staged.stdout)
                .into_iter()
                .chain(parse_numstat_entries(&unstaged.stdout))
            {
                match totals.iter_mut().find(|(path, ..)| *path == entry.path) {
                    Some(total) => {
                        total.1 += entry.insertions;
                        total.2 += entry.deletions;
                    }
                    None => totals.push((entry.path, entry.insertions, entry.deletions)),
                }
            }
            return Ok(totals
                .iter()
                .map(|(path, added, removed)| format!("{added}\t{removed}\t{path}"))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        Err(GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(utf16_len(&result.stdout)),
            stderr_length: Some(utf16_len(&result.stderr)),
            ..GitCommandError::new(
                "GitVcsDriver.statusDetails.numstat",
                cwd,
                &args,
                "git diff-index HEAD --numstat failed.",
            )
        })
    }

    /// The local status after a throttled refresh of the upstream's remote-tracking refs
    /// (`statusDetails`).
    pub async fn status_details(&self, cwd: &Path) -> Result<GitStatusDetails, GitCommandError> {
        self.refresh_status_upstream_if_stale(cwd).await;
        self.status_details_local(cwd, LocalStatusOptions::default())
            .await
    }

    /// `status`: `statusDetails` in the shape of `VcsStatusResult`, without a pull request.
    pub async fn status(&self, cwd: &Path) -> Result<VcsStatus, GitCommandError> {
        let details = self.status_details(cwd).await?;
        Ok(VcsStatus {
            is_repo: details.is_repo,
            source_control_provider: None,
            has_primary_remote: details.has_origin_remote,
            is_default_ref: details.is_default_branch,
            ref_name: details.branch,
            has_working_tree_changes: details.has_working_tree_changes,
            working_tree: details.working_tree,
            branch_changes: None,
            has_upstream: details.has_upstream,
            ahead_count: details.ahead_count,
            behind_count: details.behind_count,
            ahead_of_default_count: Some(details.ahead_of_default_count),
            pr: None,
        })
    }
}
