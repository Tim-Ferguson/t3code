//! Repository detection with caching (port of `vcs/VcsDriverRegistry.ts`). Only the git driver
//! exists, so the registry is a cache in front of `GitVcs::detect_repository` plus the kind
//! selection of `.t3code/vcs.json`.
use crate::{vcs_cache::TtlCache, vcs_error::VcsRpcError, vcs_worktree::resolve_path};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use t3_vcs::{Clock, GitVcs, VcsDriverKind, VcsError, git::VcsRepositoryIdentity, project_config};

const DETECTION_CACHE_CAPACITY: usize = 2_048;
/// Status polls detect every open cwd each tick, and detection costs three git processes.
/// `detect` re-checks each hit against the disk, so this only bounds rare changes those
/// stats cannot see.
const DETECTION_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct VcsHandle {
    pub kind: VcsDriverKind,
    pub repository: VcsRepositoryIdentity,
}

#[derive(Clone)]
struct Detected {
    handle: VcsHandle,
    /// Whether `.git` existed in the cwd and in each folder below the repository root.
    git_entries: Vec<bool>,
}

type Cached = Option<Arc<Detected>>;

pub struct VcsRegistry {
    git: GitVcs,
    cache: TtlCache<String, Cached, VcsError>,
}

/// The cwd and each parent below the repository root. A new `.git` in any of them means `git
/// init` made a nested repository. Falls back to the cwd alone when the root is not above it,
/// such as a cwd reached through a symlink.
fn folders_below_root(cwd: &Path, root: &str) -> Vec<PathBuf> {
    let (cwd_path, root_path) = (resolve_path(cwd), resolve_path(Path::new(root)));
    let Ok(relative) = cwd_path.strip_prefix(&root_path) else {
        return vec![cwd.to_path_buf()];
    };
    if relative.to_string_lossy().starts_with("..") {
        return vec![cwd.to_path_buf()];
    }
    let levels = relative.components().count().max(1);
    (0..levels)
        .map(|up| {
            let mut folder = cwd_path.clone();
            for _ in 0..up {
                folder.pop();
            }
            folder
        })
        .collect()
}

struct DiskState {
    git_entries: Vec<bool>,
    repository_on_disk: bool,
}

/// What `detect` re-checks on every cache hit, using stats instead of git. `None` when a stat
/// failed outright, which counts as stale.
async fn read_disk_state(cwd: &Path, repository: &VcsRepositoryIdentity) -> Option<DiskState> {
    let mut git_entries = Vec::new();
    for folder in folders_below_root(cwd, &repository.root_path) {
        git_entries.push(tokio::fs::try_exists(folder.join(".git")).await.ok()?);
    }
    let mut targets = vec![cwd.to_path_buf()];
    if let Some(metadata) = &repository.metadata_path {
        targets.push(resolve_path(&cwd.join(metadata)));
    }
    let mut repository_on_disk = true;
    for target in targets {
        repository_on_disk &= tokio::fs::try_exists(target).await.ok()?;
    }
    Some(DiskState {
        git_entries,
        repository_on_disk,
    })
}

impl VcsRegistry {
    pub fn new(git: GitVcs, clock: Arc<dyn Clock>) -> Self {
        Self {
            git,
            cache: TtlCache::new(DETECTION_CACHE_CAPACITY, clock),
        }
    }

    /// `get`: the driver for a concrete kind. Only git is registered.
    pub fn driver(&self, kind: VcsDriverKind) -> Result<&GitVcs, VcsError> {
        match kind {
            VcsDriverKind::Git => Ok(&self.git),
            _ => Err(VcsError::UnsupportedOperation {
                operation: "VcsDriverRegistry.get".into(),
                kind,
                detail: format!("No {kind} VCS driver is registered."),
            }),
        }
    }

    async fn detect_resolved_kind(
        &self,
        cwd: &Path,
        requested: Option<VcsDriverKind>,
    ) -> Result<Option<VcsHandle>, VcsError> {
        let kind = match requested {
            Some(VcsDriverKind::Unknown) | None => VcsDriverKind::Git,
            Some(kind) => kind,
        };
        let driver = self.driver(kind)?;
        Ok(driver
            .detect_repository(cwd)
            .await?
            .map(|repository| VcsHandle { kind, repository }))
    }

    async fn load(&self, cwd: &Path, requested: Option<VcsDriverKind>) -> Result<Cached, VcsError> {
        let Some(handle) = self.detect_resolved_kind(cwd, requested).await? else {
            return Ok(None);
        };
        let disk = read_disk_state(cwd, &handle.repository).await;
        Ok(Some(Arc::new(Detected {
            handle,
            git_entries: disk.map(|disk| disk.git_entries).unwrap_or_default(),
        })))
    }

    fn ttl(result: &Result<Cached, VcsError>) -> Duration {
        match result {
            Ok(Some(_)) => DETECTION_CACHE_TTL,
            _ => Duration::ZERO,
        }
    }

    /// The repository at `cwd`, or `None`. A hit is stale when its worktree or `.git` folder
    /// was removed, or when `git init` created a nested repository between the cwd and its
    /// root. `requested` is the explicit kind; `None` is auto.
    pub async fn detect(
        &self,
        cwd: &str,
        requested: Option<VcsDriverKind>,
    ) -> Result<Option<VcsHandle>, VcsError> {
        let path = Path::new(cwd);
        let kind = project_config::resolve_kind(path, requested);
        let key = format!("{}\0{cwd}", kind.map_or("auto", VcsDriverKind::as_str));
        let cached = self
            .cache
            .get(key.clone(), self.load(path, kind), Self::ttl)
            .await?;
        let Some(cached) = cached else {
            return Ok(None);
        };
        if let Some(disk) = read_disk_state(path, &cached.handle.repository).await
            && disk.repository_on_disk
            && disk.git_entries == cached.git_entries
        {
            return Ok(Some(cached.handle.clone()));
        }
        self.cache.invalidate(&key);
        Ok(self
            .cache
            .get(key, self.load(path, kind), Self::ttl)
            .await?
            .map(|detected| detected.handle.clone()))
    }

    /// `detect`, failing when there is no repository.
    pub async fn resolve(
        &self,
        cwd: &str,
        requested: Option<VcsDriverKind>,
    ) -> Result<VcsHandle, VcsError> {
        if let Some(handle) = self.detect(cwd, requested).await? {
            return Ok(handle);
        }
        Err(VcsError::UnsupportedOperation {
            operation: "VcsDriverRegistry.resolve".into(),
            kind: requested.unwrap_or(VcsDriverKind::Unknown),
            detail: match requested {
                None => format!("No supported VCS repository was detected at {cwd}."),
                Some(kind) => format!("No {kind} repository was detected at {cwd}."),
            },
        })
    }

    /// `ensureGitCommand`: the repository at `cwd` must exist and be a git one. Failures are
    /// the `GitCommandError` of a command that never reached git (`command: "vcs-route"`).
    pub async fn ensure_git_command(&self, operation: &str, cwd: &str) -> Result<(), VcsRpcError> {
        let handle = self.resolve(cwd, None).await.map_err(|cause| {
            VcsRpcError::route(
                operation,
                cwd,
                "Failed to resolve the VCS driver for this Git command.",
                Some(&cause),
            )
        })?;
        if handle.kind != VcsDriverKind::Git {
            return Err(VcsRpcError::route(
                operation,
                cwd,
                format!(
                    "The {operation} command currently supports Git repositories only; detected {}.",
                    handle.kind
                ),
                None,
            ));
        }
        Ok(())
    }
}
