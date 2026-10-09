//! Repository path resolution, ref listing and the cache invalidation that keeps them honest
//! (the `resolveRepositoryPaths*`, `readGitRefsSnapshot`, `listRefs` and
//! `withListRefsInvalidation` parts of `vcs/GitVcsDriverCore.ts`).
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    checkpoint::locale_compare,
    core::{
        GitCommandError, GitCore, GitOptions, GitTimeout, is_non_repository_stderr,
        parse_remote_names_in_git_order, parse_remote_ref_with_remote_names, sorted_longest_first,
        utf16_len,
    },
    remote_refs::dedupe_remote_branches_with_local_matches,
    state::{GitRefsSnapshot, RefreshKey, RepositoryPaths, SnapshotKey},
    wire::{RefKind, VcsListRefsInput, VcsListRefsResult, VcsRef},
};

const GIT_LIST_BRANCHES_DEFAULT_LIMIT: usize = 100;

/// `path.normalize(path.resolve(base, path))` without touching the filesystem.
pub(crate) fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let joined = base.join(path);
    let absolute = std::path::absolute(&joined).unwrap_or(joined);
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn normalize_cwd(cwd: &Path) -> String {
    resolve_path(Path::new("."), cwd)
        .to_string_lossy()
        .into_owned()
}

/// The directory git commands for a repository known by its common directory run from.
pub(crate) fn fetch_cwd(git_common_dir: &str) -> PathBuf {
    let dir = Path::new(git_common_dir);
    if dir.file_name().is_some_and(|name| name == ".git") {
        dir.parent().unwrap_or(dir).to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

/// `git worktree list --porcelain -z`: branch name to checkout path, skipping prunable
/// records. A branch listed twice keeps its last path.
pub fn parse_worktree_branch_paths(stdout: &str) -> Vec<(String, String)> {
    let mut paths: Vec<(String, String)> = Vec::new();
    let (mut current_path, mut current_branch, mut prunable) = (None, None, false);
    let mut flush =
        |path: &mut Option<String>, branch: &mut Option<String>, prunable: &mut bool| {
            if let (Some(path), Some(branch), false) = (path.take(), branch.take(), *prunable) {
                match paths.iter_mut().find(|(name, _)| *name == branch) {
                    Some(entry) => entry.1 = path,
                    None => paths.push((branch, path)),
                }
            }
            *prunable = false;
            (*path, *branch) = (None, None);
        };
    for field in stdout.split('\0') {
        if field.is_empty() {
            flush(&mut current_path, &mut current_branch, &mut prunable);
        } else if let Some(path) = field.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_string());
        } else if field == "prunable" || field.starts_with("prunable ") {
            prunable = true;
        }
    }
    flush(&mut current_path, &mut current_branch, &mut prunable);
    paths
}

struct RankedRef {
    reference: VcsRef,
    last_commit: i64,
}

fn by_recency_then_name(left: &RankedRef, right: &RankedRef) -> std::cmp::Ordering {
    right
        .last_commit
        .cmp(&left.last_commit)
        .then_with(|| locale_compare(&left.reference.name, &right.reference.name))
}

fn stable_diagnostics(timeout: Duration) -> GitOptions {
    GitOptions {
        env: vec![("LC_ALL".into(), Some("C".into()))],
        timeout: GitTimeout::After(timeout),
        ..GitOptions::default()
    }
}

/// `Number.parseInt(text, 10)` for a leading integer.
pub(crate) fn parse_leading_int(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let end = text
        .char_indices()
        .find(|(index, c)| !(c.is_ascii_digit() || (*index == 0 && (*c == '-' || *c == '+'))))
        .map_or(text.len(), |(index, _)| index);
    text[..end].parse().ok()
}

