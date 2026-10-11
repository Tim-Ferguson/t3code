//! Storage cleanup (`storageCleanup.ts`): removes the worktrees, browser artifacts and rotated logs the
//! `storageCleanup` and `worktreeCleanup` settings ask to be removed, and recreates a removed worktree when
//! its thread starts a turn (`recreate_missing_worktree`, the companion in `ProviderTurnStartService.ts`).
//! The two are one feature: cleanup keeps each thread's branch and `worktreePath`, and the recreation is what
//! makes removing a checkout safe.
//!
//! A sweep runs when the service starts, hourly after that, when a cleanup setting changes, and when a thread
//! is deleted or a provider session ends (so a deleted thread's worktree goes once nothing holds it). Sweeps
//! never overlap.
//!
//! This deletes user data, so every rule below is the original's and every doubt resolves to keeping the
//! checkout. A worktree is removed only when all of these hold, checked again after the Git and host calls
//! that decide eligibility, under the workspace lease of its path:
//!   - its path lies under a managed worktrees directory (the setting, its predecessors, the default) and is
//!     not a symbolic link, nor holds a project's workspace root, nor is the main checkout (`.git` is a file);
//!   - exactly one live thread uses the path and is idle (no active run, no pending request, no background
//!     work, no queued prompt) with a branch, or the thread was deleted and nothing else uses the path and no
//!     effect of it is outstanding; no live terminal or non-stopped provider session is inside it;
//!   - Git sees a clean working tree on the thread's branch whose only ignored files are `node_modules`;
//!   - one rule holds: deleted, `worktreeAfterDays` of inactivity, the head is in the default branch
//!     (`worktreeUnchanged`), or the branch's latest pull request is merged (`worktreeOnMerge`; a squash merge
//!     must name this exact head commit).
//! `git worktree remove` is run without `--force`, and removal never deletes the branch, so commits survive.
//! Pinned, settled and archived threads are not treated differently by the original, and are not here.
use crate::{
    persistence::Store,
    scheduled_tasks::SchedulerClock,
    thread_settlement::has_queued_turn_start,
    workspace_lease::{resolve, with_workspace_lease},
};
use async_trait::async_trait;
use chrono::DateTime;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use t3_vcs::{
    GitCore,
    core::{ExecuteGitInput, VcsCreateWorktreeInput},
    status::LocalStatusOptions,
};
use tokio::sync::{Notify, watch};

const DAY_MS: f64 = 86_400_000.0;
const SWEEP_INTERVAL_MS: i64 = 60 * 60 * 1000;
const IGNORED_LIST_BYTES: usize = 64 * 1024;

// ---- rules -------------------------------------------------------------------------------------------------

/// `WorktreeCleanupRules` after `resolveWorktreeCleanup`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorktreeRules {
    pub after_days: Option<f64>,
    pub on_merge: bool,
    pub on_delete: bool,
    pub unchanged: bool,
}

impl WorktreeRules {
    const OFF: Self = Self { after_days: None, on_merge: false, on_delete: false, unchanged: false };
    /// `worktreeCleanupEnabled`.
    pub fn enabled(&self) -> bool {
        self.after_days.is_some() || self.on_merge || self.on_delete || self.unchanged
    }
    fn of(rules: &Value) -> Self {
        Self {
            after_days: rules["worktreeAfterDays"].as_f64(),
            on_merge: rules["worktreeOnMerge"] == true,
            on_delete: rules["worktreeOnDelete"] == true,
            unchanged: rules["worktreeUnchanged"] == true,
        }
    }
}

/// `resolveWorktreeCleanup`: the project's own policy when its overrides carry one (null included), else the
/// environment's policy, else the `storageCleanup` rules. `off` is all rules off, `custom` its own rules.
pub fn resolve_worktree_cleanup(settings: &Value, project_id: Option<&str>) -> WorktreeRules {
    let overridden = project_id.and_then(|id| settings["projectSettingsOverrides"].get(id)).and_then(|overrides| overrides.get("worktreeCleanup"));
    let policy = overridden.unwrap_or(&settings["worktreeCleanup"]);
    match policy["mode"].as_str() {
        Some("custom") => WorktreeRules::of(&policy["rules"]),
        Some("off") => WorktreeRules::OFF,
        _ => WorktreeRules::of(&settings["storageCleanup"]),
    }
}

/// `anyWorktreePolicy`: whether the environment's or any project's rules satisfy `predicate`.
pub fn any_worktree_policy(settings: &Value, predicate: impl Fn(&WorktreeRules) -> bool) -> bool {
    predicate(&resolve_worktree_cleanup(settings, None))
        || settings["projectSettingsOverrides"].as_object().into_iter().flatten().any(|(id, _)| predicate(&resolve_worktree_cleanup(settings, Some(id))))
}

/// Whether a settings change touches what the cleanup acts on: `storageCleanup`, the environment's worktree policy, or a
/// project's.
pub fn cleanup_settings_changed(previous: &Value, next: &Value) -> bool {
    next["storageCleanup"] != previous["storageCleanup"] || next["worktreeCleanup"] != previous["worktreeCleanup"] || !same_project_policies(next, previous)
}

fn same_project_policies(left: &Value, right: &Value) -> bool {
    let ids: HashSet<&String> = [left, right].iter().flat_map(|settings| settings["projectSettingsOverrides"].as_object().into_iter().flatten().map(|(id, _)| id)).collect();
    ids.into_iter().all(|id| left["projectSettingsOverrides"][id]["worktreeCleanup"] == right["projectSettingsOverrides"][id]["worktreeCleanup"])
}

fn millis(value: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.as_str()?).ok().map(|time| time.timestamp_millis())
}

