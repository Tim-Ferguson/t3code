//! Tracked repository clones that back a freshly added project (port of
//! `project/ProjectCloneTracker.ts`): `projectClone.start`, `projectClone.cancel`,
//! `projectClone.retry` and the `subscribeProjectClones` stream.
//!
//! A clone is detached from the request that started it: the project already exists (pointing at
//! the empty destination), and progress is shown anywhere through the stream. Snapshots are
//! memory only. A done clone is dropped after a grace window; a failed or cancelled one stays
//! until it is retried or its project is deleted, since the empty project is the durable record
//! the user can act on. A server restart forgets in-flight clones.
//!
//! Cancelling aborts the clone task, which drops the git process (`kill_on_drop`), waits for the
//! task to end, then empties the destination so a retry starts clean. The destination directory
//! itself stays: it is the project's workspace root.
use crate::{
    persistence::Store,
    project::ProjectService,
    source_control_repository::{CloneProgress, CloneProgressCallback, PreparedClone, SourceControlRepositoryService},
    vcs_service::{VcsContext, VcsService},
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use t3_contracts::{
    PROJECT_CLONE_DETAIL_MAX_LENGTH, PROJECT_CLONE_ERROR_MAX_LENGTH, ProjectCloneActionInput, ProjectCloneActionResult,
    ProjectClonePhase, ProjectCloneSnapshot, ProjectCloneStage, ProjectCloneStartInput, ProjectCloneStartResult,
    SourceControlCloneRepositoryInput, SourceControlProviderKind, SourceControlRepositoryError, TrimmedNonEmptyString,
};
use tokio::{sync::watch, task::JoinHandle};

pub const METHODS: [&str; 3] = ["projectClone.start", "projectClone.cancel", "projectClone.retry"];
pub const STREAM: &str = "subscribeProjectClones";

/// Finished snapshots stay visible this long so a late subscriber sees the outcome.
const DONE_RETENTION: Duration = Duration::from_secs(30);

/// What the tracker needs from the rest of the server, captured when a clone starts so a retry
/// reuses the starting server's settings and credentials.
pub struct Hooks {
    pub service: SourceControlRepositoryService,
    pub store: Store,
    pub vcs: VcsContext,
}

impl Hooks {
    /// `createProject`: the project exists before its files do, rooted at the empty destination.
    async fn create_project(&self, project_id: &str, title: &str, root: &str) -> Result<(), String> {
        tokio::fs::create_dir_all(root).await.map_err(|error| error.to_string())?;
        let store = self.store.clone();
        let payload = json!({"type":"project.create","commandId":format!("project-clone-create:{project_id}"),"projectId":project_id,"title":title,"workspaceRoot":root});
        tokio::task::spawn_blocking(move || ProjectService::new(store).mutate(payload, Utc::now()))
            .await
            .map_err(|error| error.to_string())?
            .map(|_| ())
            .map_err(|error| crate::project::mutation_message(&error))
    }

    /// `onCloned`: the project is touched so clients re-read it, and the checkout's git status is
    /// refreshed. Failures here never undo a finished clone.
    async fn on_cloned(&self, project_id: &str, root: &str) {
        let store = self.store.clone();
        let payload = json!({"type":"project.update","commandId":format!("project-clone-done:{project_id}"),"projectId":project_id});
        match tokio::task::spawn_blocking(move || ProjectService::new(store).mutate(payload, Utc::now())).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "Project update after a clone failed"),
            Err(error) => tracing::warn!(%error, "Project update after a clone did not run"),
        }
        if let Err(error) = Box::pin(VcsService::shared().refresh_status(root, &self.vcs)).await {
            tracing::warn!(%error, "VCS status refresh after a clone failed");
        }
    }
}

struct Tracked {
    snapshot: ProjectCloneSnapshot,
    task: Option<JoinHandle<()>>,
    hooks: Arc<Hooks>,
    prepared: PreparedClone,
    provider: SourceControlProviderKind,
}

