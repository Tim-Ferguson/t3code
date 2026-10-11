//! The orchestration side of agent awareness: which committed events can change what a thread publishes to T3 Connect
//! (`relay/AgentAwarenessRelay.ts` `shouldPublishAgentAwarenessEvent`), what a thread shell projects to
//! (`packages/shared/src/agentAwareness.ts` `projectThreadAwarenessV2`), and the worker that publishes it. The decisions
//! (what is worth publishing again, what waits for confirmation, how a failed publish is retried, how the detail is cut)
//! and the signed request are `t3_remote::awareness`; this file feeds them from the store.
//!
//! The per-event cost while nothing is linked or publishing is off is one string comparison per event on the receiving
//! task (`should_publish`); the thread is read, and the opt-in secret looked at, only for an event that passed it, and a
//! thread already waiting in the queue costs a set lookup.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use t3_contracts::{EnvironmentId, ThreadId, TrimmedNonEmptyString};
use t3_remote::{
    awareness::{
        CATCH_UP_RETRY, CATCH_UP_UNLINKED_RETRY_MAX, CATCH_UP_UNLINKED_RETRY_START, PUBLISH_CONFIRM_DELAY, PublishDecision, PublishRetries, PublishTracker,
        PublishRelayConfig, PublishingStartupState, publish_thread_state, publishing_startup_state, read_publish_agent_activity_enabled, read_publish_relay_config,
        sanitize_state,
    },
    clock::{Clock, format_iso},
    control_plane::ControlPlane,
    env_keys::get_or_create_environment_key_pair,
    jwt::KeyPair,
    secrets::{RELAY_URL_SECRET, SecretStore, get_string},
    types::{AgentActivityState, AgentAwarenessPhase},
};
use tokio::{sync::Notify, task::JoinHandle};

use crate::persistence::{Store, StoredEvent};

/// The event types whose thread can publish a different state.
///
/// `projectThreadAwarenessV2` reads thread metadata, run status, pending requests and pending background work (a finished
/// subagent, a cleared roster or an ended background item can release a held completion). Message bodies and tool progress
/// cannot change the published activity.
pub fn should_publish(event: &StoredEvent) -> bool {
    let event = &event.event;
    match event.event_type.as_str() {
        // A thread imported from the first version's history is history, not news.
        "thread.created" => event.payload.get("historyOrigin").and_then(Value::as_str) != Some("v1_import"),
        "thread.archived" | "thread.unarchived" | "thread.deleted" | "thread.metadata-updated" | "thread.pull-request-synced" | "thread.model-selection-updated"
        | "thread.provider-switched" | "run.created" | "run.updated" | "runtime-request.updated" | "subagent.updated" | "provider-thread.updated" => true,
        "turn-item.updated" => crate::background_work::update_can_end_background_work(&event.payload),
        _ => false,
    }
}

fn holds_completion(kind: &str) -> bool {
    // Work that will wake the agent keeps the run going; a dev server does not. Work the adapter cannot name holds.
    kind != "command"
}