/// `storageCleanupThreadIdle`: a live session keeps its cwd even when no turn is running. `thread` is a thread shell.
pub fn thread_idle(thread: &Value, now_ms: i64) -> bool {
    !thread["branch"].is_null()
        && !thread["worktreePath"].is_null()
        && thread["activeRunId"].is_null()
        && matches!(thread["status"].as_str(), Some("idle" | "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back"))
        && thread["pendingBackgroundTasks"].as_array().is_none_or(|tasks| tasks.is_empty())
        && thread["pendingRuntimeRequest"].is_null()
        && !has_queued_turn_start(thread, now_ms)
}

/// `storageCleanupActivityAt`: pull request refreshes and renames do not reset the inactivity clock. `None` when no
/// time can be read, which the callers treat as recent.
pub fn activity_at(thread: &Value) -> Option<i64> {
    ["createdAt", "latestUserMessageAt", "latestRunRequestedAt", "latestRunStartedAt", "latestRunCompletedAt"].iter().filter_map(|field| millis(&thread[*field])).max()
}

/// `storageCleanupPullRequestMerged`: whether the host's pull request proves this worktree's head was merged. A squash
/// or rebase merge leaves the head outside the default branch, so the merged pull request then has to name this exact
/// commit, from this branch, into the default branch.
pub fn pull_request_merged(pull_request: Option<&crate::git_workflow_status::BranchPullRequest>, branch: &str, default_branch: &str, head_sha: &str, integrated: bool) -> bool {
    use t3_contracts::ChangeRequestState;
    pull_request.is_some_and(|pr| {
        pr.state == ChangeRequestState::Merged
            && (integrated || (pr.head_ref == branch && pr.base_ref == default_branch && pr.head_sha.as_deref() == Some(head_sha)))
    })
}

/// A path of the form `…/node_modules/` is a dependency install, reproducible; anything else ignored may hold secrets or data.
/// Git lists a directory holding only ignored files (`packages/`) as well as the install inside it, so a nested install
/// keeps the worktree; the original's pattern is the same (`/(^|\/)node_modules\/$/`), and that errs toward keeping.
fn is_dependency_directory(entry: &str) -> bool {
    entry == "node_modules/" || entry.ends_with("/node_modules/")
}

fn inside(root: &Path, target: &Path) -> bool {
    target != root && target.starts_with(root)
}

// ---- what the cleanup acts through ----------------------------------------------------------------------------

/// The settings the cleanup reads, as the original's `ServerSettingsService.getSettings` and `subscribeChanges`.
#[async_trait]
pub trait SettingsFeed: Send + Sync + 'static {
    /// The settings now, as the wire shape. An error skips the sweep.
    async fn current(&self) -> Result<Value, String>;
    /// Each later change of the settings.
    async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>>;
}

/// The settings service of the server.
pub struct ServiceSettings(pub crate::server_settings::SettingsService);

#[async_trait]
impl SettingsFeed for ServiceSettings {
    async fn current(&self) -> Result<Value, String> {
        let settings = self.0.snapshot().await.map_err(|error| error.to_string())?;
        serde_json::to_value(settings).map_err(|error| error.to_string())
    }
    async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        let mut subscription = self.0.subscribe().await.ok()?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(next) = subscription.recv().await {
                let Ok(value) = serde_json::to_value(next) else { break };
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Some(receiver)
    }
}

/// `GitManager.branchPullRequest(…, { refresh: true })`.
#[async_trait]
pub trait BranchPullRequests: Send + Sync + 'static {
    async fn latest(&self, cwd: &Path, branch: &str) -> Result<Option<crate::git_workflow_status::BranchPullRequest>, String>;
}

/// The pull request of a branch as the source control host reports it.
pub struct HostPullRequests {
    pub host: crate::source_control_host::Host,
    pub core: GitCore,
}

#[async_trait]
impl BranchPullRequests for HostPullRequests {
    async fn latest(&self, cwd: &Path, branch: &str) -> Result<Option<crate::git_workflow_status::BranchPullRequest>, String> {
        crate::git_workflow_status::branch_pull_request(&self.host, &self.core, &cwd.to_string_lossy(), branch, true).await
    }
}

/// The running and starting terminals, by `terminal.metadata` events (`noteTerminal`).
#[derive(Default)]
pub struct LiveTerminals(Mutex<HashMap<(String, String), Value>>);

impl LiveTerminals {
    pub fn apply(&self, event: &Value) {
        let mut terminals = self.0.lock().unwrap();
        let key = |terminal: &Value| (terminal["threadId"].as_str().unwrap_or_default().to_owned(), terminal["terminalId"].as_str().unwrap_or_default().to_owned());
        match event["type"].as_str() {
            Some("snapshot") => {
                terminals.clear();
                for terminal in event["terminals"].as_array().into_iter().flatten() {
                    terminals.insert(key(terminal), terminal.clone());
                }
            }
            Some("upsert") => {
                terminals.insert(key(&event["terminal"]), event["terminal"].clone());
            }
            Some("remove") => {
                terminals.remove(&(event["threadId"].as_str().unwrap_or_default().to_owned(), event["terminalId"].as_str().unwrap_or_default().to_owned()));
            }
            _ => {}
        }
    }

    /// `hasTerminal`: a starting or running terminal of, or inside, the worktree.
    fn holds(&self, worktree: &Path) -> bool {
        self.0.lock().unwrap().values().any(|terminal| {
            if !matches!(terminal["status"].as_str(), Some("starting" | "running")) {
                return false;
            }
            let cwd = resolve(terminal["cwd"].as_str().unwrap_or_default());
            terminal["worktreePath"].as_str().is_some_and(|path| resolve(path) == worktree) || cwd == worktree || inside(worktree, &cwd)
        })
    }
}