struct Inner {
    entries: Mutex<HashMap<String, Tracked>>,
    retention_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    sequence: AtomicU64,
    changes: watch::Sender<Arc<Vec<ProjectCloneSnapshot>>>,
    /// start, cancel, retry and discard mutate the same entry and directory; one at a time keeps a
    /// double-clicked Retry from racing two clones into it.
    action: tokio::sync::Mutex<()>,
    retention: Duration,
}

#[derive(Clone)]
pub struct ProjectCloneTracker(Arc<Inner>);

/// Why `projectClone.start` failed, as the original's two error types.
#[derive(Debug)]
pub enum StartError {
    Repository(SourceControlRepositoryError),
    Dispatch(String),
}

impl StartError {
    pub fn wire(&self) -> Value {
        match self {
            Self::Repository(error) => error.wire(),
            Self::Dispatch(message) => json!({"_tag":"OrchestrationDispatchCommandError","message":message}),
        }
    }
}

impl From<SourceControlRepositoryError> for StartError {
    fn from(error: SourceControlRepositoryError) -> Self {
        Self::Repository(error)
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `clampText`: at most `max` UTF-16 units, ending in an ellipsis when cut.
fn clamp(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut units = 0;
    for character in text.chars() {
        if units + character.len_utf16() > max - 1 {
            break;
        }
        units += character.len_utf16();
        kept.push(character);
    }
    kept.push('…');
    kept
}

/// The tracker of the server that owns `store`. One per state directory, so a clone outlives the
/// request that started it and every connection sees the same list.
pub fn of(store: &Store) -> ProjectCloneTracker {
    static TRACKERS: OnceLock<Mutex<HashMap<String, ProjectCloneTracker>>> = OnceLock::new();
    let key = store.state_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_else(|| format!("memory:{}", store.identity()));
    TRACKERS.get_or_init(Default::default).lock().unwrap().entry(key).or_insert_with(|| ProjectCloneTracker::new(DONE_RETENTION)).clone()
}

impl ProjectCloneTracker {
    pub fn new(retention: Duration) -> Self {
        let (changes, _) = watch::channel(Arc::new(Vec::new()));
        Self(Arc::new(Inner {
            entries: Mutex::new(HashMap::new()),
            retention_tasks: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(0),
            changes,
            action: tokio::sync::Mutex::new(()),
            retention,
        }))
    }

    /// Every tracked clone first, then the whole list after each change. A subscriber that falls
    /// behind sees only the newest list, which is safe because lists are whole states.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Vec<ProjectCloneSnapshot>>> {
        self.0.changes.subscribe()
    }

    pub fn get(&self, project_id: &str) -> Option<ProjectCloneSnapshot> {
        self.0.entries.lock().unwrap().get(project_id).map(|tracked| tracked.snapshot.clone())
    }

    fn publish(&self) {
        let list: Vec<ProjectCloneSnapshot> = {
            let entries = self.0.entries.lock().unwrap();
            let mut list: Vec<_> = entries.values().map(|tracked| tracked.snapshot.clone()).collect();
            list.sort_by_key(|snapshot| snapshot.sequence);
            list
        };
        self.0.changes.send_replace(Arc::new(list));
    }

    /// Applies `change` to a tracked clone under the next sequence number and publishes.
    fn modify(&self, project_id: &str, change: impl FnOnce(&mut Tracked)) -> bool {
        {
            let mut entries = self.0.entries.lock().unwrap();
            let Some(tracked) = entries.get_mut(project_id) else { return false };
            change(tracked);
            tracked.snapshot.sequence = self.0.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        }
        self.publish();
        true
    }

    fn clear_retention(&self, project_id: &str) {
        if let Some(task) = self.0.retention_tasks.lock().unwrap().remove(project_id) {
            task.abort();
        }
    }

    fn remove(&self, project_id: &str) {
        self.0.entries.lock().unwrap().remove(project_id);
        self.publish();
    }

    fn schedule_removal(&self, project_id: &str) {
        self.clear_retention(project_id);
        let (tracker, id, delay) = (self.clone(), project_id.to_string(), self.0.retention);
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            tracker.0.retention_tasks.lock().unwrap().remove(&id);
            tracker.remove(&id);
        });
        self.0.retention_tasks.lock().unwrap().insert(project_id.to_string(), task);
    }

    fn progress(&self, project_id: &str, update: CloneProgress) {
        self.modify(project_id, |tracked| {
            let snapshot = &mut tracked.snapshot;
            snapshot.stage = update.stage;
            snapshot.percent = update.percent;
            snapshot.detail = update.detail.map(|detail| clamp(&detail, PROJECT_CLONE_DETAIL_MAX_LENGTH));
        });
    }

    fn finish(&self, project_id: &str, phase: ProjectClonePhase, error: Option<String>) {
        let applied = self.modify(project_id, |tracked| {
            tracked.task = None;
            let snapshot = &mut tracked.snapshot;
            snapshot.phase = phase;
            snapshot.ended_at = Some(now_iso());
            if phase == ProjectClonePhase::Done {
                snapshot.percent = Some(100);
            }
            snapshot.error = error.map(|error| clamp(&error, PROJECT_CLONE_ERROR_MAX_LENGTH));
        });
        if applied && phase == ProjectClonePhase::Done {
            self.schedule_removal(project_id);
        }
    }

    /// Runs the clone in its own task. Once git has finished the clone is marked done before the
    /// post-clone hook runs, so a late Cancel cannot tear down a complete checkout.
    fn launch(&self, project_id: &str) {
        let (hooks, prepared, provider) = {
            let entries = self.0.entries.lock().unwrap();
            let Some(tracked) = entries.get(project_id) else { return };
            (tracked.hooks.clone(), tracked.prepared.clone(), tracked.provider)
        };
        let (tracker, id) = (self.clone(), project_id.to_string());
        let task = tokio::spawn(async move {
            let progress: CloneProgressCallback = {
                let (tracker, id) = (tracker.clone(), id.clone());
                Arc::new(move |update| tracker.progress(&id, update))
            };
            let outcome = Box::pin(hooks.service.run_clone(&prepared, provider, Some(progress), t3_vcs::core::GitTimeout::Unlimited)).await;
            match outcome {
                Ok(()) => {
                    tracker.finish(&id, ProjectClonePhase::Done, None);
                    hooks.on_cloned(&id, &prepared.destination_path.to_string_lossy()).await;
                }
                Err(error) => {
                    let detail = if error.detail.trim().is_empty() { "The repository could not be cloned.".to_string() } else { error.detail.clone() };
                    tracker.finish(&id, ProjectClonePhase::Failed, Some(detail));
                }
            }
        });
        let mut entries = self.0.entries.lock().unwrap();
        match entries.get_mut(project_id) {
            Some(tracked) if tracked.snapshot.phase == ProjectClonePhase::Running => tracked.task = Some(task),
            _ => {}
        }
    }

    /// Resolves the remote, creates the project, and starts the clone in the background. Fails
    /// before creating anything when the destination or repository is unusable so the caller can
    /// report it inline.
    pub async fn start(&self, input: ProjectCloneStartInput, hooks: Arc<Hooks>) -> Result<ProjectCloneStartResult, StartError> {
        let _action = self.0.action.lock().await;
        let provider = input.provider.unwrap_or(SourceControlProviderKind::Unknown);
        let prepared = hooks
            .service
            .prepare_clone(&SourceControlCloneRepositoryInput {
                provider: input.provider,
                repository: input.repository.clone(),
                remote_url: input.remote_url.clone(),
                destination_path: input.destination_path.clone(),
                protocol: input.protocol,
            })
            .await?;
        let project_id = input.project_id.as_str().to_string();
        let destination = prepared.destination_path.to_string_lossy().into_owned();
        let snapshot = ProjectCloneSnapshot {
            project_id: input.project_id.clone(),
            remote_url: TrimmedNonEmptyString::new(&prepared.remote_url).map_err(|error| StartError::Dispatch(error.to_string()))?,
            destination_path: TrimmedNonEmptyString::new(&destination).map_err(|error| StartError::Dispatch(error.to_string()))?,
            repository: prepared.repository.clone(),
            phase: ProjectClonePhase::Running,
            stage: ProjectCloneStage::Connecting,
            percent: None,
            detail: None,
            error: None,
            started_at: now_iso(),
            ended_at: None,
            sequence: self.0.sequence.fetch_add(1, Ordering::SeqCst) + 1,
        };
        {
            let mut entries = self.0.entries.lock().unwrap();
            // A second start for the same project, or for a destination another clone already
            // owns, must not race two gits into one directory.
            if entries.values().any(|tracked| tracked.snapshot.project_id.as_str() == project_id || tracked.prepared.destination_path == prepared.destination_path) {
                return Err(SourceControlRepositoryError::new(provider, "cloneRepository", "A clone into this destination is already in progress.").into());
            }
            entries.insert(project_id.clone(), Tracked { snapshot, task: None, hooks: hooks.clone(), prepared: prepared.clone(), provider });
        }
        // Everything after the claim runs to completion even if the requesting connection drops:
        // a claimed entry with no task could neither be cancelled nor retried. A failure in here
        // releases the claim.
        let (tracker, title, created_id) = (self.clone(), input.title.as_str().to_string(), project_id.clone());
        let created = tokio::spawn({
            let destination = destination.clone();
            async move {
                tracker.clear_retention(&created_id);
                // The entry is registered before the project exists so a thread launch or a
                // project delete racing this call already sees it.
                if let Err(message) = hooks.create_project(&created_id, &title, &destination).await {
                    tracing::warn!(%message, "Creating the project for a clone failed");
                    tracker.remove(&created_id);
                    return Err("Failed to create clone project.".to_string());
                }
                tracker.publish();
                tracker.launch(&created_id);
                Ok(())
            }
        })
        .await
        .map_err(|error| StartError::Dispatch(error.to_string()))?;
        created.map_err(StartError::Dispatch)?;
        Ok(ProjectCloneStartResult {
            project_id: input.project_id,
            cwd: TrimmedNonEmptyString::new(&destination).map_err(|error| StartError::Dispatch(error.to_string()))?,
            remote_url: TrimmedNonEmptyString::new(&prepared.remote_url).map_err(|error| StartError::Dispatch(error.to_string()))?,
            repository: prepared.repository,
        })
    }

    /// Interrupts a running clone and deletes the partial checkout. `false` when there is none.
    pub async fn cancel(&self, project_id: &str) -> bool {
        let _action = self.0.action.lock().await;
        let (task, hooks, destination) = {
            let mut entries = self.0.entries.lock().unwrap();
            let Some(tracked) = entries.get_mut(project_id) else { return false };
            if tracked.snapshot.phase != ProjectClonePhase::Running || tracked.task.is_none() {
                return false;
            }
            (tracked.task.take().unwrap(), tracked.hooks.clone(), tracked.prepared.destination_path.clone())
        };
        // Past this point the cancel finishes even if the requesting connection drops: a dead
        // task must not stay behind a "running" snapshot.
        let tracker = self.clone();
        let id = project_id.to_string();
        let _ = tokio::spawn(async move {
            task.abort();
            let _ = task.await;
            let phase = tracker.get(&id).map(|snapshot| snapshot.phase);
            // Git finished in the window before the abort landed: keep it.
            if phase == Some(ProjectClonePhase::Done) {
                return;
            }
            if phase == Some(ProjectClonePhase::Running) {
                tracker.finish(&id, ProjectClonePhase::Cancelled, None);
            }
            // The partial checkout goes so a retry starts from an empty destination.
            if let Err(error) = hooks.service.discard_clone(&destination.to_string_lossy()).await {
                tracing::warn!(%error, "Discarding a cancelled clone failed");
            }
        })
        .await;
        true
    }

    /// Restarts a failed or cancelled clone into the same destination. `Ok(false)` when there is
    /// nothing to retry.
    pub async fn retry(&self, project_id: &str) -> Result<bool, SourceControlRepositoryError> {
        let _action = self.0.action.lock().await;
        let (hooks, destination) = {
            let entries = self.0.entries.lock().unwrap();
            let Some(tracked) = entries.get(project_id) else { return Ok(false) };
            if !matches!(tracked.snapshot.phase, ProjectClonePhase::Failed | ProjectClonePhase::Cancelled) {
                return Ok(false);
            }
            (tracked.hooks.clone(), tracked.prepared.destination_path.clone())
        };
        // A retry into leftover files would fail on the non-empty destination with a less useful
        // message, so a cleanup failure is the error here.
        hooks.service.discard_clone(&destination.to_string_lossy()).await?;
        let started_at = now_iso();
        self.modify(project_id, |tracked| {
            let snapshot = &mut tracked.snapshot;
            snapshot.phase = ProjectClonePhase::Running;
            snapshot.stage = ProjectCloneStage::Connecting;
            snapshot.percent = None;
            snapshot.detail = None;
            snapshot.error = None;
            snapshot.started_at = started_at;
            snapshot.ended_at = None;
        });
        self.launch(project_id);
        Ok(true)
    }

    /// Stops tracking a project's clone, ending it if it still runs and removing an unfinished
    /// checkout. Called when the project is deleted.
    pub async fn discard(&self, project_id: &str) {
        let _action = self.0.action.lock().await;
        let (task, hooks, destination) = {
            let mut entries = self.0.entries.lock().unwrap();
            let Some(tracked) = entries.get_mut(project_id) else { return };
            (tracked.task.take(), tracked.hooks.clone(), tracked.prepared.destination_path.clone())
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        // Git may have finished while the abort was landing; the project is going away either
        // way, but a complete checkout is the user's.
        if self.get(project_id).map(|snapshot| snapshot.phase) != Some(ProjectClonePhase::Done) {
            let _ = hooks.service.discard_clone(&destination.to_string_lossy()).await;
        }
        self.clear_retention(project_id);
        self.remove(project_id);
    }
}