/// `projectThreadAwarenessV2` from a thread shell and the title of its project.
pub fn project_thread(environment_id: &str, project_title: &str, shell: &Value) -> Option<AgentActivityState> {
    if shell["lineage"]["relationshipToParent"] == "subagent" {
        return None;
    }
    let request = &shell["pendingRuntimeRequest"];
    let phase = if request["kind"] == "user_input" {
        AgentAwarenessPhase::WaitingForInput
    } else if !request.is_null() && request["kind"] != "auth_refresh" {
        AgentAwarenessPhase::WaitingForApproval
    } else {
        // `activityRunStatus ?? status`
        let status = if shell["activityRunStatus"].is_null() { &shell["status"] } else { &shell["activityRunStatus"] };
        match status.as_str()? {
            "preparing" | "starting" => AgentAwarenessPhase::Starting,
            "running" | "waiting" => AgentAwarenessPhase::Running,
            "completed" => {
                let holds = shell["pendingBackgroundTasks"].as_array().is_some_and(|tasks| tasks.iter().any(|task| task["kind"].as_str().is_none_or(holds_completion)));
                if holds { AgentAwarenessPhase::Running } else { AgentAwarenessPhase::Completed }
            }
            "failed" => AgentAwarenessPhase::Failed,
            _ => return None,
        }
    };
    let (headline, detail) = match phase {
        AgentAwarenessPhase::Starting => ("Starting agent", None),
        AgentAwarenessPhase::Running => ("Agent is working", None),
        AgentAwarenessPhase::WaitingForApproval => ("Approval needed", None),
        AgentAwarenessPhase::WaitingForInput => ("Waiting for input", None),
        AgentAwarenessPhase::Completed => ("Agent finished", Some("Review the completed task.")),
        AgentAwarenessPhase::Failed => ("Agent failed", Some("The agent run failed.")),
        AgentAwarenessPhase::Stale => ("Update delayed", None),
    };
    let thread_id = shell["id"].as_str()?;
    let text = |value: &str| TrimmedNonEmptyString::new(value).ok();
    Some(AgentActivityState {
        environment_id: EnvironmentId::new(environment_id).ok()?,
        thread_id: ThreadId::new(thread_id).ok()?,
        project_title: text(project_title)?,
        thread_title: text(shell["title"].as_str()?)?,
        phase,
        headline: text(headline)?,
        detail: detail.and_then(text),
        model_title: text(shell["modelSelection"]["model"].as_str()?)?,
        updated_at: text(shell["updatedAt"].as_str()?)?,
        deep_link: text(&format!("/threads/{}/{}", url_component(environment_id), url_component(thread_id)))?,
    })
}

/// `encodeURIComponent`.
fn url_component(value: &str) -> String {
    value.bytes().map(|byte| if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) { (byte as char).to_string() } else { format!("%{byte:02X}") }).collect()
}

fn millis(value: &Value) -> Option<i64> {
    value.as_str().and_then(|text| text.parse::<chrono::DateTime<chrono::Utc>>().ok()).map(|time| time.timestamp_millis())
}

pub struct AgentAwareness {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    secrets: Arc<dyn SecretStore>,
    control_plane: Arc<dyn ControlPlane>,
    clock: Arc<dyn Clock>,
    environment_id: String,
    started_at_ms: i64,
    /// Threads waiting to be published, each once, in the order they were asked for.
    queue: Mutex<Queue>,
    wake: Notify,
    /// Wakes the wait of the catch-up publish (`requestCatchUp`); one pending request is kept.
    catch_up: Notify,
    idle: tokio::sync::watch::Sender<usize>,
    state: tokio::sync::Mutex<Worker>,
    retries: Mutex<HashMap<String, Retry>>,
    snapshot_published: AtomicBool,
    key_pair: Mutex<Option<KeyPair>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<String>,
    queued: HashSet<String>,
    /// Queued plus being published.
    pending: usize,
}

#[derive(Default)]
struct Worker {
    tracker: PublishTracker,
    attempts: PublishRetries,
    published_config: Option<PublishRelayConfig>,
}

struct Retry {
    generation: u64,
    timer: Option<JoinHandle<()>>,
}

impl Clone for AgentAwareness {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl AgentAwareness {
    pub fn new(store: Store, secrets: Arc<dyn SecretStore>, control_plane: Arc<dyn ControlPlane>, clock: Arc<dyn Clock>, environment_id: String) -> Self {
        let started_at_ms = clock.now_ms();
        Self {
            inner: Arc::new(Inner {
                store,
                secrets,
                control_plane,
                clock,
                environment_id,
                started_at_ms,
                queue: Mutex::default(),
                wake: Notify::new(),
                catch_up: Notify::new(),
                idle: tokio::sync::watch::channel(0).0,
                state: tokio::sync::Mutex::default(),
                retries: Mutex::default(),
                snapshot_published: AtomicBool::new(false),
                key_pair: Mutex::new(None),
                tasks: Mutex::default(),
            }),
        }
    }

    /// Retries a pending catch-up publish now. Called after this process links or enables publishing.
    pub fn request_catch_up(&self) {
        self.inner.catch_up.notify_one();
    }

    /// Asks for a thread's state to be published; a thread already waiting is not queued twice.
    pub fn enqueue(&self, thread_id: &str) {
        self.cancel_retry(thread_id);
        self.enqueue_queued(thread_id);
    }