pub struct Options {
    pub store: Store,
    /// The state directory: `browser-artifacts`, `logs` and (by default) `../worktrees` are relative to it.
    pub state_dir: PathBuf,
    pub settings: Arc<dyn SettingsFeed>,
    pub git: GitCore,
    pub pull_requests: Arc<dyn BranchPullRequests>,
    pub clock: Arc<dyn SchedulerClock>,
    pub terminals: Arc<LiveTerminals>,
}

struct Inner {
    options: Options,
    sweeping: tokio::sync::Mutex<()>,
    stop: AtomicBool,
}

/// A thread of the sweep: its shell, and for a deleted one the root of its project.
struct Candidate {
    shell: Value,
    deleted_project_root: Option<String>,
}

#[derive(Clone)]
pub struct StorageCleanup {
    inner: Arc<Inner>,
    requested: Arc<AtomicU64>,
    wake: Arc<Notify>,
    completed: Arc<watch::Sender<u64>>,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl StorageCleanup {
    pub fn new(options: Options) -> Self {
        Self {
            inner: Arc::new(Inner { options, sweeping: tokio::sync::Mutex::new(()), stop: AtomicBool::new(false) }),
            requested: Arc::default(),
            wake: Arc::default(),
            completed: Arc::new(watch::channel(0).0),
            tasks: Arc::default(),
        }
    }

    /// The server's cleanup: the process's git, the source control host of the settings, and the clock idle release runs on
    /// (so a test moves time). Started with [`StorageCleanup::start`] and the terminal manager's metadata stream.
    pub fn for_server(execution: &crate::execution::ExecutionService, state_dir: PathBuf, settings: crate::server_settings::SettingsService) -> Self {
        let core = crate::vcs_service::VcsService::shared().core().clone();
        let host = crate::source_control_host::Host::process_default().with_settings(Some(settings.clone()));
        let held = crate::cleanup_hold::HeldSettings { inner: ServiceSettings(settings), state_dir: state_dir.clone() };
        Self::new(Options {
            store: execution.store.clone(),
            state_dir,
            settings: Arc::new(held),
            git: core.clone(),
            pull_requests: Arc::new(HostPullRequests { host, core }),
            clock: execution.providers.idle_release().clock(),
            terminals: Arc::default(),
        })
    }

    /// Asks for a sweep; requests made while one runs share the next.
    pub fn request(&self) {
        self.requested.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// The sweeps finished so far, as the number of requests they covered; for a test to wait on the next.
    pub fn completed(&self) -> watch::Receiver<u64> {
        self.completed.subscribe()
    }

    /// Resolves once a sweep that began after the requests made so far has finished (`worker.drain`).
    pub async fn drain(&self) {
        let target = self.requested.load(Ordering::SeqCst);
        let _ = self.completed.subscribe().wait_for(|done| *done >= target).await;
    }

    /// Starts the sweeps (`StorageCleanup.start`): once now and hourly on the injected clock, on a change of the
    /// cleanup settings, and on a thread deletion or provider-session change while a delete rule exists.
    pub async fn start(&self, terminal_events: Option<crate::terminal_manager::TerminalSubscription>) {
        let mut tasks = vec![];
        // The worker.
        {
            let (this, completed) = (self.clone(), self.completed.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    this.wake.notified().await;
                    let target = this.requested.load(Ordering::SeqCst);
                    if this.inner.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    this.sweep().await;
                    completed.send_replace(target);
                }
            }));
        }
        let settings = self.inner.options.settings.clone();
        let last = Arc::new(Mutex::new(settings.current().await.unwrap_or(Value::Null)));
        let changes = settings.changes().await;
        let mut events = self.inner.options.store.subscribe();
        // Hourly, the first at once.
        {
            let this = self.clone();
            tasks.push(tokio::spawn(async move {
                let clock = this.inner.options.clock.clone();
                loop {
                    let due = clock.now_ms() + SWEEP_INTERVAL_MS;
                    this.request();
                    this.drain().await;
                    clock.sleep_until(due).await;
                }
            }));
        }
        if let Some(mut changes) = changes {
            let (this, last) = (self.clone(), last.clone());
            tasks.push(tokio::spawn(async move {
                while let Some(next) = changes.recv().await {
                    let unchanged = {
                        let mut last = last.lock().unwrap();
                        let unchanged = !cleanup_settings_changed(&last, &next);
                        if !unchanged {
                            *last = next;
                        }
                        unchanged
                    };
                    if !unchanged {
                        this.request();
                    }
                }
            }));
        }
        {
            let (this, last) = (self.clone(), last.clone());
            tasks.push(tokio::spawn(async move {
                loop {
                    let relevant = match events.recv().await {
                        Ok(batch) => batch.iter().any(|stored| matches!(stored.event.event_type.as_str(), "thread.deleted" | "provider-session.updated" | "provider-session.detached")),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    // A Rust provider session leaves the projection when it is detached, where the original marks it
                    // stopped; either way the path stops being held, so both wake the sweep.
                    let wanted = relevant && any_worktree_policy(&last.lock().unwrap(), |rules| rules.on_delete);
                    if wanted {
                        this.request();
                    }
                }
            }));
        }
        if let Some(mut events) = terminal_events {
            let live = self.inner.options.terminals.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(Some(event)) = events.recv().await {
                    live.apply(&event);
                }
            }));
        }
        self.tasks.lock().unwrap().extend(tasks);
    }

    /// Stops the sweeps; one in flight finishes its current worktree first.
    pub async fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let tasks: Vec<_> = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        let _sweep = self.inner.sweeping.lock().await;
    }

    /// `sweep`: worktrees, then browser artifacts, then rotated logs, each failing alone.
    pub async fn sweep(&self) {
        let _sweeping = self.inner.sweeping.lock().await;
        let options = &self.inner.options;
        let settings = match options.settings.current().await {
            Ok(settings) => settings,
            Err(error) => return tracing::warn!(%error, "storage cleanup failed"),
        };
        let now = options.clock.now_ms();
        self.clean_worktrees(&settings, now).await;
        let cleanup = &settings["storageCleanup"];
        self.clean_files(&options.state_dir.join("browser-artifacts"), cleanup["browserArtifactsAfterDays"].as_f64(), now, false).await;
        self.clean_files(&options.state_dir.join("logs"), cleanup["logsAfterDays"].as_f64(), now, true).await;
    }

    fn threads(&self) -> (Vec<Value>, Vec<Value>) {
        // Projects the original lists (`listShells`) and the live threads that use a worktree (`getShellSnapshot`, both locations).
        let store = &self.inner.options.store;
        let projects = store.projections("project").unwrap_or_default().into_iter().filter(|project| project["deletedAt"].is_null()).collect();
        let threads = store
            .projections("thread")
            .unwrap_or_default()
            .iter()
            .filter(|projection| projection["thread"]["deletedAt"].is_null() && !projection["thread"]["worktreePath"].is_null())
            .map(crate::thread::shell)
            .collect();
        (projects, threads)
    }

    async fn contains_project_root(&self, worktree: &Path, roots: &[String]) -> bool {
        for root in roots {
            let project = resolve(root);
            if project == worktree || inside(worktree, &project) {
                return true;
            }
            let real = tokio::fs::canonicalize(&project).await.unwrap_or_else(|_| project.clone());
            if real == worktree || inside(worktree, &real) {
                return true;
            }
        }
        false
    }

    async fn clean_worktrees(&self, settings: &Value, now: i64) {
        if !any_worktree_policy(settings, WorktreeRules::enabled) {
            return;
        }
        let options = &self.inner.options;
        let default_dir = crate::vcs_worktree::default_worktrees_directory(Some(&options.state_dir));
        let mut directories = vec![default_dir.clone()];
        let configured = std::iter::once(settings["worktreesDirectory"].as_str().unwrap_or_default().to_owned()).chain(settings["previousWorktreesDirectories"].as_array().into_iter().flatten().filter_map(|value| value.as_str().map(str::to_owned)));
        for setting in configured {
            if let Some(directory) = crate::vcs_worktree::resolve_worktrees_directory(&setting, &default_dir) {
                if !directories.contains(&directory) {
                    directories.push(directory);
                }
            }
        }
        // An unmounted drive only skips its own worktrees; a directory whose real path is a filesystem root is no root.
        let mut roots = vec![];
        for directory in directories {
            if let Ok(real) = tokio::fs::canonicalize(&directory).await {
                if real.parent().is_some() {
                    roots.push(real);
                }
            }
        }
        if roots.is_empty() {
            return;
        }
        let deleted = if any_worktree_policy(settings, |rules| rules.on_delete) { self.deleted_candidates(settings) } else { vec![] };
        let (projects, threads) = self.threads();
        let mut groups: HashMap<PathBuf, Vec<&Value>> = HashMap::new();
        for thread in &threads {
            groups.entry(resolve(thread["worktreePath"].as_str().unwrap_or_default())).or_default().push(thread);
        }
        let mut candidates: Vec<Candidate> = groups.values().filter(|group| group.len() == 1).map(|group| Candidate { shell: group[0].clone(), deleted_project_root: None }).collect();
        candidates.extend(deleted.into_iter().filter(|candidate| !groups.contains_key(&resolve(candidate.shell["worktreePath"].as_str().unwrap_or_default()))));
        let mut refreshed: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        for candidate in candidates {
            if self.inner.stop.load(Ordering::SeqCst) {
                return;
            }
            let thread = &candidate.shell;
            let rules = resolve_worktree_cleanup(settings, thread["projectId"].as_str());
            if !rules.enabled() {
                continue;
            }
            let worktree = resolve(thread["worktreePath"].as_str().unwrap_or_default());
            let deleted = candidate.deleted_project_root.is_some();
            let project_root = match &candidate.deleted_project_root {
                Some(root) => Some(root.clone()),
                None => projects.iter().find(|project| project["projectId"] == thread["projectId"]).and_then(|project| project["workspaceRoot"].as_str().map(str::to_owned)),
            };
            let Some(project_root) = project_root else { continue };
            if (!deleted && !thread_idle(thread, now)) || options.terminals.holds(&worktree) {
                continue;
            }
            let outcome = with_workspace_lease(worktree.clone(), self.remove_worktree(settings, &candidate, &worktree, &project_root, &projects, &rules, deleted, now, &mut refreshed)).await;
            if let Err(reason) = outcome {
                tracing::debug!(thread = thread["id"].as_str().unwrap_or_default(), %reason, "storage cleanup skipped worktree");
            }
        }
    }

    /// Threads that were deleted, with the root of the project that owned them, whose project rules delete on deletion.
    fn deleted_candidates(&self, settings: &Value) -> Vec<Candidate> {
        let store = &self.inner.options.store;
        store
            .projections("thread")
            .unwrap_or_default()
            .iter()
            .filter(|projection| !projection["thread"]["deletedAt"].is_null())
            .filter_map(|projection| {
                let thread = &projection["thread"];
                if thread["worktreePath"].is_null() || thread["branch"].is_null() || !resolve_worktree_cleanup(settings, thread["projectId"].as_str()).on_delete {
                    return None;
                }
                let project = store.projection("project", thread["projectId"].as_str()?).ok().flatten()?;
                Some(Candidate { shell: thread.clone(), deleted_project_root: Some(project["workspaceRoot"].as_str()?.to_owned()) })
            })
            .collect()
    }

    /// One worktree, under its lease. `Ok` is also "kept": every `Err` is a reason the checkout stays.
    #[allow(clippy::too_many_arguments)]
    async fn remove_worktree(
        &self,
        settings: &Value,
        candidate: &Candidate,
        worktree: &Path,
        project_root: &str,
        projects: &[Value],
        rules: &WorktreeRules,
        deleted: bool,
        now: i64,
        refreshed: &mut HashMap<PathBuf, HashSet<String>>,
    ) -> Result<(), String> {
        let options = &self.inner.options;
        let (git, thread) = (&options.git, &candidate.shell);
        let keep = |reason: &str| Err::<(), String>(reason.to_owned());
        let branch = thread["branch"].as_str().ok_or("thread has no branch")?;
        if !tokio::fs::try_exists(worktree).await.map_err(|error| error.to_string())? {
            return Ok(());
        }
        // Roots are canonical, so compare canonical paths. A symlinked parent (a linked drive) is fine; a symlinked
        // worktree directory is not.
        let real = tokio::fs::canonicalize(worktree).await.map_err(|error| error.to_string())?;
        let real_parent = tokio::fs::canonicalize(worktree.parent().ok_or("worktree has no parent")?).await.map_err(|error| error.to_string())?;
        if Some(real.as_path()) != worktree.file_name().map(|name| real_parent.join(name)).as_deref() {
            return keep("the worktree directory is a symbolic link");
        }
        if !self.managed_roots(settings).await.iter().any(|root| inside(root, &real)) {
            return keep("outside the managed worktrees directories");
        }
        let mut roots: Vec<String> = vec![project_root.to_owned()];
        roots.extend(projects.iter().filter_map(|project| project["workspaceRoot"].as_str().map(str::to_owned)));
        if self.contains_project_root(worktree, &roots).await {
            return keep("holds a project's workspace root");
        }
        // A linked worktree has a .git file. Never remove a main checkout.
        if !tokio::fs::metadata(worktree.join(".git")).await.map_err(|error| error.to_string())?.is_file() {
            return keep("a main checkout");
        }
        let clean = |status: &t3_vcs::status::GitStatusDetails| status.is_repo && status.branch.as_deref() == Some(branch) && !status.has_working_tree_changes;
        let status = git.status_details_local(worktree, LocalStatusOptions::default()).await.map_err(|error| error.to_string())?;
        if !clean(&status) {
            return keep("not a clean checkout of the thread's branch");
        }
        let head = git.resolve_commit(worktree, "HEAD").await.map_err(|error| error.to_string())?;
        if !self.only_dependency_ignores(worktree).await? {
            return keep("holds ignored files other than dependency installs");
        }
        let old = !deleted
            && rules.after_days.is_some_and(|days| activity_at(thread).is_some_and(|activity| (activity as f64) < now as f64 - days * DAY_MS));
        let mut eligible = deleted || old;
        if !eligible && (rules.unchanged || rules.on_merge) {
            let repository = resolve(project_root);
            let remote = git.resolve_primary_remote_name(&repository).await.map_err(|error| error.to_string())?;
            let Some(default_branch) = git.resolve_default_branch_name(&repository, &remote).await.map_err(|error| error.to_string())? else { return keep("no default branch") };
            let default_ref = format!("refs/remotes/{remote}/{default_branch}");
            if refreshed.entry(repository.clone()).or_default().insert(default_ref.clone()) {
                git.fetch_remote_tracking_branch(&repository, &remote, &default_branch).await.map_err(|error| error.to_string())?;
            }
            let base = git.resolve_commit(worktree, &default_ref).await.map_err(|error| error.to_string())?;
            let ancestor = git
                .execute(ExecuteGitInput {
                    operation: "StorageCleanup.integratedBranch".into(),
                    cwd: worktree.into(),
                    args: vec!["merge-base".into(), "--is-ancestor".into(), head.clone(), base],
                    allow_non_zero_exit: true,
                    ..Default::default()
                })
                .await
                .map_err(|error| error.to_string())?;
            let integrated = ancestor.exit_code == 0;
            eligible = integrated && rules.unchanged;
            if !eligible && rules.on_merge {
                let pull_request = options.pull_requests.latest(worktree, branch).await?;
                eligible = pull_request_merged(pull_request.as_ref(), branch, &default_branch, &head, integrated);
            }
        }
        if !eligible {
            return Ok(());
        }
        // Re-read after Git/host calls so a queued turn, resumed session or new thread sharing this path cancels the removal.
        let (latest_projects, latest_threads) = self.threads();
        let mut roots: Vec<String> = vec![project_root.to_owned()];
        roots.extend(latest_projects.iter().filter_map(|project| project["workspaceRoot"].as_str().map(str::to_owned)));
        if self.contains_project_root(worktree, &roots).await {
            return keep("holds a project's workspace root");
        }
        let latest: Vec<&Value> = latest_threads.iter().filter(|entry| resolve(entry["worktreePath"].as_str().unwrap_or_default()) == worktree).collect();
        if options.terminals.holds(worktree) {
            return keep("a terminal is open in it");
        }
        let current = options.settings.current().await?;
        if deleted {
            if !latest.is_empty() || !resolve_worktree_cleanup(&current, thread["projectId"].as_str()).on_delete {
                return keep("the path is in use again or the delete rule is off");
            }
            // Deletion queues durable cleanup. Do not remove its checkout until every effect has finished successfully or was cancelled.
            let thread_id = thread["id"].as_str().unwrap_or_default().to_owned();
            let pending = options
                .store
                .read(|connection| Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM rust_effect_outbox WHERE thread_id=?1 AND status NOT IN ('succeeded','cancelled'))", [thread_id], |row| row.get::<_, bool>(0))?))
                .map_err(|error| error.to_string())?;
            if pending {
                return keep("an effect of the deleted thread is outstanding");
            }
        } else if latest.len() != 1 || latest[0]["id"] != thread["id"] || !thread_idle(latest[0], now) || activity_at(latest[0]) != activity_at(thread) {
            return keep("the thread changed");
        }
        // Sessions can outlive their run and can be shared across app threads. One without a readable cwd is not guessed at.
        for projection in options.store.projections("thread").map_err(|error| error.to_string())? {
            for session in projection["providerSessions"].as_array().into_iter().flatten().filter(|session| session["status"] != "stopped") {
                let cwd = resolve(session["cwd"].as_str().ok_or("a provider session has no cwd")?);
                if cwd == worktree || inside(worktree, &cwd) {
                    return keep("a provider session runs in it");
                }
            }
        }
        let final_status = git.status_details_local(worktree, LocalStatusOptions::default()).await.map_err(|error| error.to_string())?;
        if !clean(&final_status) {
            return keep("changed while deciding");
        }
        if git.resolve_commit(worktree, "HEAD").await.map_err(|error| error.to_string())? != head {
            return keep("the head moved while deciding");
        }
        if !self.only_dependency_ignores(worktree).await? {
            return keep("gained ignored files while deciding");
        }
        if resolve_worktree_cleanup(&current, thread["projectId"].as_str()) != *rules {
            return keep("the rules changed while deciding");
        }
        git.remove_worktree(&resolve(project_root), worktree, false).await.map_err(|error| error.to_string())?;
        // The project's status shown to clients lists its worktrees (`gitManager.invalidateStatus`).
        {
            use crate::vcs_status::StatusSource as _;
            crate::vcs_service::VcsService::shared().status().source().invalidate_status(project_root).await;
        }
        // The branch and path stay: `recreate_missing_worktree` makes the checkout again when the thread resumes.
        tracing::info!(thread = thread["id"].as_str().unwrap_or_default(), "storage cleanup removed worktree");
        Ok(())
    }

    async fn managed_roots(&self, settings: &Value) -> Vec<PathBuf> {
        let default_dir = crate::vcs_worktree::default_worktrees_directory(Some(&self.inner.options.state_dir));
        let mut directories = vec![default_dir.clone()];
        let configured = std::iter::once(settings["worktreesDirectory"].as_str().unwrap_or_default().to_owned()).chain(settings["previousWorktreesDirectories"].as_array().into_iter().flatten().filter_map(|value| value.as_str().map(str::to_owned)));
        directories.extend(configured.filter_map(|setting| crate::vcs_worktree::resolve_worktrees_directory(&setting, &default_dir)));
        let mut roots = vec![];
        for directory in directories {
            if let Ok(real) = tokio::fs::canonicalize(&directory).await {
                if real.parent().is_some() {
                    roots.push(real);
                }
            }
        }
        roots
    }

    /// Ignored files can contain secrets or local datasets. Dependency installs are reproducible; every other ignored
    /// path, or a listing too long to read whole, prevents automatic removal.
    async fn only_dependency_ignores(&self, worktree: &Path) -> Result<bool, String> {
        let listing = self
            .inner
            .options
            .git
            .execute(ExecuteGitInput {
                operation: "StorageCleanup.ignoredFiles".into(),
                cwd: worktree.into(),
                args: ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"].map(String::from).to_vec(),
                max_output_bytes: Some(IGNORED_LIST_BYTES),
                append_truncation_marker: true,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(!listing.stdout_truncated && listing.stdout.split('\0').all(|entry| entry.is_empty() || is_dependency_directory(entry)))
    }

    /// `cleanFiles`: browser artifacts (the files directly in the directory) or rotated logs (`*.log.N`, `*.ndjson.N`,
    /// in any subdirectory) older than `days`. Symbolic links, and anything whose real path differs, are left alone.
    async fn clean_files(&self, root: &Path, days: Option<f64>, now: i64, rotated_logs: bool) {
        let Some(days) = days else { return };
        if let Err(error) = self.clean_files_in(root, days, now, rotated_logs).await {
            tracing::warn!(%error, root = %root.display(), "{}", if rotated_logs { "rotated log cleanup failed" } else { "browser artifact cleanup failed" });
        }
    }

    async fn clean_files_in(&self, root: &Path, days: f64, now: i64, rotated_logs: bool) -> Result<(), String> {
        if !tokio::fs::try_exists(root).await.map_err(|error| error.to_string())? {
            return Ok(());
        }
        let real_root = tokio::fs::canonicalize(root).await.map_err(|error| error.to_string())?;
        if real_root != resolve(&root.to_string_lossy()) {
            return Ok(());
        }
        self.visit(&real_root, &real_root, days, now, rotated_logs).await
    }

    fn visit<'a>(&'a self, real_root: &'a Path, directory: &'a Path, days: f64, now: i64, rotated_logs: bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let mut names = vec![];
            let mut read = tokio::fs::read_dir(directory).await.map_err(|error| error.to_string())?;
            while let Some(entry) = read.next_entry().await.map_err(|error| error.to_string())? {
                names.push(entry.file_name());
            }
            for name in names {
                let target = directory.join(&name);
                if tokio::fs::canonicalize(&target).await.map_err(|error| error.to_string())? != target || !inside(real_root, &target) {
                    continue;
                }
                let metadata = tokio::fs::metadata(&target).await.map_err(|error| error.to_string())?;
                let name = name.to_string_lossy();
                if metadata.is_dir() && rotated_logs {
                    self.visit(real_root, &target, days, now, rotated_logs).await?;
                } else if metadata.is_file() && (!rotated_logs || is_rotated_log(&name)) {
                    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|age| age.as_millis() as f64);
                    if modified.is_some_and(|modified| modified < now as f64 - days * DAY_MS) {
                        // The setting is read again for each file; one that changed ends this directory's pass.
                        let current = self.inner.options.settings.current().await?;
                        let field = if rotated_logs { "logsAfterDays" } else { "browserArtifactsAfterDays" };
                        if current["storageCleanup"][field].as_f64() != Some(days) {
                            return Ok(());
                        }
                        tokio::fs::remove_file(&target).await.map_err(|error| error.to_string())?;
                    }
                }
            }
            Ok(())
        })
    }
}

