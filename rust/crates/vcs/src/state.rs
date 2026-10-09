//! What a `GitCore` remembers between calls (the caches of `makeGitVcsDriverCore`): resolved
//! repository paths, the default branch and `origin` existence per repository, the throttled
//! background upstream fetch and its failure back-off, and the ref snapshots `listRefs` pages
//! through. Every lifetime is a field of `GitTimings`, so tests can shorten or cross them.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{cache::TtlCache, clock::Clock, core::GitCommandError, wire::VcsRef};

#[derive(Debug, Clone)]
pub struct GitTimings {
    /// How long a background upstream fetch suppresses the next one for the same remote.
    pub status_upstream_refresh_interval: Duration,
    pub status_upstream_refresh_timeout: Duration,
    /// Back-off after a failed background fetch: the base doubles per consecutive failure.
    pub status_upstream_refresh_failure_base_cooldown: Duration,
    pub status_upstream_refresh_failure_max_cooldown: Duration,
    pub repository_paths_ttl: Duration,
    /// How long a forced re-resolution of repository paths is reused.
    pub repository_paths_refresh_coalesce_ttl: Duration,
    pub non_repository_paths_ttl: Duration,
    pub list_refs_snapshot_ttl: Duration,
    pub list_refs_refresh_coalesce_ttl: Duration,
    pub list_refs_refresh_failure_cooldown: Duration,
    pub status_default_branch_ttl: Duration,
    pub status_origin_exists_ttl: Duration,
}

impl Default for GitTimings {
    fn default() -> Self {
        Self {
            status_upstream_refresh_interval: Duration::from_secs(15),
            status_upstream_refresh_timeout: Duration::from_secs(5),
            status_upstream_refresh_failure_base_cooldown: Duration::from_secs(30),
            status_upstream_refresh_failure_max_cooldown: Duration::from_secs(15 * 60),
            repository_paths_ttl: Duration::from_secs(10 * 60),
            repository_paths_refresh_coalesce_ttl: Duration::from_secs(5),
            non_repository_paths_ttl: Duration::from_secs(1),
            list_refs_snapshot_ttl: Duration::from_secs(2 * 60),
            list_refs_refresh_coalesce_ttl: Duration::from_secs(5),
            list_refs_refresh_failure_cooldown: Duration::from_secs(30),
            status_default_branch_ttl: Duration::from_secs(5 * 60),
            status_origin_exists_ttl: Duration::from_secs(5 * 60),
        }
    }
}

const STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY: usize = 2_048;
const REPOSITORY_PATHS_CACHE_CAPACITY: usize = 2_048;
const STATUS_STATIC_CACHE_CAPACITY: usize = 2_048;
const LIST_REFS_SNAPSHOT_CACHE_CAPACITY: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryPaths {
    pub git_common_dir: String,
    pub worktree_root: Option<String>,
    pub current_branch: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GitRefsSnapshot {
    pub local_branches: Vec<VcsRef>,
    pub remote_branches: Vec<VcsRef>,
    pub has_primary_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SnapshotKey {
    pub git_common_dir: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RefreshKey {
    pub git_common_dir: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct StatusRemoteKey {
    pub git_common_dir: String,
    pub remote_name: String,
}

/// A map that forgets its oldest entry past `capacity`; re-inserting a key makes it newest.
struct BoundedMap<V> {
    capacity: usize,
    entries: HashMap<String, (V, u64)>,
    sequence: u64,
}

impl<V: Copy> BoundedMap<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            sequence: 0,
        }
    }

    fn get(&self, key: &str) -> Option<V> {
        self.entries.get(key).map(|(value, _)| *value)
    }

    fn insert(&mut self, key: &str, value: V) {
        self.sequence += 1;
        self.entries.insert(key.to_string(), (value, self.sequence));
        if self.entries.len() > self.capacity
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, sequence))| *sequence)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// The counters `resolveListRefsSnapshot` uses to tell a snapshot taken before an
/// invalidation from one taken after: `epoch` names the snapshot a repository is currently
/// served, `generation` changes whenever the repository was invalidated mid-read.
pub(crate) struct RefsCounters {
    epochs: BoundedMap<u64>,
    epoch_sequence: u64,
    generations: BoundedMap<u64>,
    generation_sequence: u64,
}

impl RefsCounters {
    pub(crate) fn epoch(&self, git_common_dir: &str) -> Option<u64> {
        self.epochs.get(git_common_dir)
    }

