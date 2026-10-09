//! Git status for the `vcs.*` methods: the status source over `t3_vcs` (the
//! `GitWorkflowService` / `GitManager` status half) and the broadcaster behind
//! `subscribeVcsStatus` and `vcs.refreshStatus` (port of `vcs/VcsStatusBroadcaster.ts`).
//!
//! The broadcaster caches one local and one remote part per working directory, publishes a change
//! only when a part differs from the cached one, and keeps one background poller per directory
//! that has a subscriber asking for remote status. Pull request and source control provider
//! lookups belong to the pull request work and are absent here: `pr` is `null` and no
//! `sourceControlProvider` is reported, as the original does when no provider answers.
use crate::{vcs_cache::TtlCache, vcs_error::VcsRpcError, vcs_registry::VcsRegistry};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_vcs::{
    Clock, GitCommandError, VcsDriverKind,
    core::GitCore,
    status::{LocalStatusOptions, VcsStatus, WorkingTree},
    wire::{VcsPullResult, VcsStatusLocalResult, VcsStatusRemoteResult},
};
use tokio::{sync::broadcast, task::JoinHandle};

const STATUS_RESULT_CACHE_TTL: Duration = Duration::from_secs(1);
const STATUS_RESULT_CACHE_CAPACITY: usize = 2_048;
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const FAILURE_BASE_DELAY: Duration = Duration::from_secs(30);
const FAILURE_MAX_DELAY: Duration = Duration::from_secs(15 * 60);
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// `GitRemoteStatusOptions` plus `refreshMissingPullRequest`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteOptions {
    /// `Some(false)` reads the remote status without first fetching the upstream.
    pub refresh_upstream: Option<bool>,
    /// Retry a cached missing pull request: the lookup is asked again and the result cache bypassed.
    pub refresh_missing_pull_request: bool,
}

/// What the broadcaster needs from git: the original's `GitWorkflowService` status methods.
pub trait StatusSource: Send + Sync + 'static {
    fn local_status(
        &self,
        cwd: &str,
    ) -> impl Future<Output = Result<VcsStatusLocalResult, VcsRpcError>> + Send;
    fn remote_status(
        &self,
        cwd: &str,
        options: RemoteOptions,
    ) -> impl Future<Output = Result<Option<VcsStatusRemoteResult>, VcsRpcError>> + Send;
    fn invalidate_local(&self, cwd: &str) -> impl Future<Output = ()> + Send;
    fn invalidate_remote(&self, cwd: &str) -> impl Future<Output = ()> + Send;
    fn invalidate_status(&self, cwd: &str) -> impl Future<Output = ()> + Send;
    fn pull_current_branch(
        &self,
        cwd: &str,
    ) -> impl Future<Output = Result<VcsPullResult, VcsRpcError>> + Send;
}

// ---------------------------------------------------------------------------------------------
// Status source over t3_vcs
// ---------------------------------------------------------------------------------------------

pub struct GitStatusSource {
    registry: Arc<VcsRegistry>,
    core: GitCore,
    /// The server's settings, once a request has brought them: the pull request lookup reads its
    /// credentials from them, as every other pull request read does.
    settings: std::sync::Mutex<Option<crate::server_settings::WeakSettings>>,
    local: TtlCache<String, VcsStatusLocalResult, VcsRpcError>,
    remote: TtlCache<String, Option<VcsStatusRemoteResult>, VcsRpcError>,
}

fn is_not_git_repository(error: &GitCommandError) -> bool {
    error
        .to_string()
        .to_lowercase()
        .contains("not a git repository")
}

fn non_repository_local() -> VcsStatusLocalResult {
    VcsStatusLocalResult {
        is_repo: false,
        source_control_provider: None,
        has_primary_remote: false,
        is_default_ref: false,
        ref_name: None,
        has_working_tree_changes: false,
        working_tree: WorkingTree::default(),
        branch_changes: None,
    }
}