    fn enqueue_queued(&self, thread_id: &str) {
        let inner = &self.inner;
        {
            let mut queue = inner.queue.lock().unwrap();
            if !queue.queued.insert(thread_id.to_owned()) {
                return;
            }
            queue.waiting.push_back(thread_id.to_owned());
            queue.pending += 1;
            inner.idle.send_replace(queue.pending);
        }
        inner.wake.notify_one();
    }

    /// Completes when every thread asked for so far has been published, or found not to need it. Retries and confirmations
    /// that are still to come are not waited for.
    pub async fn drain(&self) {
        let mut idle = self.inner.idle.subscribe();
        let _ = idle.wait_for(|pending| *pending == 0).await;
    }

    /// Publishes one thread now and waits for it (`publishThread`).
    pub async fn publish_thread(&self, thread_id: &str) {
        self.enqueue(thread_id);
        self.drain().await;
    }

    /// Starts the worker, the listener of committed events, and the catch-up publish of threads that are active now.
    pub fn start(&self) {
        let events = self.inner.store.subscribe();
        let relay = read_publish_relay_config(self.inner.secrets.as_ref());
        let enabled = read_publish_agent_activity_enabled(self.inner.secrets.as_ref());
        match publishing_startup_state(relay.is_some(), enabled) {
            PublishingStartupState::WaitingForLink => tracing::info!("agent activity publishing standby; waiting for T3 Connect link reconciliation"),
            PublishingStartupState::Disabled => tracing::info!("agent activity publishing disabled by T3 Connect configuration"),
            PublishingStartupState::Enabled => tracing::info!(relay_url = relay.as_ref().map(|config| config.url.as_str()), "agent activity publishing enabled"),
        }
        let startup_enabled = enabled && relay.is_some();
        let mut tasks = self.inner.tasks.lock().unwrap();
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.work().await }));
        let me = self.clone();
        tasks.push(tokio::spawn(async move { me.listen(events).await }));
        let me = self.clone();
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            me.publish_active_threads_when_configured(!startup_enabled).await;
        }));
    }

    pub async fn shutdown(&self) {
        let tasks: Vec<_> = self.inner.tasks.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        let retries: Vec<_> = self.inner.retries.lock().unwrap().drain().collect();
        for (_, retry) in retries {
            if let Some(timer) = retry.timer {
                timer.abort();
            }
        }
    }

    async fn listen(&self, mut events: tokio::sync::broadcast::Receiver<Vec<StoredEvent>>) {
        loop {
            match events.recv().await {
                Ok(batch) => {
                    for event in batch.iter().filter(|event| should_publish(event)) {
                        self.enqueue(&event.event.aggregate_id);
                    }
                }
                // Events were missed: the catch-up of the active threads is the only way back, and it runs once a link exists.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    async fn work(&self) {
        loop {
            let next = {
                let mut queue = self.inner.queue.lock().unwrap();
                let next = queue.waiting.pop_front();
                if let Some(thread) = &next {
                    // Removing the id when processing starts allows one new queue entry for updates received while the
                    // current snapshot is being published.
                    queue.queued.remove(thread);
                }
                next
            };
            let Some(thread) = next else {
                self.inner.wake.notified().await;
                continue;
            };
            self.process(&thread).await;
            let mut queue = self.inner.queue.lock().unwrap();
            queue.pending -= 1;
            self.inner.idle.send_replace(queue.pending);
        }
    }

    async fn process(&self, thread_id: &str) {
        // A retry timer in flight is superseded by this publish.
        let timer = self.inner.retries.lock().unwrap().get_mut(thread_id).and_then(|retry| retry.timer.take());
        if let Some(timer) = timer {
            timer.abort();
        }
        match self.publish_unsafe(thread_id).await {
            Ok(()) => self.cancel_retry(thread_id),
            Err(error) => {
                tracing::warn!(thread_id, %error, "agent activity publish failed");
                self.schedule_retry(thread_id).await;
            }
        }
    }

    fn cancel_retry(&self, thread_id: &str) {
        if let Some(retry) = self.inner.retries.lock().unwrap().remove(thread_id) {
            if let Some(timer) = retry.timer {
                timer.abort();
            }
        }
    }

    async fn schedule_retry(&self, thread_id: &str) {
        let Some(delay) = self.inner.state.lock().await.attempts.next_delay(thread_id) else {
            // The exhausted budget stays until fresh activity or a successful publish clears it.
            tracing::warn!(thread_id, "agent activity publish retry budget exhausted");
            return;
        };
        let generation = {
            let retries = self.inner.retries.lock().unwrap();
            retries.get(thread_id).map_or(0, |retry| retry.generation) + 1
        };
        let me = self.clone();
        let thread = thread_id.to_owned();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let current = me.inner.retries.lock().unwrap().get(&thread).is_some_and(|retry| retry.generation == generation);
            if current {
                if let Some(retry) = me.inner.retries.lock().unwrap().get_mut(&thread) {
                    retry.timer = None;
                }
                me.enqueue_queued(&thread);
            }
        });
        if let Some(previous) = self.inner.retries.lock().unwrap().insert(thread_id.to_owned(), Retry { generation, timer: Some(timer) }) {
            if let Some(previous) = previous.timer {
                previous.abort();
            }
        }
    }

    fn schedule_confirm(&self, thread_id: &str) {
        let me = self.clone();
        let thread = thread_id.to_owned();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(PUBLISH_CONFIRM_DELAY).await;
            me.enqueue_queued(&thread);
        });
        self.inner.tasks.lock().unwrap().push(timer);
    }

    /// `publishThreadUnsafe`.
    async fn publish_unsafe(&self, thread_id: &str) -> Result<(), String> {
        let inner = &self.inner;
        let mut worker = inner.state.lock().await;
        if !read_publish_agent_activity_enabled(inner.secrets.as_ref()) {
            self.cancel_all_retries();
            worker.tracker.reset_connection();
            worker.published_config = None;
            tracing::debug!(thread_id, "agent activity publish skipped; publication disabled");
            return Ok(());
        }
        let Some(config) = read_publish_relay_config(inner.secrets.as_ref()) else {
            self.cancel_all_retries();
            worker.tracker.reset_connection();
            worker.published_config = None;
            tracing::debug!(thread_id, "agent activity publish skipped; relay link credentials unavailable");
            return Ok(());
        };
        if worker.published_config.as_ref() != Some(&config) {
            worker.tracker.reset_connection();
            worker.published_config = Some(config.clone());
        }
        let key_pair = self.key_pair()?;

        // Per-thread read: this runs for every activity-relevant event, so its cost must not grow with the number of threads.
        let projection = inner.store.projection("thread", thread_id).map_err(|error| error.to_string())?;
        let shell = projection.as_ref().filter(|projection| projection["thread"]["deletedAt"].is_null()).map(crate::thread::shell);
        if shell.as_ref().is_some_and(|shell| shell["lineage"]["relationshipToParent"] == "subagent") && !worker.tracker.has_published(thread_id) {
            // Subagents never project activity, so the relay holds no row to clear. Checked before the archive filter so
            // archiving one stays quiet too.
            return Ok(());
        }
        let live = shell.filter(|shell| shell["archivedAt"].is_null());
        let project = match &live {
            Some(shell) => shell["projectId"]
                .as_str()
                .map(|id| inner.store.projection("project", id))
                .transpose()
                .map_err(|error| error.to_string())?
                .flatten()
                .filter(|project| project["deletedAt"].is_null()),
            None => None,
        };
        let state = match (&live, &project) {
            (Some(shell), Some(project)) => sanitize_state(project_thread(&inner.environment_id, project["title"].as_str().unwrap_or_default(), shell)),
            _ => None,
        };
        let terminal_since_start = live.as_ref().and_then(|shell| millis(&shell["latestRunCompletedAt"])).is_some_and(|completed| completed > inner.started_at_ms);
        let now_ms = inner.clock.now_ms();
        match worker.tracker.decide(thread_id, state.as_ref(), terminal_since_start, now_ms) {
            PublishDecision::Skip => Ok(()),
            PublishDecision::DeferThenRecheck => {
                tracing::info!(thread_id, "agent activity publish deferred pending confirmation");
                self.schedule_confirm(thread_id);
                Ok(())
            }
            PublishDecision::Publish => {
                publish_thread_state(inner.control_plane.as_ref(), inner.clock.as_ref(), &key_pair, &config, &inner.environment_id, thread_id, state.clone())
                    .await
                    .map_err(|error| error.to_string())?;
                worker.tracker.record_published(thread_id, state.as_ref());
                worker.attempts.clear(thread_id);
                Ok(())
            }
        }
    }

    fn cancel_all_retries(&self) {
        let retries: Vec<_> = self.inner.retries.lock().unwrap().drain().collect();
        for (_, retry) in retries {
            if let Some(timer) = retry.timer {
                timer.abort();
            }
        }
    }

    fn key_pair(&self) -> Result<KeyPair, String> {
        let mut cached = self.inner.key_pair.lock().unwrap();
        if let Some(pair) = cached.as_ref() {
            return Ok(pair.clone());
        }
        let pair = get_or_create_environment_key_pair(self.inner.secrets.as_ref()).map_err(|error| error.to_string())?;
        *cached = Some(pair.clone());
        Ok(pair)
    }

    /// `publishActiveThreadsUnsafe`: why the catch-up did not run, or that it did.
    async fn publish_active_threads(&self) -> CatchUp {
        let inner = &self.inner;
        if get_string(inner.secrets.as_ref(), RELAY_URL_SECRET).ok().flatten().is_none_or(|url| url.is_empty()) {
            tracing::debug!("agent activity snapshot skipped; relay link credentials unavailable");
            return CatchUp::Unlinked;
        }
        if !read_publish_agent_activity_enabled(inner.secrets.as_ref()) {
            tracing::debug!("agent activity snapshot skipped; publication disabled");
            return CatchUp::Disabled;
        }
        if read_publish_relay_config(inner.secrets.as_ref()).is_none() {
            return CatchUp::Unlinked;
        }
        let Ok(snapshot) = crate::thread::ThreadService::new(inner.store.clone()).shell_snapshot() else { return CatchUp::Failed };
        let titles: HashMap<&str, &str> = snapshot["projects"].as_array().into_iter().flatten().filter_map(|project| Some((project["id"].as_str()?, project["title"].as_str()?))).collect();
        let active: Vec<String> = snapshot["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|shell| {
                let title = titles.get(shell["projectId"].as_str()?)?;
                let state = project_thread(&inner.environment_id, title, shell)?;
                // A finished thread is news only when it finished after this server started.
                let terminal = matches!(state.phase, AgentAwarenessPhase::Completed | AgentAwarenessPhase::Failed);
                (!terminal || millis(&shell["latestRunCompletedAt"]).is_some_and(|completed| completed > inner.started_at_ms)).then(|| shell["id"].as_str().map(str::to_owned))?
            })
            .collect();
        if active.is_empty() {
            tracing::debug!("agent activity snapshot has no publishable threads");
            return CatchUp::Published;
        }
        tracing::info!(count = active.len(), "publishing active agent activity snapshot");
        for thread in &active {
            self.enqueue(thread);
        }
        self.drain().await;
        CatchUp::Published
    }

    /// Publishes the catch-up snapshot of active threads once the environment is linked and publishing is enabled. Many
    /// environments never link, so while unlinked the retry backs off from 5 s to 60 s; `request_catch_up` ends the wait early.
    async fn publish_active_threads_when_configured(&self, log_enabled_when_ready: bool) {
        let mut unlinked_delay = CATCH_UP_UNLINKED_RETRY_START;
        while !self.inner.snapshot_published.load(Ordering::SeqCst) {
            let result = self.publish_active_threads().await;
            if result == CatchUp::Published {
                self.inner.snapshot_published.store(true, Ordering::SeqCst);
                if log_enabled_when_ready {
                    tracing::info!(relay_url = read_publish_relay_config(self.inner.secrets.as_ref()).map(|config| config.url), "agent activity publishing enabled after link reconciliation");
                }
                return;
            }
            let delay = if result == CatchUp::Unlinked { unlinked_delay } else { CATCH_UP_RETRY };
            tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = self.inner.catch_up.notified() => {} }
            if result == CatchUp::Unlinked {
                unlinked_delay = (unlinked_delay * 2).min(CATCH_UP_UNLINKED_RETRY_MAX);
            }
        }
    }

    /// The instant this server started, in the clock's terms, as an ISO time.
    pub fn started_at(&self) -> String {
        format_iso(self.inner.started_at_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatchUp {
    Published,
    Unlinked,
    Disabled,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell(overrides: Value) -> Value {
        let mut shell = json!({"id":"thread-1","projectId":"p","title":"Fix the bug","modelSelection":{"instanceId":"codex","model":"gpt-5"},"updatedAt":"2026-01-01T00:00:00.000Z","status":"idle","activityRunStatus":null,"pendingRuntimeRequest":null,"pendingBackgroundTasks":[],"lineage":{"relationshipToParent":"root"}});
        for (key, value) in overrides.as_object().unwrap() {
            shell[key] = value.clone();
        }
        shell
    }

    fn phase(overrides: Value) -> Option<(&'static str, String, Option<String>)> {
        project_thread("env-1", "Project", &shell(overrides)).map(|state| {
            let wire = serde_json::to_value(&state).unwrap();
            let phase = ["starting", "running", "waiting_for_approval", "waiting_for_input", "completed", "failed", "stale"].into_iter().find(|name| wire["phase"] == *name).unwrap();
            (phase, wire["headline"].as_str().unwrap().to_owned(), wire["detail"].as_str().map(str::to_owned))
        })
    }

    #[test]
    fn the_phase_ladder_is_the_originals() {
        assert_eq!(phase(json!({"status":"idle"})), None);
        for (status, expected) in [("preparing", "starting"), ("starting", "starting"), ("running", "running"), ("waiting", "running"), ("failed", "failed"), ("completed", "completed")] {
            assert_eq!(phase(json!({ "status": status })).map(|found| found.0), Some(expected), "{status}");
        }
        for status in ["idle", "queued", "interrupted", "cancelled", "rolled_back"] {
            assert_eq!(phase(json!({ "status": status })), None, "{status}");
        }
        // The activity run wins over the latest run's status.
        assert_eq!(phase(json!({"status":"completed","activityRunStatus":"running"})).map(|found| found.0), Some("running"));
        // A pending request outranks every run status; auth refreshes do not count.
        assert_eq!(phase(json!({"status":"running","pendingRuntimeRequest":{"kind":"user_input"}})).map(|found| found.0), Some("waiting_for_input"));
        assert_eq!(phase(json!({"status":"running","pendingRuntimeRequest":{"kind":"command_approval"}})).map(|found| found.0), Some("waiting_for_approval"));
        assert_eq!(phase(json!({"status":"running","pendingRuntimeRequest":{"kind":"auth_refresh"}})).map(|found| found.0), Some("running"));
        assert_eq!(phase(json!({"status":"completed"})), Some(("completed", "Agent finished".into(), Some("Review the completed task.".into()))));
        assert_eq!(phase(json!({"status":"failed"})), Some(("failed", "Agent failed".into(), Some("The agent run failed.".into()))));
        assert_eq!(phase(json!({"lineage":{"relationshipToParent":"subagent"},"status":"running"})), None);
    }

    #[test]
    fn background_work_that_wakes_the_agent_holds_a_completion() {
        let completed = |kind: &str| phase(json!({"status":"completed","pendingBackgroundTasks":[{"taskId":"t","kind":kind}]})).map(|found| found.0);
        assert_eq!(completed("command"), Some("completed"));
        for kind in ["subagent", "monitor", "background_task", "unheard-of"] {
            assert_eq!(completed(kind), Some("running"), "{kind}");
        }
    }

    #[test]
    fn the_state_names_its_thread_and_links_to_it() {
        let state = project_thread("env/1", "Project", &shell(json!({"status":"running"}))).unwrap();
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(wire["deepLink"], "/threads/env%2F1/thread-1");
        assert_eq!(wire["modelTitle"], "gpt-5");
        assert_eq!(wire["projectTitle"], "Project");
        assert_eq!(wire["threadTitle"], "Fix the bug");
        assert_eq!(wire["updatedAt"], "2026-01-01T00:00:00.000Z");
        assert!(wire.get("detail").is_none());
    }

    fn event(kind: &str, payload: Value) -> StoredEvent {
        serde_json::from_value(json!({"sequence":1,"eventId":"e","aggregateKind":"thread","aggregateId":"thread-1","occurredAt":"2026-01-01T00:00:00.000Z","commandId":null,"causationEventId":null,"correlationId":null,"type":kind,"payload":payload,"metadata":{}})).unwrap()
    }

    #[test]
    fn only_events_that_can_change_the_published_state_are_taken() {
        for kind in [
            "thread.created", "thread.archived", "thread.unarchived", "thread.deleted", "thread.metadata-updated", "thread.pull-request-synced", "thread.model-selection-updated",
            "thread.provider-switched", "run.created", "run.updated", "runtime-request.updated", "subagent.updated", "provider-thread.updated",
        ] {
            assert!(should_publish(&event(kind, json!({}))), "{kind}");
        }
        for kind in [
            "thread.settled", "thread.unsettled", "thread.snoozed", "thread.unsnoozed", "thread.auto-settle-set", "thread.pinned", "thread.unpinned", "thread.pin-reordered",
            "thread.active-reordered", "thread.visited", "thread.marked-unread", "thread.runtime-mode-updated", "thread.interaction-mode-updated", "run.background-work-cancelled",
            "run-attempt.created", "run-attempt.updated", "node.updated", "provider-session.attached", "provider-session.updated", "provider-session.detached", "provider-turn.updated",
            "message.updated", "plan.updated", "checkpoint-scope.created", "checkpoint.captured", "checkpoint.rollback-requested", "context-handoff.updated",
            "context-transfer.created", "context-transfer.updated", "something.unheard-of",
        ] {
            assert!(!should_publish(&event(kind, json!({}))), "{kind}");
        }
        assert!(!should_publish(&event("thread.created", json!({"historyOrigin":"v1_import"}))));
        assert!(should_publish(&event("thread.created", json!({"historyOrigin":"native"}))));
        // A turn item counts only when it is background work that is no longer active.
        assert!(should_publish(&event("turn-item.updated", json!({"type":"subagent","status":"completed"}))));
        assert!(should_publish(&event("turn-item.updated", json!({"type":"command_execution","status":"failed"}))));
        assert!(!should_publish(&event("turn-item.updated", json!({"type":"subagent","status":"running"}))));
        assert!(!should_publish(&event("turn-item.updated", json!({"type":"assistant_message","status":"completed"}))));
        assert!(!should_publish(&event("turn-item.updated", json!({}))));
    }

    /// What the publisher costs the path every committed event takes. Run on demand, not by the suite:
    ///   cargo test -p t3-server --lib agent_awareness::tests::awareness_bench -- --ignored --nocapture
    /// The event types and payloads are those of the original's recorded events (`event-payloads.jsonl.gz`), repeated; the filter
    /// is what every event pays, the worker what a passing event pays when nothing is linked (two secret reads) and when
    /// publishing is on (a thread read and a projection, then the decision).
    #[test]
    #[ignore]
    fn awareness_bench() {
        use std::io::Read;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&include_bytes!("../tests/fixtures/event-payloads.jsonl.gz")[..]).read_to_string(&mut text).unwrap();
        let rows: Vec<Value> = text.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|row| row["aggregate"] == "thread").collect();
        let events: Vec<StoredEvent> = (0..70_000).map(|index| {
            let row = &rows[index % rows.len()];
            event(row["type"].as_str().unwrap(), row["canonical"].clone())
        }).collect();
        let started = std::time::Instant::now();
        let mut passing = 0usize;
        for _ in 0..10 {
            passing += events.iter().filter(|event| should_publish(event)).count();
        }
        let elapsed = started.elapsed();
        eprintln!("filter: {:.0} ns per event over {} events ({} passed per pass of {})", elapsed.as_nanos() as f64 / (10.0 * events.len() as f64), 10 * events.len(), passing / 10, events.len());

        // What a passing event costs the worker while publishing is off: the opt-in is looked at, and nothing else.
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-awareness-bench-").tempdir().unwrap();
        let secrets = crate::connect::ConnectSecrets(crate::server_secret_store::ServerSecretStore::open(directory.path().join("secrets")).unwrap());
        let rounds = 20_000;
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(read_publish_agent_activity_enabled(&secrets));
        }
        eprintln!("opt-in read (publishing off): {:.0} ns", started.elapsed().as_nanos() as f64 / rounds as f64);
        // And when publishing is on: a shell projected to the state that is published.
        let shell_value = shell(json!({"status":"running"}));
        let started = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(project_thread("env-1", "Project", &shell_value));
        }
        eprintln!("shell projected to a state: {:.0} ns", started.elapsed().as_nanos() as f64 / rounds as f64);
    }
}