    pub(crate) fn bump_epoch(&mut self, git_common_dir: &str) -> u64 {
        self.epoch_sequence += 1;
        self.epochs.insert(git_common_dir, self.epoch_sequence);
        self.epoch_sequence
    }

    /// The generation of a repository, assigned on first use.
    pub(crate) fn current_generation(&mut self, git_common_dir: &str) -> u64 {
        let generation = self.generations.get(git_common_dir).unwrap_or_else(|| {
            self.generation_sequence += 1;
            self.generation_sequence
        });
        self.generations.insert(git_common_dir, generation);
        generation
    }

    pub(crate) fn bump_generation(&mut self, git_common_dir: &str) -> u64 {
        self.generation_sequence += 1;
        self.generations
            .insert(git_common_dir, self.generation_sequence);
        self.generation_sequence
    }
}

pub(crate) struct CoreState {
    pub clock: Arc<dyn Clock>,
    pub timings: GitTimings,
    pub repository_paths: TtlCache<String, Option<RepositoryPaths>, GitCommandError>,
    pub repository_paths_refresh: TtlCache<String, Option<RepositoryPaths>, GitCommandError>,
    pub default_branch: TtlCache<String, Option<String>, GitCommandError>,
    pub origin_exists: TtlCache<String, bool, GitCommandError>,
    pub status_remote_refresh: TtlCache<StatusRemoteKey, bool, GitCommandError>,
    refresh_failures: Mutex<BoundedMap<u32>>,
    pub snapshots: TtlCache<SnapshotKey, Arc<GitRefsSnapshot>, GitCommandError>,
    pub snapshot_refresh: TtlCache<RefreshKey, Arc<GitRefsSnapshot>, GitCommandError>,
    pub refs: Mutex<RefsCounters>,
}

impl CoreState {
    pub(crate) fn new(clock: Arc<dyn Clock>, timings: GitTimings) -> Self {
        Self {
            repository_paths: TtlCache::new(REPOSITORY_PATHS_CACHE_CAPACITY, clock.clone()),
            repository_paths_refresh: TtlCache::new(REPOSITORY_PATHS_CACHE_CAPACITY, clock.clone()),
            default_branch: TtlCache::new(STATUS_STATIC_CACHE_CAPACITY, clock.clone()),
            origin_exists: TtlCache::new(STATUS_STATIC_CACHE_CAPACITY, clock.clone()),
            status_remote_refresh: TtlCache::new(
                STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY,
                clock.clone(),
            ),
            refresh_failures: Mutex::new(BoundedMap::new(STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY)),
            snapshots: TtlCache::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY, clock.clone()),
            snapshot_refresh: TtlCache::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY, clock.clone()),
            refs: Mutex::new(RefsCounters {
                epochs: BoundedMap::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY),
                epoch_sequence: 0,
                generations: BoundedMap::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY),
                generation_sequence: 0,
            }),
            clock,
            timings,
        }
    }

    fn failure_key(key: &StatusRemoteKey) -> String {
        format!("{}\0{}", key.git_common_dir, key.remote_name)
    }

    pub(crate) fn record_refresh_failure(&self, key: &StatusRemoteKey) {
        let mut failures = self.refresh_failures.lock().unwrap();
        let key = Self::failure_key(key);
        let count = failures.get(&key).unwrap_or(0) + 1;
        failures.remove(&key);
        failures.insert(&key, count);
    }

    pub(crate) fn clear_refresh_failures(&self, key: &StatusRemoteKey) {
        self.refresh_failures
            .lock()
            .unwrap()
            .remove(&Self::failure_key(key));
    }

    pub(crate) fn refresh_failure_cooldown(&self, key: &StatusRemoteKey) -> Duration {
        let failures = self
            .refresh_failures
            .lock()
            .unwrap()
            .get(&Self::failure_key(key))
            .unwrap_or(1);
        status_upstream_refresh_failure_cooldown(&self.timings, failures)
    }
}

/// How long a failed background fetch suppresses the next attempt: the base cooldown doubled
/// per consecutive failure, up to the maximum.
pub fn status_upstream_refresh_failure_cooldown(
    timings: &GitTimings,
    consecutive_failures: u32,
) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(32);
    timings
        .status_upstream_refresh_failure_base_cooldown
        .saturating_mul(2u32.saturating_pow(exponent))
        .min(timings.status_upstream_refresh_failure_max_cooldown)
}