impl GitCore {
    pub(crate) async fn resolve_repository_paths_uncached(
        &self,
        cwd: &Path,
    ) -> Result<Option<RepositoryPaths>, GitCommandError> {
        let common_args = ["rev-parse", "--git-common-dir"];
        let common = self
            .run(
                "GitVcsDriver.resolveRepositoryPaths.commonDir",
                cwd,
                common_args,
                GitOptions {
                    allow_non_zero_exit: true,
                    ..stable_diagnostics(Duration::from_secs(5))
                },
            )
            .await?;
        if common.exit_code != 0 {
            if is_non_repository_stderr(common.stderr.trim()) {
                return Ok(None);
            }
            let args = common_args.map(String::from);
            return Err(GitCommandError {
                exit_code: Some(common.exit_code),
                stdout_length: Some(utf16_len(&common.stdout)),
                stderr_length: Some(utf16_len(&common.stderr)),
                ..GitCommandError::new(
                    "GitVcsDriver.resolveRepositoryPaths.commonDir",
                    cwd,
                    &args,
                    "Failed to resolve the Git common directory.",
                )
            });
        }
        let resolved = resolve_path(cwd, Path::new(common.stdout.trim()));
        let git_common_dir = tokio::fs::canonicalize(&resolved)
            .await
            .unwrap_or(resolved)
            .to_string_lossy()
            .into_owned();
        let allow_failure = || GitOptions::allow_failure(Duration::from_secs(5));
        let (worktree_root, current_branch) = tokio::try_join!(
            self.run(
                "GitVcsDriver.resolveRepositoryPaths.worktreeRoot",
                cwd,
                ["rev-parse", "--show-toplevel"],
                allow_failure(),
            ),
            self.run(
                "GitVcsDriver.resolveRepositoryPaths.currentBranch",
                cwd,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
                allow_failure(),
            ),
        )?;
        let root = worktree_root.stdout.trim();
        let branch = current_branch.stdout.trim();
        Ok(Some(RepositoryPaths {
            git_common_dir,
            worktree_root: (worktree_root.exit_code == 0 && !root.is_empty()).then(|| {
                resolve_path(cwd, Path::new(root))
                    .to_string_lossy()
                    .into_owned()
            }),
            current_branch: (current_branch.exit_code == 0 && !branch.is_empty())
                .then(|| branch.to_string()),
        }))
    }

    /// Where `cwd` keeps its repository: cached for ten minutes (one second for a directory
    /// that is not a repository). `refresh` re-resolves, but at most once per few seconds.
    pub(crate) async fn resolve_repository_paths(
        &self,
        cwd: &Path,
        refresh: bool,
    ) -> Result<Option<RepositoryPaths>, GitCommandError> {
        let state = &self.state;
        let key = normalize_cwd(cwd);
        let ttl = |long: Duration| {
            move |result: &Result<Option<RepositoryPaths>, GitCommandError>| match result {
                Ok(None) => state.timings.non_repository_paths_ttl,
                Ok(Some(_)) => long,
                Err(_) => Duration::ZERO,
            }
        };
        let cached = |key: String| {
            let load_from = PathBuf::from(&key);
            state.repository_paths.get(
                key,
                async move { self.resolve_repository_paths_uncached(&load_from).await },
                ttl(state.timings.repository_paths_ttl),
            )
        };
        if !refresh {
            return cached(key).await;
        }
        state
            .repository_paths_refresh
            .get(
                key.clone(),
                async {
                    state.repository_paths.invalidate(&key);
                    cached(key.clone()).await
                },
                ttl(state.timings.repository_paths_refresh_coalesce_ttl),
            )
            .await
    }

    pub(crate) fn invalidate_repository_paths(&self, cwd: &Path) {
        let key = normalize_cwd(cwd);
        self.state.repository_paths_refresh.invalidate(&key);
        self.state.repository_paths.invalidate(&key);
    }

    /// Everything the cached ref and status views of `cwd`'s repository may no longer reflect
    /// after a command that changes refs, remotes or worktrees.
    pub(crate) async fn invalidate_after_mutation(&self, cwd: &Path) {
        let _ = self.invalidate_list_refs_snapshot(cwd).await;
        self.invalidate_status_static_caches(cwd).await;
    }

    async fn invalidate_list_refs_snapshot(&self, cwd: &Path) -> Result<(), GitCommandError> {
        let Some(paths) = self.resolve_repository_paths(cwd, false).await? else {
            return Ok(());
        };
        let state = &self.state;
        let git_common_dir = paths.git_common_dir;
        let previous_generation = {
            let mut counters = state.refs.lock().unwrap();
            let previous = counters.current_generation(&git_common_dir);
            counters.bump_generation(&git_common_dir);
            counters.bump_epoch(&git_common_dir);
            previous
        };
        state.snapshot_refresh.invalidate(&RefreshKey {
            git_common_dir,
            generation: previous_generation,
        });
        self.invalidate_repository_paths(cwd);
        Ok(())
    }

    async fn invalidate_status_static_caches(&self, cwd: &Path) {
        let key = match self.resolve_repository_paths(cwd, false).await {
            Ok(Some(paths)) => paths.git_common_dir,
            _ => normalize_cwd(cwd),
        };
        self.state.default_branch.invalidate(&key);
        self.state.origin_exists.invalidate(&key);
    }