/// What a command on a project's files meets while its clone has not landed
/// (`rejectCommandsDuringClone`): the message of the refusal, or `None` to go ahead.
pub fn refusal(store: &Store, project_id: &str) -> Option<&'static str> {
    match of(store).get(project_id)?.phase {
        ProjectClonePhase::Done => None,
        ProjectClonePhase::Running => Some("The repository is still being cloned."),
        _ => Some("The repository was not cloned. Retry the clone first."),
    }
}

fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("typed project clone result")
}

fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}

pub async fn command(state: &crate::transport::ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let tracker = of(&state.store);
    match tag {
        "projectClone.start" => {
            let input: ProjectCloneStartInput = decode(payload)?;
            let hooks = Arc::new(Hooks {
                service: SourceControlRepositoryService::new(crate::source_control_registry::SourceControlRegistry::new(crate::source_control_rpc::host(state))),
                store: state.store.clone(),
                vcs: crate::vcs_rpc::context(state),
            });
            let result = Box::pin(tracker.start(input, hooks)).await.map_err(|error| error.wire())?;
            Ok(encode(&result))
        }
        "projectClone.cancel" => {
            let input: ProjectCloneActionInput = decode(payload)?;
            Ok(encode(&ProjectCloneActionResult { applied: Box::pin(tracker.cancel(input.project_id.as_str())).await }))
        }
        "projectClone.retry" => {
            let input: ProjectCloneActionInput = decode(payload)?;
            let applied = Box::pin(tracker.retry(input.project_id.as_str())).await.map_err(|error| error.wire())?;
            Ok(encode(&ProjectCloneActionResult { applied }))
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}