/// The cache key and the directory git runs in: symlinks resolved when the path exists.
async fn canonical(cwd: &str) -> String {
    tokio::fs::canonicalize(cwd)
        .await
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

fn cache_ttl<T>(result: &Result<T, VcsRpcError>) -> Duration {
    if result.is_ok() {
        STATUS_RESULT_CACHE_TTL
    } else {
        Duration::ZERO
    }
}

impl GitStatusSource {
    pub fn new(registry: Arc<VcsRegistry>, core: GitCore, clock: Arc<dyn Clock>) -> Self {
        Self {
            registry,
            core,
            settings: std::sync::Mutex::new(None),
            local: TtlCache::new(STATUS_RESULT_CACHE_CAPACITY, clock.clone()),
            remote: TtlCache::new(STATUS_RESULT_CACHE_CAPACITY, clock),
        }
    }

    /// Remembers the settings a request came with, so a pull request looked up for a status can
    /// use a token saved there. A request without settings leaves what is known as it was.
    pub fn use_settings(&self, settings: Option<crate::server_settings::SettingsService>) {
        if let Some(settings) = settings {
            // Weakly: this lives as long as the process, and must not keep the settings running.
            *self.settings.lock().unwrap() = Some(settings.downgrade());
        }
    }

    fn host(&self) -> crate::source_control_host::Host {
        crate::source_control_host::Host::process_default().with_settings(self.settings.lock().unwrap().as_ref().and_then(crate::server_settings::WeakSettings::upgrade))
    }

    /// `detectGitRepositoryForStatus`: whether `cwd` is a git repository.
    async fn is_git_repository(&self, operation: &str, cwd: &str) -> Result<bool, VcsRpcError> {
        match self.registry.detect(cwd, None).await {
            Err(cause) => Err(VcsRpcError::manager(
                operation,
                cwd,
                "Failed to detect a VCS repository for this Git workflow.",
                Some(&cause),
            )),
            Ok(None) => Ok(false),
            Ok(Some(handle)) if handle.kind != VcsDriverKind::Git => Err(VcsRpcError::manager(
                operation,
                cwd,
                format!(
                    "The {operation} workflow currently supports Git repositories only; detected {}. ({cwd})",
                    handle.kind
                ),
                None,
            )),
            Ok(Some(_)) => Ok(true),
        }
    }

    async fn read_local(&self, cwd: &str) -> Result<VcsStatusLocalResult, VcsRpcError> {
        let details = match self
            .core
            .status_details_local(
                Path::new(cwd),
                LocalStatusOptions {
                    include_divergence: false,
                    include_branch_changes: true,
                },
            )
            .await
        {
            Ok(details) => details,
            Err(error) if is_not_git_repository(&error) => Default::default(),
            Err(error) => return Err(error.into()),
        };
        let mut local = VcsStatusLocalResult::from(&details);
        if details.is_repo {
            local.source_control_provider =
                crate::source_control_registry::hosting_provider_for_status_with(self.host(), &self.core, cwd, details.branch.as_deref()).await;
        }
        Ok(local)
    }

    async fn read_remote(
        &self,
        cwd: &str,
        refresh_upstream: bool,
        refresh_missing_pull_request: bool,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        let details = match self
            .core
            .status_details_remote(Path::new(cwd), refresh_upstream)
            .await
        {
            Ok(details) => details,
            Err(error) if is_not_git_repository(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !details.is_repo {
            return Ok(None);
        }
        let mut remote = VcsStatusRemoteResult::from(&details);
        if let Some(branch) = details.branch.clone() {
            // A failed lookup keeps the last pull request known for the branch.
            remote.pr = crate::git_workflow_status::StatusPrLookup::shared()
                .lookup(
                    &self.host(),
                    &self.core,
                    cwd,
                    &crate::git_workflow_status::BranchDetails {
                        branch,
                        upstream_ref: details.upstream_ref.clone(),
                        default_branch: details.default_branch.clone(),
                        is_default_branch: details.is_default_branch,
                    },
                    refresh_missing_pull_request,
                )
                .await;
        }
        Ok(Some(remote))
    }
}

impl StatusSource for GitStatusSource {
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, VcsRpcError> {
        if !self
            .is_git_repository("GitWorkflowService.localStatus", cwd)
            .await?
        {
            return Ok(non_repository_local());
        }
        let key = canonical(cwd).await;
        self.local
            .get(key.clone(), self.read_local(&key), cache_ttl)
            .await
    }

    async fn remote_status(
        &self,
        cwd: &str,
        options: RemoteOptions,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        if !self
            .is_git_repository("GitWorkflowService.remoteStatus", cwd)
            .await?
        {
            return Ok(None);
        }
        let key = canonical(cwd).await;
        if options.refresh_upstream == Some(false) || options.refresh_missing_pull_request {
            return self
                .read_remote(&key, options.refresh_upstream != Some(false), options.refresh_missing_pull_request)
                .await;
        }
        self.remote
            .get(key.clone(), self.read_remote(&key, true, false), cache_ttl)
            .await
    }

    async fn invalidate_local(&self, cwd: &str) {
        self.local.invalidate(&canonical(cwd).await);
    }
    async fn invalidate_remote(&self, cwd: &str) {
        self.remote.invalidate(&canonical(cwd).await);
    }
    async fn invalidate_status(&self, cwd: &str) {
        let key = canonical(cwd).await;
        self.local.invalidate(&key);
        self.remote.invalidate(&key);
        // Full invalidation is the explicit-freshness path (git actions, user refresh); it also
        // bypasses the slow pull request lookup cache. The periodic poll keeps that cache warm.
        crate::git_workflow_status::StatusPrLookup::shared().bump_epoch(&key);
    }

    async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, VcsRpcError> {
        self.registry
            .ensure_git_command("GitWorkflowService.pullCurrentBranch", cwd)
            .await?;
        Ok(Box::pin(self.core.pull_current_branch(Path::new(cwd))).await?)
    }
}

// ---------------------------------------------------------------------------------------------
// Broadcaster
// ---------------------------------------------------------------------------------------------

/// What the broadcaster asks of its surroundings. The original reads these from the background
/// policy and settings services.
#[derive(Clone)]
pub struct Hooks {
    /// `automaticGitFetchInterval`; zero turns periodic fetching off.
    pub fetch_interval: Arc<dyn Fn() -> BoxFuture<'static, Duration> + Send + Sync>,
    /// `BackgroundPolicy.shouldRunScopeWork({ type: "vcs-status", cwd })`.
    pub scope_allowed: Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>,
    /// `VcsAutoPullPolicy.isEnabled(cwd)`: the project's `defaultAutoPull`.
    pub auto_pull: Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatusEvent {
    Snapshot {
        local: VcsStatusLocalResult,
        remote: Option<VcsStatusRemoteResult>,
    },
    LocalUpdated(VcsStatusLocalResult),
    RemoteUpdated(Option<VcsStatusRemoteResult>),
}

impl StatusEvent {
    /// `VcsStatusStreamEvent`.
    pub fn wire(&self) -> Value {
        match self {
            Self::Snapshot { local, remote } => {
                json!({"_tag":"snapshot","local":local,"remote":remote})
            }
            Self::LocalUpdated(local) => json!({"_tag":"localUpdated","local":local}),
            Self::RemoteUpdated(remote) => json!({"_tag":"remoteUpdated","remote":remote}),
        }
    }
}

/// `mergeGitStatusParts`: no remote reads as no upstream.
pub fn merge_status(
    local: &VcsStatusLocalResult,
    remote: Option<&VcsStatusRemoteResult>,
) -> VcsStatus {
    let (has_upstream, ahead_count, behind_count, ahead_of_default_count, pr) = match remote {
        Some(remote) => (
            remote.has_upstream,
            remote.ahead_count,
            remote.behind_count,
            remote.ahead_of_default_count,
            remote.pr.clone(),
        ),
        None => (false, 0, 0, Some(0), None),
    };
    VcsStatus {
        is_repo: local.is_repo,
        source_control_provider: local.source_control_provider.clone(),
        has_primary_remote: local.has_primary_remote,
        is_default_ref: local.is_default_ref,
        ref_name: local.ref_name.clone(),
        has_working_tree_changes: local.has_working_tree_changes,
        working_tree: local.working_tree.clone(),
        branch_changes: local.branch_changes.clone(),
        has_upstream,
        ahead_count,
        behind_count,
        ahead_of_default_count,
        pr,
    }
}

/// How long to wait after the `consecutive_failures`-th failed refresh: 30 seconds doubling up
/// to 15 minutes, and never below the configured interval.
pub fn remote_refresh_failure_delay(consecutive_failures: u32, configured: Duration) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(32);
    FAILURE_BASE_DELAY
        .saturating_mul(2u32.saturating_pow(exponent))
        .min(FAILURE_MAX_DELAY)
        .max(configured)
}

#[derive(Clone)]
struct Change {
    cwd: String,
    event: StatusEvent,
}

#[derive(Default, Clone)]
struct CachedStatus {
    local: Option<VcsStatusLocalResult>,
    /// `None` until a remote read finished; `Some(None)` is a finished read with no remote.
    remote: Option<Option<VcsStatusRemoteResult>>,
}

struct Poller {
    task: JoinHandle<()>,
    subscribers: usize,
    /// The working directories (as clients named them) whose subscribers want this poller.
    demand: Arc<Mutex<HashMap<String, usize>>>,
}

#[derive(Default)]
struct PollerState {
    consecutive_failures: u32,
    needs_initial_refresh: bool,
}

#[derive(Clone, Copy, Default)]
struct RefreshOptions {
    refresh_upstream: Option<bool>,
}

type LockMap = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

/// One permit per cwd for remote reads that write the cache. Without it a periodic poll that
/// started before a pull request was opened could finish after the refresh and overwrite the
/// fresh result. Released locks leave the map.
struct CwdLock {
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    locks: LockMap,
    key: String,
}

impl Drop for CwdLock {
    fn drop(&mut self) {
        self.guard.take();
        let mut locks = self.locks.lock().unwrap();
        if locks
            .get(&self.key)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(&self.key);
        }
    }
}

struct Inner<S> {
    source: S,
    changes: broadcast::Sender<Change>,
    cache: Mutex<HashMap<String, CachedStatus>>,
    locks: LockMap,
    pollers: Mutex<HashMap<String, Poller>>,
    /// Woken whenever a subscriber lets go of a poller.
    poller_released: tokio::sync::Notify,
}

pub struct Broadcaster<S: StatusSource> {
    inner: Arc<Inner<S>>,
}

impl<S: StatusSource> Clone for Broadcaster<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: StatusSource> Broadcaster<S> {
    pub fn new(source: S) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                changes: broadcast::channel(CHANGE_CHANNEL_CAPACITY).0,
                cache: Mutex::default(),
                locks: Arc::default(),
                pollers: Mutex::default(),
                poller_released: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn source(&self) -> &S {
        &self.inner.source
    }

    /// The cached status, loading whichever part is missing. Used by the orchestration
    /// turn-end refresh, which is not ported yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_status(&self, cwd: &str) -> Result<VcsStatus, VcsRpcError> {
        self.inner.get_status(cwd).await
    }

    /// Re-reads the local part only, publishing it when it changed.
    pub async fn refresh_local_status(
        &self,
        cwd: &str,
    ) -> Result<VcsStatusLocalResult, VcsRpcError> {
        let cwd = canonical(cwd).await;
        self.inner.refresh_local_status_core(&cwd).await
    }

    /// The explicit refresh: fetches the upstream, re-reads both parts, publishes what
    /// changed, and fast-forwards a clean default branch whose project enabled auto-pull.
    pub async fn refresh_status(&self, raw: &str, hooks: &Hooks) -> Result<VcsStatus, VcsRpcError> {
        self.inner.refresh_status(raw, hooks).await
    }

    /// Re-reads the remote part of a loaded cwd without fetching, if background policy allows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn refresh_pull_request_status(
        &self,
        raw: &str,
        hooks: &Hooks,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        self.inner.refresh_pull_request_status(raw, hooks).await
    }

    /// Opens a status subscription: the first event is a snapshot, then every change to the
    /// directory's status. Dropping the stream releases the subscriber's hold on the poller.
    pub async fn open_stream(
        &self,
        cwd: &str,
        include_remote: bool,
        hooks: &Hooks,
    ) -> Result<StatusStream<S>, VcsRpcError> {
        let inner = &self.inner;
        let normalized = canonical(cwd).await;
        let receiver = inner.changes.subscribe();
        let local = inner.get_or_load_local(&normalized).await?;
        let cached = inner.cached(&normalized);
        let remote = cached.remote.clone().flatten();
        let release = include_remote.then(|| {
            inner.retain_poller(
                &normalized,
                cwd,
                hooks,
                // Nothing remote cached yet: the first refresh is due now.
                cached.remote.is_none(),
            )
        });
        Ok(StatusStream {
            first: Some(StatusEvent::Snapshot { local, remote }),
            receiver,
            cwd: normalized,
            inner: inner.clone(),
            _release: release.map(|demand_cwd| Release {
                inner: inner.clone(),
                cwd: demand_cwd.0,
                demand_cwd: demand_cwd.1,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) async fn poll_tick_for_test(
        &self,
        cwd: &str,
        demand: &[&str],
        hooks: &Hooks,
        needs_initial_refresh: bool,
    ) -> Duration {
        let demand = Arc::new(Mutex::new(
            demand.iter().map(|cwd| (cwd.to_string(), 1)).collect(),
        ));
        let mut state = PollerState {
            needs_initial_refresh,
            consecutive_failures: 0,
        };
        self.inner.poll_tick(cwd, &demand, hooks, &mut state).await
    }

    /// Returns once no subscriber holds a poller for `cwd` (a canonical path).
    pub async fn wait_for_no_poller(&self, cwd: &str) {
        loop {
            let released = self.inner.poller_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self.poller_subscribers(cwd).is_none() {
                return;
            }
            released.await;
        }
    }

    /// How many subscribers hold the poller of `cwd` (a canonical path), if one is running.
    pub fn poller_subscribers(&self, cwd: &str) -> Option<usize> {
        self.inner
            .pollers
            .lock()
            .unwrap()
            .get(cwd)
            .map(|poller| poller.subscribers)
    }
}

struct Release<S: StatusSource> {
    inner: Arc<Inner<S>>,
    cwd: String,
    demand_cwd: String,
}

impl<S: StatusSource> Drop for Release<S> {
    fn drop(&mut self) {
        self.inner.release_poller(&self.cwd, &self.demand_cwd);
    }
}

/// A live subscription. Its `next` yields the snapshot and then each published change for the
/// directory; dropping it ends the subscription.
pub struct StatusStream<S: StatusSource> {
    first: Option<StatusEvent>,
    receiver: broadcast::Receiver<Change>,
    cwd: String,
    inner: Arc<Inner<S>>,
    _release: Option<Release<S>>,
}

impl<S: StatusSource> StatusStream<S> {
    pub async fn next(&mut self) -> Option<StatusEvent> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        loop {
            match self.receiver.recv().await {
                Ok(change) if change.cwd == self.cwd => return Some(change.event),
                Ok(_) => {}
                // A subscriber too slow to keep up gets the current state instead of the
                // updates it missed.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let cached = self.inner.cached(&self.cwd);
                    if let Some(local) = cached.local {
                        return Some(StatusEvent::Snapshot {
                            local,
                            remote: cached.remote.flatten(),
                        });
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl<S: StatusSource> Inner<S> {
    fn cached(&self, cwd: &str) -> CachedStatus {
        self.cache
            .lock()
            .unwrap()
            .get(cwd)
            .cloned()
            .unwrap_or_default()
    }

    async fn lock(&self, cwd: &str) -> CwdLock {
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(cwd.to_string())
            .or_default()
            .clone();
        CwdLock {
            guard: Some(lock.lock_owned().await),
            locks: self.locks.clone(),
            key: cwd.to_string(),
        }
    }

    fn publish(&self, cwd: &str, event: StatusEvent) {
        let _ = self.changes.send(Change {
            cwd: cwd.to_string(),
            event,
        });
    }

    fn update_local(
        &self,
        cwd: &str,
        local: VcsStatusLocalResult,
        publish: bool,
    ) -> VcsStatusLocalResult {
        let changed = {
            let mut cache = self.cache.lock().unwrap();
            let entry = cache.entry(cwd.to_string()).or_default();
            let changed = entry.local.as_ref() != Some(&local);
            entry.local = Some(local.clone());
            changed
        };
        if publish && changed {
            self.publish(cwd, StatusEvent::LocalUpdated(local.clone()));
        }
        local
    }

    fn update_remote(
        &self,
        cwd: &str,
        remote: Option<VcsStatusRemoteResult>,
        publish: bool,
    ) -> Option<VcsStatusRemoteResult> {
        let changed = {
            let mut cache = self.cache.lock().unwrap();
            let entry = cache.entry(cwd.to_string()).or_default();
            let changed = entry.remote.as_ref() != Some(&remote);
            entry.remote = Some(remote.clone());
            changed
        };
        if publish && changed {
            self.publish(cwd, StatusEvent::RemoteUpdated(remote.clone()));
        }
        remote
    }

    fn update_both(
        &self,
        cwd: &str,
        local: VcsStatusLocalResult,
        remote: Option<VcsStatusRemoteResult>,
        publish: bool,
    ) -> VcsStatus {
        let changed = {
            let mut cache = self.cache.lock().unwrap();
            let entry = cache.entry(cwd.to_string()).or_default();
            let changed =
                entry.local.as_ref() != Some(&local) || entry.remote.as_ref() != Some(&remote);
            entry.local = Some(local.clone());
            entry.remote = Some(remote.clone());
            changed
        };
        let merged = merge_status(&local, remote.as_ref());
        if publish && changed {
            self.publish(cwd, StatusEvent::Snapshot { local, remote });
        }
        merged
    }

    async fn get_or_load_local(&self, cwd: &str) -> Result<VcsStatusLocalResult, VcsRpcError> {
        if let Some(local) = self.cached(cwd).local {
            return Ok(local);
        }
        let local = self.source.local_status(cwd).await?;
        Ok(self.update_local(cwd, local, false))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    async fn get_status(&self, raw: &str) -> Result<VcsStatus, VcsRpcError> {
        let cwd = canonical(raw).await;
        let cached = self.cached(&cwd);
        if let (Some(local), Some(remote)) = (&cached.local, &cached.remote) {
            return Ok(merge_status(local, remote.as_ref()));
        }
        let _lock = self.lock(&cwd).await;
        let latest = self.cached(&cwd);
        let (local, remote) = tokio::try_join!(
            async {
                match latest.local {
                    Some(local) => Ok(local),
                    None => self.source.local_status(&cwd).await,
                }
            },
            async {
                match latest.remote {
                    Some(remote) => Ok(remote),
                    None => {
                        self.source
                            .remote_status(&cwd, RemoteOptions::default())
                            .await
                    }
                }
            },
        )?;
        Ok(self.update_both(&cwd, local, remote, false))
    }

    async fn refresh_local_status_core(
        &self,
        cwd: &str,
    ) -> Result<VcsStatusLocalResult, VcsRpcError> {
        self.source.invalidate_local(cwd).await;
        let local = self.source.local_status(cwd).await?;
        Ok(self.update_local(cwd, local, true))
    }

    /// Fast-forwards a clean default branch that is only behind, when the project enabled
    /// auto-pull. Every failure is logged and ends the attempt.
    async fn maybe_auto_pull(
        &self,
        cwd: &str,
        remote: Option<&VcsStatusRemoteResult>,
        policy_cwds: &[String],
        hooks: &Hooks,
    ) -> Option<(VcsStatusLocalResult, Option<VcsStatusRemoteResult>)> {
        let attempt = async {
            let mut enabled = false;
            for policy_cwd in policy_cwds {
                enabled |= (hooks.auto_pull)(policy_cwd.clone()).await;
            }
            let Some(remote) = remote else {
                return Ok(None);
            };
            if !remote.has_upstream
                || remote.ahead_count > 0
                || remote.behind_count == 0
                || !enabled
            {
                return Ok(None);
            }
            self.source.invalidate_local(cwd).await;
            let local = self.source.local_status(cwd).await?;
            if !local.is_repo || !local.is_default_ref || local.has_working_tree_changes {
                return Ok(None);
            }
            self.source.pull_current_branch(cwd).await?;
            self.source.invalidate_status(cwd).await;
            let (local, remote) = tokio::try_join!(
                self.source.local_status(cwd),
                self.source.remote_status(
                    cwd,
                    RemoteOptions {
                        refresh_upstream: Some(false),
                        ..RemoteOptions::default()
                    }
                ),
            )?;
            self.update_both(cwd, local.clone(), remote.clone(), true);
            Ok::<_, VcsRpcError>(Some((local, remote)))
        };
        match attempt.await {
            Ok(pulled) => pulled,
            Err(error) => {
                tracing::warn!(cwd=%cwd,error=%error,"Automatic project pull failed");
                None
            }
        }
    }

    async fn refresh_remote_status(
        &self,
        cwd: &str,
        options: RefreshOptions,
        policy_cwds: &[String],
        hooks: &Hooks,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        let _lock = self.lock(cwd).await;
        if options.refresh_upstream != Some(false) {
            self.source.invalidate_remote(cwd).await;
        }
        let previous = self.cached(cwd).remote.flatten();
        let remote = self
            .source
            .remote_status(
                cwd,
                RemoteOptions {
                    refresh_upstream: options.refresh_upstream,
                    ..RemoteOptions::default()
                },
            )
            .await?;
        if let Some((_, pulled)) = self
            .maybe_auto_pull(cwd, remote.as_ref(), policy_cwds, hooks)
            .await
        {
            return Ok(pulled);
        }
        // Local status holds the Changes totals, which compare against remote refs. A fetch can
        // move them with no local trigger (a push from a terminal, a PR merged on the host), so
        // re-read local status on the first fetch and whenever divergence moves.
        if let Some(remote) = &remote
            && previous.as_ref().is_none_or(|previous| {
                previous.ahead_count != remote.ahead_count
                    || previous.behind_count != remote.behind_count
                    || previous.ahead_of_default_count != remote.ahead_of_default_count
            })
        {
            self.refresh_local_status_core(cwd).await?;
        }
        Ok(self.update_remote(cwd, remote, true))
    }

    async fn refresh_status(&self, raw: &str, hooks: &Hooks) -> Result<VcsStatus, VcsRpcError> {
        let cwd = canonical(raw).await;
        let _lock = self.lock(&cwd).await;
        self.source.invalidate_status(&cwd).await;
        // Local after remote: the fetch can move the base the Changes totals compare with.
        let remote = self
            .source
            .remote_status(&cwd, RemoteOptions::default())
            .await?;
        let local = self.source.local_status(&cwd).await?;
        if let Some((local, remote)) = self
            .maybe_auto_pull(&cwd, remote.as_ref(), &[raw.to_string()], hooks)
            .await
        {
            return Ok(merge_status(&local, remote.as_ref()));
        }
        Ok(self.update_both(&cwd, local, remote, true))
    }

    async fn refresh_pull_request_status(
        &self,
        raw: &str,
        hooks: &Hooks,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        let cwd = canonical(raw).await;
        let _lock = self.lock(&cwd).await;
        if self.cached(&cwd).remote.flatten().is_none() {
            return Ok(None);
        }
        let demand: Vec<String> = match self.pollers.lock().unwrap().get(&cwd) {
            Some(poller) => poller.demand.lock().unwrap().keys().cloned().collect(),
            None => vec![raw.to_string()],
        };
        let mut should_refresh = false;
        for demand_cwd in demand {
            should_refresh |= (hooks.scope_allowed)(demand_cwd).await;
        }
        if !should_refresh {
            return Ok(None);
        }
        // Resolve the checked-out branch again: a cached PR can belong to the previous branch
        // after an agent checks out another one.
        let remote = self
            .source
            .remote_status(
                &cwd,
                RemoteOptions {
                    refresh_upstream: Some(false),
                    refresh_missing_pull_request: true,
                },
            )
            .await?;
        Ok(self.update_remote(&cwd, remote, true))
    }

    /// One pass of a poller: refreshes the remote status if there is demand, and says how long
    /// to wait before the next pass.
    async fn poll_tick(
        &self,
        cwd: &str,
        demand: &Arc<Mutex<HashMap<String, usize>>>,
        hooks: &Hooks,
        state: &mut PollerState,
    ) -> Duration {
        let configured = (hooks.fetch_interval)().await;
        let active = if configured.is_zero() {
            DEFAULT_REFRESH_INTERVAL
        } else {
            configured
        };
        if configured.is_zero() && !state.needs_initial_refresh {
            return active;
        }
        let demand_cwds: Vec<String> = demand.lock().unwrap().keys().cloned().collect();
        let mut should_run = state.needs_initial_refresh;
        if !should_run {
            for demand_cwd in &demand_cwds {
                should_run |= (hooks.scope_allowed)(demand_cwd.clone()).await;
            }
        }
        if !should_run {
            return active;
        }
        let refreshed = self
            .refresh_remote_status(
                cwd,
                RefreshOptions {
                    refresh_upstream: Some(!configured.is_zero()),
                },
                &demand_cwds,
                hooks,
            )
            .await;
        match refreshed {
            Ok(_) => {
                state.needs_initial_refresh = false;
                state.consecutive_failures = 0;
                active
            }
            Err(error) => {
                state.consecutive_failures += 1;
                let next = remote_refresh_failure_delay(state.consecutive_failures, active);
                tracing::warn!(
                    cwd_length = cwd.encode_utf16().count(),
                    failure_tag = error.tag(),
                    failure_operation = error.operation().unwrap_or(""),
                    consecutive_failures = state.consecutive_failures,
                    next_delay_ms = next.as_millis() as u64,
                    "VCS remote status refresh failed"
                );
                next
            }
        }
    }

    /// Starts (or joins) the poller of `cwd`. Returns the pair a `Release` needs.
    fn retain_poller(
        self: &Arc<Self>,
        cwd: &str,
        demand_cwd: &str,
        hooks: &Hooks,
        refresh_immediately: bool,
    ) -> (String, String) {
        let mut pollers = self.pollers.lock().unwrap();
        if let Some(poller) = pollers.get_mut(cwd) {
            *poller
                .demand
                .lock()
                .unwrap()
                .entry(demand_cwd.to_string())
                .or_default() += 1;
            poller.subscribers += 1;
        } else {
            let demand = Arc::new(Mutex::new(HashMap::from([(demand_cwd.to_string(), 1)])));
            let task = tokio::spawn(run_poller(
                self.clone(),
                cwd.to_string(),
                demand.clone(),
                hooks.clone(),
                refresh_immediately,
            ));
            pollers.insert(
                cwd.to_string(),
                Poller {
                    task,
                    subscribers: 1,
                    demand,
                },
            );
        }
        (cwd.to_string(), demand_cwd.to_string())
    }

    fn release_poller(&self, cwd: &str, demand_cwd: &str) {
        self.release_poller_locked(cwd, demand_cwd);
        self.poller_released.notify_waiters();
    }

    fn release_poller_locked(&self, cwd: &str, demand_cwd: &str) {
        let mut pollers = self.pollers.lock().unwrap();
        let Some(poller) = pollers.get_mut(cwd) else {
            return;
        };
        if poller.subscribers > 1 {
            poller.subscribers -= 1;
            let mut demand = poller.demand.lock().unwrap();
            match demand.get_mut(demand_cwd) {
                Some(count) if *count > 1 => *count -= 1,
                _ => {
                    demand.remove(demand_cwd);
                }
            }
            return;
        }
        if let Some(poller) = pollers.remove(cwd) {
            poller.task.abort();
        }
    }
}

async fn run_poller<S: StatusSource>(
    inner: Arc<Inner<S>>,
    cwd: String,
    demand: Arc<Mutex<HashMap<String, usize>>>,
    hooks: Hooks,
    refresh_immediately: bool,
) {
    let mut state = PollerState {
        needs_initial_refresh: refresh_immediately,
        consecutive_failures: 0,
    };
    if !refresh_immediately {
        let configured = (hooks.fetch_interval)().await;
        tokio::time::sleep(if configured.is_zero() {
            DEFAULT_REFRESH_INTERVAL
        } else {
            configured
        })
        .await;
    }
    loop {
        let delay = inner.poll_tick(&cwd, &demand, &hooks, &mut state).await;
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
#[path = "vcs_status_tests.rs"]
mod tests;