    async fn read_git_refs_snapshot(
        &self,
        git_common_dir: &str,
    ) -> Result<GitRefsSnapshot, GitCommandError> {
        let cwd = fetch_cwd(git_common_dir);
        let git_dir = |args: &[&str]| -> Vec<String> {
            ["--git-dir", git_common_dir]
                .into_iter()
                .chain(args.iter().copied())
                .map(String::from)
                .collect()
        };
        let large = |options: GitOptions| GitOptions {
            max_output_bytes: Some(16 * 1024 * 1024),
            ..options
        };
        let (refs, default_ref, worktrees, remotes) = tokio::try_join!(
            self.run(
                "GitVcsDriver.listRefs.snapshotRefs",
                &cwd,
                git_dir(&[
                    "for-each-ref",
                    "--format=%(refname)%09%(committerdate:unix)%09%(symref)",
                    "refs/heads",
                    "refs/remotes",
                ]),
                large(stable_diagnostics(Duration::from_secs(30)))
                    .detail("Git ref snapshot enumeration failed."),
            ),
            self.run(
                "GitVcsDriver.listRefs.defaultRef",
                &cwd,
                git_dir(&["symbolic-ref", "refs/remotes/origin/HEAD"]),
                GitOptions::allow_failure(Duration::from_secs(5)),
            ),
            self.run(
                "GitVcsDriver.listRefs.worktreeList",
                &cwd,
                git_dir(&["worktree", "list", "--porcelain", "-z"]),
                large(GitOptions::allow_failure(Duration::from_secs(30))),
            ),
            self.run(
                "GitVcsDriver.listRefs.remoteNames",
                &cwd,
                git_dir(&["remote"]),
                GitOptions::allow_failure(Duration::from_secs(5)),
            ),
        )?;

        let remote_names = if remotes.exit_code == 0 {
            sorted_longest_first(parse_remote_names_in_git_order(&remotes.stdout))
        } else {
            Vec::new()
        };
        let default_branch = (default_ref.exit_code == 0).then(|| {
            let text = default_ref.stdout.trim();
            text.strip_prefix("refs/remotes/origin/")
                .unwrap_or(text)
                .to_string()
        });
        let worktree_paths: HashMap<String, String> = if worktrees.exit_code == 0 {
            parse_worktree_branch_paths(&worktrees.stdout)
                .into_iter()
                .map(|(branch, path)| {
                    let path = resolve_path(Path::new("."), Path::new(&path));
                    (branch, path)
                })
                .filter(|(_, path)| std::fs::metadata(path).is_ok())
                .map(|(branch, path)| (branch, path.to_string_lossy().into_owned()))
                .collect()
        } else {
            HashMap::new()
        };

        let (mut local, mut remote): (Vec<RankedRef>, Vec<RankedRef>) = (Vec::new(), Vec::new());
        for line in refs.stdout.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split('\t');
            let full_name = fields.next().unwrap_or("");
            let last_commit = fields.next().and_then(parse_leading_int).unwrap_or(0);
            let symbolic_target = fields.next().unwrap_or("");
            if full_name.is_empty() || !symbolic_target.is_empty() {
                continue;
            }
            if let Some(name) = full_name.strip_prefix("refs/heads/") {
                local.push(RankedRef {
                    reference: VcsRef {
                        name: name.to_string(),
                        current: false,
                        is_remote: false,
                        is_default: default_branch.as_deref() == Some(name),
                        worktree_path: worktree_paths.get(name).cloned(),
                        remote_name: None,
                    },
                    last_commit,
                });
            } else if let Some(name) = full_name.strip_prefix("refs/remotes/") {
                let parsed = parse_remote_ref_with_remote_names(name, &remote_names);
                remote.push(RankedRef {
                    reference: VcsRef {
                        name: name.to_string(),
                        current: false,
                        is_remote: true,
                        is_default: matches!(
                            (&default_branch, &parsed),
                            (Some(default), Some((remote_name, branch)))
                                if remote_name == "origin" && branch == default
                        ),
                        worktree_path: None,
                        remote_name: parsed.map(|(remote_name, _)| remote_name),
                    },
                    last_commit,
                });
            }
        }
        local.sort_by(by_recency_then_name);
        remote.sort_by(by_recency_then_name);
        Ok(GitRefsSnapshot {
            local_branches: local.into_iter().map(|ranked| ranked.reference).collect(),
            remote_branches: remote.into_iter().map(|ranked| ranked.reference).collect(),
            has_primary_remote: remote_names.iter().any(|name| name == "origin"),
        })
    }

    /// The snapshot `listRefs` pages through. A forced refresh (or the first read of a
    /// repository) goes through a coalescing cache that bumps the epoch, so a snapshot read
    /// stays singleflight for the whole git scan and a failed scan is not retried for a
    /// cooldown. If the repository is invalidated while a snapshot loads, the read starts over.
    async fn resolve_list_refs_snapshot(
        &self,
        git_common_dir: &str,
        refresh: bool,
    ) -> Result<Arc<GitRefsSnapshot>, GitCommandError> {
        let state = &self.state;
        let timings = &state.timings;
        let snapshot_ttl = |result: &Result<Arc<GitRefsSnapshot>, GitCommandError>| {
            if result.is_ok() {
                timings.list_refs_snapshot_ttl
            } else {
                Duration::ZERO
            }
        };
        let read_epoch = |epoch: u64| {
            state.snapshots.get(
                SnapshotKey {
                    git_common_dir: git_common_dir.to_string(),
                    epoch,
                },
                async move {
                    self.read_git_refs_snapshot(git_common_dir)
                        .await
                        .map(Arc::new)
                },
                snapshot_ttl,
            )
        };
        loop {
            let (generation, current_epoch) = {
                let mut counters = state.refs.lock().unwrap();
                (
                    counters.current_generation(git_common_dir),
                    counters.epoch(git_common_dir),
                )
            };
            let snapshot = match current_epoch {
                Some(epoch) if !refresh => read_epoch(epoch).await?,
                _ => {
                    state
                        .snapshot_refresh
                        .get(
                            RefreshKey {
                                git_common_dir: git_common_dir.to_string(),
                                generation,
                            },
                            async {
                                let epoch = state.refs.lock().unwrap().bump_epoch(git_common_dir);
                                read_epoch(epoch).await
                            },
                            |result| match result {
                                Ok(_) => timings.list_refs_refresh_coalesce_ttl,
                                Err(_) => timings.list_refs_refresh_failure_cooldown,
                            },
                        )
                        .await?
                }
            };
            if state
                .refs
                .lock()
                .unwrap()
                .current_generation(git_common_dir)
                == generation
            {
                return Ok(snapshot);
            }
        }
    }

    /// Branches for the ref picker: current and default first, then by recency, remote
    /// copies of local branches hidden unless asked for. A directory that is gone yields an
    /// empty non-repository result.
    pub async fn list_refs(
        &self,
        input: &VcsListRefsInput,
    ) -> Result<VcsListRefsResult, GitCommandError> {
        let cwd = Path::new(&input.cwd);
        let refresh = input.refresh == Some(true);
        let paths = match self.resolve_repository_paths(cwd, refresh).await {
            Ok(paths) => paths,
            Err(error) if self.is_missing_cwd(&error, cwd) => None,
            Err(error) => return Err(error),
        };
        let Some(paths) = paths else {
            return Ok(VcsListRefsResult {
                refs: Vec::new(),
                is_repo: false,
                has_primary_remote: false,
                next_cursor: None,
                total_count: 0,
            });
        };
        let snapshot = self
            .resolve_list_refs_snapshot(&paths.git_common_dir, refresh)
            .await?;
        let has_current_worktree_branch = paths.worktree_root.is_some()
            && snapshot
                .local_branches
                .iter()
                .any(|reference| reference.worktree_path == paths.worktree_root);
        let local = snapshot.local_branches.iter().map(|reference| VcsRef {
            current: if has_current_worktree_branch {
                reference.worktree_path == paths.worktree_root
            } else {
                paths.current_branch.as_deref() == Some(reference.name.as_str())
            },
            ..reference.clone()
        });
        let mut combined: Vec<VcsRef> = local
            .chain(snapshot.remote_branches.iter().cloned())
            .collect();
        if input.include_matching_remote_refs != Some(true) {
            combined = dedupe_remote_branches_with_local_matches(&combined);
        }
        // Keep current/default refs on the first page even when the default only exists as
        // origin/<default> (remote refs sort after all locals).
        combined.sort_by_key(|reference| {
            if reference.current {
                0
            } else if reference.is_default {
                1
            } else {
                2
            }
        });
        let query = input
            .query
            .as_deref()
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let matching: Vec<VcsRef> = combined
            .into_iter()
            .filter(|reference| match input.ref_kind {
                Some(RefKind::Local) => !reference.is_remote,
                Some(RefKind::Remote) => reference.is_remote,
                _ => true,
            })
            .filter(|reference| {
                query
                    .as_ref()
                    .is_none_or(|query| reference.name.to_lowercase().contains(query))
            })
            .collect();
        let total_count = matching.len();
        let cursor = input.cursor.unwrap_or(0);
        let limit = input.limit.unwrap_or(GIT_LIST_BRANCHES_DEFAULT_LIMIT);
        let refs: Vec<VcsRef> = matching.into_iter().skip(cursor).take(limit).collect();
        let next_cursor = (cursor + refs.len() < total_count).then_some(cursor + refs.len());
        Ok(VcsListRefsResult {
            refs,
            is_repo: true,
            has_primary_remote: snapshot.has_primary_remote,
            next_cursor,
            total_count,
        })
    }

    /// A spawn failure because `cwd` does not exist (or is not a directory).
    pub(crate) fn is_missing_cwd(&self, error: &GitCommandError, cwd: &Path) -> bool {
        error.cause.is_some() && !cwd.is_dir()
    }
}