/// `/\.(?:log|ndjson)\.\d+$/`.
fn is_rotated_log(name: &str) -> bool {
    let Some((stem, digits)) = name.rsplit_once('.') else { return false };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) && (stem.ends_with(".log") || stem.ends_with(".ndjson"))
}

// ---- recreation --------------------------------------------------------------------------------------------------

/// The companion in `ProviderTurnStartService`: a thread with a branch and a worktree path whose checkout is gone
/// (removed by cleanup, or by hand) gets it back from its branch before the turn starts. A failure is logged and
/// the turn goes on, as in the original, where it then fails with the provider's own error for the missing cwd.
pub async fn recreate_missing_worktree(store: &Store, settings: impl FnOnce() -> t3_contracts::ServerSettings, git: &GitCore, thread_id: &str) {
    // Three fields of the projection, not the projection: a long thread's is large and this runs at every turn start.
    let fields = store.read(|connection| {
        Ok(connection
            .query_row(
                "SELECT json_extract(state_json,'$.thread.worktreePath'),json_extract(state_json,'$.thread.branch'),json_extract(state_json,'$.thread.projectId') FROM rust_projections WHERE aggregate_kind='thread' AND aggregate_id=?1",
                [thread_id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?)),
            )
            .ok())
    });
    let Ok(Some((Some(worktree), Some(branch), project_id))) = fields else { return };
    let (worktree, branch) = (worktree.as_str(), branch.as_str());
    // A path that cannot be checked is taken to exist.
    if tokio::fs::try_exists(worktree).await.unwrap_or(true) {
        return;
    }
    let Some(project) = project_id.as_deref().and_then(|id| store.projection("project", id).ok().flatten()) else { return };
    let Some(root) = project["workspaceRoot"].as_str() else { return };
    tracing::warn!(thread = thread_id, worktree, branch, "provider turn start recreating missing worktree");
    let project_id = project_id.as_deref().and_then(|id| id.parse::<t3_contracts::ProjectId>().ok());
    let input = VcsCreateWorktreeInput { cwd: root.into(), ref_name: branch.into(), new_ref_name: None, base_ref_name: None, path: Some(worktree.into()) };
    let result = with_workspace_lease(resolve(worktree), async {
        git.prune_worktrees(Path::new(root)).await?;
        crate::vcs_worktree::create_for_launch(git, &settings(), project_id.as_ref(), store.state_dir(), &input, None).await.map(|_| ())
    })
    .await;
    if let Err(error) = result {
        tracing::warn!(thread = thread_id, worktree, %error, "provider turn start failed to recreate worktree");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000_000;

    fn at(offset_ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(NOW + offset_ms).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn shell(overrides: Value) -> Value {
        let mut thread = json!({
            "id": "t", "projectId": "p", "branch": "feature", "worktreePath": "/worktrees/feature", "status": "idle", "activeRunId": null,
            "pendingBackgroundTasks": [], "pendingRuntimeRequest": null, "latestUserMessageAt": null, "latestRunId": null,
            "latestRunRequestedAt": null, "latestRunStartedAt": null, "latestRunCompletedAt": null, "createdAt": at(-30 * 86_400_000),
        });
        for (key, value) in overrides.as_object().unwrap() {
            thread[key] = value.clone();
        }
        thread
    }

    #[test]
    fn only_an_idle_thread_with_a_branch_and_a_worktree_is_eligible() {
        assert!(thread_idle(&shell(json!({})), NOW));
        assert!(!thread_idle(&shell(json!({"branch": null})), NOW));
        assert!(!thread_idle(&shell(json!({"worktreePath": null})), NOW));
        for status in ["running", "starting", "preparing", "waiting", "queued"] {
            assert!(!thread_idle(&shell(json!({"status": status})), NOW), "{status}");
        }
        for status in ["idle", "completed", "interrupted", "failed", "cancelled", "rolled_back"] {
            assert!(thread_idle(&shell(json!({"status": status})), NOW), "{status}");
        }
        assert!(!thread_idle(&shell(json!({"activeRunId": "run"})), NOW));
        assert!(!thread_idle(&shell(json!({"pendingBackgroundTasks": [{"taskId": "1", "kind": "command"}]})), NOW));
        assert!(!thread_idle(&shell(json!({"pendingRuntimeRequest": {"id": "r", "kind": "command", "createdAt": at(0)}})), NOW));
        assert!(!thread_idle(&shell(json!({"latestUserMessageAt": at(-1000)})), NOW), "a prompt no run has adopted yet holds the checkout");
    }

    #[test]
    fn inactivity_is_measured_from_the_newest_run_or_message_not_from_metadata() {
        let thread = shell(json!({"latestRunCompletedAt": at(-3 * 86_400_000), "updatedAt": at(0)}));
        assert_eq!(activity_at(&thread), Some(NOW - 3 * 86_400_000));
        assert_eq!(activity_at(&json!({})), None);
    }

    #[test]
    fn a_merged_pull_request_proves_the_head_only_by_integration_or_by_naming_it() {
        use crate::git_workflow_status::BranchPullRequest;
        use t3_contracts::ChangeRequestState::{Closed, Merged, Open};
        let sha = "a".repeat(40);
        let pr = |state, head: &str, base: &str, head_sha: Option<&str>| BranchPullRequest { state, head_ref: head.into(), base_ref: base.into(), head_sha: head_sha.map(str::to_owned) };
        let merged = |pr: Option<&BranchPullRequest>, integrated| pull_request_merged(pr, "feature", "main", &sha, integrated);
        assert!(merged(Some(&pr(Merged, "feature", "main", None)), true), "a merge commit leaves the head in the default branch");
        assert!(merged(Some(&pr(Merged, "feature", "main", Some(&sha))), false), "a squash merge must name this head");
        for (name, other) in [
            ("a later commit", pr(Merged, "feature", "main", Some(&"c".repeat(40)))),
            ("a release branch", pr(Merged, "feature", "release", Some(&sha))),
            ("its stack parent", pr(Merged, "feature", "stack-parent", Some(&sha))),
            ("no reported head", pr(Merged, "feature", "main", None)),
            ("another branch", pr(Merged, "other", "main", Some(&sha))),
            ("still open", pr(Open, "feature", "main", Some(&sha))),
            ("closed unmerged", pr(Closed, "feature", "main", Some(&sha))),
        ] {
            assert!(!merged(Some(&other), false), "{name}");
        }
        assert!(!merged(None, true) && !merged(None, false));
        assert!(!merged(Some(&pr(Open, "feature", "main", None)), true), "integration alone is not a merged pull request");
    }

    #[test]
    fn the_rules_come_from_the_project_then_the_environment_then_the_storage_settings() {
        let rules = |days: Value, merge: bool| json!({"worktreeAfterDays": days, "worktreeOnMerge": merge, "worktreeOnDelete": false, "worktreeUnchanged": false});
        let settings = json!({
            "storageCleanup": {"worktreeAfterDays": 30, "worktreeOnMerge": true, "worktreeOnDelete": true, "worktreeUnchanged": true},
            "worktreeCleanup": null,
            "projectSettingsOverrides": {
                "off": {"worktreeCleanup": {"mode": "off"}},
                "custom": {"worktreeCleanup": {"mode": "custom", "rules": rules(json!(7), false)}},
                "inherit": {"worktreeCleanup": null},
                "other": {"sidebarAutoSettleOnMerge": true},
            },
        });
        assert_eq!(resolve_worktree_cleanup(&settings, None), WorktreeRules { after_days: Some(30.0), on_merge: true, on_delete: true, unchanged: true });
        assert_eq!(resolve_worktree_cleanup(&settings, Some("absent")), resolve_worktree_cleanup(&settings, None));
        assert_eq!(resolve_worktree_cleanup(&settings, Some("inherit")), resolve_worktree_cleanup(&settings, None));
        assert_eq!(resolve_worktree_cleanup(&settings, Some("other")), resolve_worktree_cleanup(&settings, None));
        assert!(!resolve_worktree_cleanup(&settings, Some("off")).enabled());
        assert_eq!(resolve_worktree_cleanup(&settings, Some("custom")), WorktreeRules { after_days: Some(7.0), on_merge: false, on_delete: false, unchanged: false });
        // An environment-level policy is the base the projects without their own policy get.
        let mut environment = settings.clone();
        environment["worktreeCleanup"] = json!({"mode": "off"});
        assert!(!resolve_worktree_cleanup(&environment, None).enabled());
        assert!(!resolve_worktree_cleanup(&environment, Some("other")).enabled());
        assert!(any_worktree_policy(&environment, |rules| rules.after_days == Some(7.0)), "a project's custom rules count");
        assert!(!any_worktree_policy(&json!({"storageCleanup": {}, "projectSettingsOverrides": {}}), WorktreeRules::enabled));
    }

    #[test]
    fn only_a_change_to_a_cleanup_setting_asks_for_a_sweep() {
        let base = json!({"storageCleanup": {"worktreeAfterDays": 30}, "worktreeCleanup": null, "projectSettingsOverrides": {"p": {"worktreeCleanup": {"mode": "off"}, "sidebarAutoSettleOnMerge": true}}, "theme": "a"});
        assert!(!cleanup_settings_changed(&base, &base));
        let mut other = base.clone();
        other["theme"] = json!("b");
        other["projectSettingsOverrides"]["p"]["sidebarAutoSettleOnMerge"] = json!(false);
        assert!(!cleanup_settings_changed(&base, &other), "settings the cleanup does not read");
        for change in [
            |settings: &mut Value| settings["storageCleanup"]["worktreeAfterDays"] = json!(7),
            |settings: &mut Value| settings["worktreeCleanup"] = json!({"mode": "off"}),
            |settings: &mut Value| settings["projectSettingsOverrides"]["p"]["worktreeCleanup"] = json!({"mode": "custom", "rules": {}}),
            |settings: &mut Value| settings["projectSettingsOverrides"]["q"] = json!({"worktreeCleanup": {"mode": "off"}}),
            |settings: &mut Value| {
                settings["projectSettingsOverrides"].as_object_mut().unwrap().remove("p");
            },
        ] {
            let mut changed = base.clone();
            change(&mut changed);
            assert!(cleanup_settings_changed(&base, &changed), "{changed}");
        }
    }

    #[test]
    fn only_dependency_installs_may_be_ignored_and_rotated_logs_are_named_by_a_numeric_suffix() {
        assert!(is_dependency_directory("node_modules/") && is_dependency_directory("web/node_modules/"));
        assert!(!is_dependency_directory("my_node_modules/") && !is_dependency_directory(".env") && !is_dependency_directory("node_modules/x"));
        for name in ["server.log.1", "a.ndjson.12", "x.log.0"] {
            assert!(is_rotated_log(name), "{name}");
        }
        for name in ["server.log", "server.log.", "server.log.x", "server.txt.1", "log.1", "a.logs.1"] {
            assert!(!is_rotated_log(name), "{name}");
        }
    }
}
