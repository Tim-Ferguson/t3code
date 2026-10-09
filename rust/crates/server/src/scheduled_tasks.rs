//! Scheduled tasks: a prompt that the server launches as an agent turn on a schedule, when a webhook
//! is called, or on demand (port of `apps/server/src/scheduledTasks/ScheduledTaskService.ts` and
//! `scheduling/Scheduler.ts`).
//!
//! How a run happens, and why it cannot happen twice:
//! - A run is claimed in one SQLite transaction that marks the task `running` and writes a run record
//!   holding the command and message ids the launch will use. Only a task that is not `running` can be
//!   claimed, so two claims of one task never both succeed.
//! - The launch goes through the same orchestration commands a client uses, under those ids. A launch
//!   command is recorded in the command receipt table when it commits, so repeating it with the same id
//!   is a no-op that returns the thread the first attempt made.
//! - If the server stops between claim and completion, startup recovery finds the `running` task. When
//!   the launch command's receipt exists the run is recorded as `succeeded` against that thread;
//!   otherwise nothing was launched and it is recorded as `failed` ("interrupted by a server restart").
//!   Either way the task moves to its next occurrence and the run is never started again.
//!
//! Every decision (what is due, what was missed, when a task runs next) reads stored state and the
//! injected clock: the timer task only decides when to look again. A fixed-time run that is more than
//! ten minutes late is skipped to its next occurrence; an overdue interval task catches up with a single
//! run. Scheduled runs do not consult the background policy, as in the original.
use crate::{
    execution::ExecutionService,
    persistence::{Store, StoreError},
    scheduled_tasks_schedule::{
        LocalZone, Zone, is_missed_fixed_time_run, is_same_schedule, iso_from_ms, next_scheduled_run_at, parse_iso,
    },
    scheduled_tasks_webhook::{
        WebhookRequest, constant_time_equals, redact_headers, redact_query, render_webhook_prompt, verify_webhook_signature,
    },
};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use t3_contracts::{
    IntervalMs, IntervalScheduleFields, ScheduledTask, ScheduledTaskDeleteInput, ScheduledTaskDeleteResult,
    ScheduledTaskGetWebhookDeliveryInput, ScheduledTaskGetWebhookDeliveryResult, ScheduledTaskId,
    ScheduledTaskListResult, ScheduledTaskListWebhookDeliveriesInput, ScheduledTaskListWebhookDeliveriesResult,
    ScheduledTaskRotateWebhookTokenInput, ScheduledTaskRunNowInput, ScheduledTaskSchedule, ScheduledTaskSetEnabledInput,
    ScheduledTaskUpsertInput, ScheduledTaskUpsertSchedule, ScheduledTaskWebhookDeliveryOutcome, ScheduledTaskWebhookSignature,
    TrimmedNonEmptyString, WebhookScheduleFields, trim_wire_string,
};
use tokio::sync::{Notify, watch};

/// Path prefix of the environment route that receives webhook requests.
pub const WEBHOOK_ROUTE_PREFIX: &str = "/api/hooks";
/// Deliveries kept per task; older ones are pruned on insert.
const WEBHOOK_DELIVERY_RETENTION: i64 = 50;
/// Body text kept in the delivery log. Larger bodies are cut and flagged.
const WEBHOOK_DELIVERY_LOG_BODY_LIMIT: usize = 64 * 1024;
/// Rendered prompt text kept in the delivery log.
const WEBHOOK_DELIVERY_LOG_PROMPT_LIMIT: usize = 64 * 1024;
/// Deliveries one task may hold at once, running or waiting their turn.
const WEBHOOK_MAX_QUEUED_PER_TASK: u32 = 20;
/// Accepted deliveries per task per minute; the tunnel hostname is public as well as the relay.
const WEBHOOK_RATE_LIMIT_PER_MINUTE: usize = 60;
/// The longest prompt a provider accepts for one turn.
const PROVIDER_SEND_TURN_MAX_INPUT_CHARS: usize = 120_000;
/// Run records kept per task.
const RUN_HISTORY_RETENTION: i64 = 100;
/// How often the original polled; a task still due after a pass is looked at again after this.
const REPOLL_MS: i64 = 5_000;

// ---- clock -----------------------------------------------------------------------------------------

/// The time source of the scheduler. Production reads the system clock; tests set a manual one, so a
/// due time is crossed by moving the clock and nothing waits for real time.
pub trait SchedulerClock: Send + Sync + 'static {
    fn now_ms(&self) -> i64;
    /// Resolves once the clock reads at least `deadline_ms`.
    fn sleep_until(&self, deadline_ms: i64) -> BoxFuture<'static, ()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;
impl SchedulerClock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn sleep_until(&self, deadline_ms: i64) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            // Monotonic timers stop while a machine sleeps, so wait in bounded steps against the wall clock.
            loop {
                let remaining = deadline_ms - chrono::Utc::now().timestamp_millis();
                if remaining <= 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(remaining.min(30_000) as u64)).await;
            }
        })
    }
}

/// A clock that only moves when told to.
pub struct ManualClock {
    now: watch::Sender<i64>,
}
impl ManualClock {
    pub fn new(now_ms: i64) -> Arc<Self> {
        Arc::new(Self { now: watch::channel(now_ms).0 })
    }
    pub fn set(&self, now_ms: i64) {
        self.now.send_replace(now_ms);
    }
    pub fn advance(&self, by_ms: i64) {
        self.now.send_modify(|now| *now += by_ms);
    }
}
impl SchedulerClock for ManualClock {
    fn now_ms(&self) -> i64 {
        *self.now.borrow()
    }
    fn sleep_until(&self, deadline_ms: i64) -> BoxFuture<'static, ()> {
        let mut now = self.now.subscribe();
        Box::pin(async move {
            let _ = now.wait_for(|now| *now >= deadline_ms).await;
        })
    }
}

// ---- errors ----------------------------------------------------------------------------------------

/// `ScheduledTaskError`: the only error the `scheduledTasks.*` methods return besides authorization.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledTaskError {
    pub message: String,
    pub task_id: Option<String>,
    /// The wire form of the cause, as the original encodes a defect: `{name, message}`.
    pub cause: Option<Value>,
}
impl ScheduledTaskError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), task_id: None, cause: None }
    }
    fn for_task(message: impl Into<String>, task_id: &str) -> Self {
        Self { message: message.into(), task_id: Some(task_id.to_owned()), cause: None }
    }
    fn caused_by(mut self, cause: &dyn std::fmt::Display) -> Self {
        self.cause = Some(json!({"name":"Error","message":cause.to_string()}));
        self
    }
    fn caused_by_task_error(mut self, cause: &ScheduledTaskError) -> Self {
        self.cause = Some(json!({"name":"ScheduledTaskError","message":cause.message}));
        self
    }
    pub fn wire(&self) -> Value {
        let mut wire = json!({"_tag":"ScheduledTaskError","message":self.message});
        if let Some(id) = &self.task_id {
            wire["taskId"] = json!(id);
        }
        if let Some(cause) = &self.cause {
            wire["cause"] = cause.clone();
        }
        wire
    }
}
impl std::fmt::Display for ScheduledTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ScheduledTaskError {}

fn storage(message: &str, task_id: Option<&str>, cause: impl std::fmt::Display) -> ScheduledTaskError {
    let error = match task_id {
        Some(id) => ScheduledTaskError::for_task(message, id),
        None => ScheduledTaskError::new(message),
    };
    error.caused_by(&cause)
}

// ---- storage ---------------------------------------------------------------------------------------

const COLUMNS: &str = "task_id, title, prompt, enabled, schedule_json, project_id, thread_id, workspace_strategy_json, model_selection_json, runtime_mode, interaction_mode, created_by, creation_source, created_at, updated_at, next_run_at, last_run_at, last_run_status, last_run_error, run_count, webhook_token, webhook_secret";

/// New tables only: an existing database opens unchanged and gains them on first use.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rust_scheduled_tasks (
  task_id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, prompt TEXT NOT NULL, enabled INTEGER NOT NULL,
  schedule_json TEXT NOT NULL, project_id TEXT NOT NULL, thread_id TEXT, workspace_strategy_json TEXT NOT NULL,
  model_selection_json TEXT NOT NULL, runtime_mode TEXT NOT NULL, interaction_mode TEXT NOT NULL,
  created_by TEXT NOT NULL, creation_source TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  next_run_at TEXT, last_run_at TEXT, last_run_status TEXT NOT NULL, last_run_error TEXT, run_count INTEGER NOT NULL,
  webhook_token TEXT, webhook_secret TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_tasks_due ON rust_scheduled_tasks(enabled, next_run_at) WHERE enabled = 1 AND next_run_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS rust_scheduled_tasks_project ON rust_scheduled_tasks(project_id, updated_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_runs (
  run_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, trigger TEXT NOT NULL, started_at TEXT NOT NULL,
  completed_at TEXT, status TEXT NOT NULL, command_id TEXT NOT NULL, message_id TEXT NOT NULL,
  thread_id TEXT, delivery_id TEXT, error TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_runs_task ON rust_scheduled_task_runs(task_id, started_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_webhook_deliveries (
  delivery_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, received_at TEXT NOT NULL, method TEXT NOT NULL,
  query TEXT NOT NULL, headers_json TEXT NOT NULL, body TEXT NOT NULL, body_bytes INTEGER NOT NULL,
  body_truncated INTEGER NOT NULL, outcome TEXT NOT NULL, signature_verified INTEGER NOT NULL,
  missing_fields_json TEXT NOT NULL, rendered_prompt TEXT, error TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_webhook_deliveries_task ON rust_scheduled_task_webhook_deliveries(task_id, received_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_webhook_relay_deliveries (
  relay_delivery_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, seen_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_webhook_relay_deliveries_seen ON rust_scheduled_task_webhook_relay_deliveries(seen_at);";

#[derive(Debug, Clone)]
struct Row {
    task_id: String,
    title: String,
    prompt: String,
    enabled: i64,
    schedule_json: String,
    project_id: String,
    thread_id: Option<String>,
    workspace_strategy_json: String,
    model_selection_json: String,
    runtime_mode: String,
    interaction_mode: String,
    created_by: String,
    creation_source: String,
    created_at: String,
    updated_at: String,
    next_run_at: Option<String>,
    last_run_at: Option<String>,
    last_run_status: String,
    last_run_error: Option<String>,
    run_count: i64,
    webhook_token: Option<String>,
    webhook_secret: Option<String>,
}
impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            task_id: row.get(0)?,
            title: row.get(1)?,
            prompt: row.get(2)?,
            enabled: row.get(3)?,
            schedule_json: row.get(4)?,
            project_id: row.get(5)?,
            thread_id: row.get(6)?,
            workspace_strategy_json: row.get(7)?,
            model_selection_json: row.get(8)?,
            runtime_mode: row.get(9)?,
            interaction_mode: row.get(10)?,
            created_by: row.get(11)?,
            creation_source: row.get(12)?,
            created_at: row.get(13)?,
            updated_at: row.get(14)?,
            next_run_at: row.get(15)?,
            last_run_at: row.get(16)?,
            last_run_status: row.get(17)?,
            last_run_error: row.get(18)?,
            run_count: row.get(19)?,
            webhook_token: row.get(20)?,
            webhook_secret: row.get(21)?,
        })
    }
}

/// `encodeURIComponent`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn webhook_endpoint(row: &Row, relay_base: Option<&str>) -> Option<Value> {
    let token = row.webhook_token.as_deref()?;
    let id = encode_uri_component(&row.task_id);
    Some(json!({
        "path": format!("{WEBHOOK_ROUTE_PREFIX}/{id}/{token}"),
        "url": relay_base.map(|base| format!("{base}/{id}/{token}")),
        "hasSecret": row.webhook_secret.is_some(),
    }))
}

fn decode_row(row: &Row, relay_base: Option<&str>) -> Result<ScheduledTask, ScheduledTaskError> {
    let fail = |cause: &dyn std::fmt::Display| {
        // The diagnostic can only carry an id that itself decodes.
        let error = match ScheduledTaskId::new(&row.task_id) {
            Ok(id) => ScheduledTaskError::for_task("Could not decode schedule task row.", id.as_str()),
            Err(_) => ScheduledTaskError::new("Could not decode schedule task row."),
        };
        error.caused_by(cause)
    };
    let parse = |text: &str| serde_json::from_str::<Value>(text).map_err(|error| fail(&error));
    let schedule = parse(&row.schedule_json)?;
    let webhook = if schedule["type"] == "webhook" { webhook_endpoint(row, relay_base) } else { None };
    let mut task = json!({
        "id": row.task_id, "title": row.title, "prompt": row.prompt, "enabled": row.enabled == 1, "schedule": schedule,
        "projectId": row.project_id, "threadId": row.thread_id, "workspaceStrategy": parse(&row.workspace_strategy_json)?,
        "modelSelection": parse(&row.model_selection_json)?, "runtimeMode": row.runtime_mode, "interactionMode": row.interaction_mode,
        "createdBy": row.created_by, "creationSource": row.creation_source, "createdAt": row.created_at, "updatedAt": row.updated_at,
        "nextRunAt": row.next_run_at, "lastRunAt": row.last_run_at, "lastRunStatus": row.last_run_status,
        "lastRunError": row.last_run_error, "runCount": row.run_count,
    });
    if let Some(webhook) = webhook {
        task["webhook"] = webhook;
    }
    serde_json::from_value(task).map_err(|error| fail(&error))
}

fn select_rows(connection: &rusqlite::Connection, tail: &str, parameters: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<Row>> {
    let mut statement = connection.prepare(&format!("SELECT {COLUMNS} FROM rust_scheduled_tasks {tail}"))?;
    statement.query_map(parameters, Row::read)?.collect()
}

fn row_for(connection: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Row>> {
    Ok(select_rows(connection, "WHERE task_id = ?1", &[&id])?.into_iter().next())
}

/// A transaction whose body reports typed errors: the transaction rolls back and the error comes out.
fn transact<R>(store: &Store, body: impl FnOnce(&Transaction<'_>) -> Result<R, ScheduledTaskError>) -> Result<R, ScheduledTaskError> {
    let mut captured = None;
    let outcome = store.transaction(|transaction| {
        body(transaction).map_err(|error| {
            captured = Some(error);
            StoreError::InvalidCommand("scheduled task transaction rolled back".into())
        })
    });
    outcome.map_err(|error| captured.unwrap_or_else(|| storage("Could not access scheduled task storage.", None, error)))
}

fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("contract values serialize")
}
/// The wire name of a string enum, as stored in a text column.
fn enum_text<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value).expect("contract values serialize") {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

fn next_run_iso(enabled: bool, schedule: &ScheduledTaskSchedule, from_ms: i64, zone: &dyn Zone) -> Option<String> {
    if !enabled {
        return None;
    }
    next_scheduled_run_at(schedule, from_ms, zone).and_then(iso_from_ms)
}

fn iso(ms: i64) -> String {
    iso_from_ms(ms).expect("the clock reads a representable time")
}

/// The persisted record of one run, for history and for restart recovery.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run_id: String,
    pub task_id: String,
    /// `scheduled`, `manual` or `webhook`.
    pub trigger: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    /// `running`, `succeeded` or `failed`.
    pub status: String,
    pub command_id: String,
    pub message_id: String,
    /// The thread the run created, or the thread a bound task posted into.
    pub thread_id: Option<String>,
    pub delivery_id: Option<String>,
    pub error: Option<String>,
}
impl RunRecord {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            run_id: row.get(0)?,
            task_id: row.get(1)?,
            trigger: row.get(2)?,
            started_at: row.get(3)?,
            completed_at: row.get(4)?,
            status: row.get(5)?,
            command_id: row.get(6)?,
            message_id: row.get(7)?,
            thread_id: row.get(8)?,
            delivery_id: row.get(9)?,
            error: row.get(10)?,
        })
    }
}
const RUN_COLUMNS: &str = "run_id, task_id, trigger, started_at, completed_at, status, command_id, message_id, thread_id, delivery_id, error";

// ---- launching -------------------------------------------------------------------------------------

/// What a run asks the orchestration to do, built from the task as it is when the run starts.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub task: ScheduledTask,
    pub command_id: String,
    pub message_id: String,
    /// The task's prompt, or the prompt a webhook request filled in.
    pub prompt: String,
}
/// Starts the agent turn for a run. Launching twice under one `command_id` must be a no-op.
pub trait RunLauncher: Send + Sync + 'static {
    /// The thread the turn runs in.
    fn launch(&self, request: LaunchRequest) -> BoxFuture<'static, Result<String, String>>;
}

/// The real launcher: a task with no thread launches a new thread through `ThreadLaunchService`; a
/// task bound to a thread queues its prompt behind any turn that thread is running.
pub struct ExecutionLauncher {
    execution: ExecutionService,
}
impl ExecutionLauncher {
    pub fn new(execution: ExecutionService) -> Arc<Self> {
        Arc::new(Self { execution })
    }
}
impl RunLauncher for ExecutionLauncher {
    fn launch(&self, request: LaunchRequest) -> BoxFuture<'static, Result<String, String>> {
        let execution = self.execution.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let task = &request.task;
                let now = chrono::Utc::now();
                match &task.thread_id {
                    None => {
                        let input = json!({
                            "commandId": request.command_id, "projectId": task.project_id, "title": task.title,
                            "modelSelection": task.model_selection, "runtimeMode": task.runtime_mode,
                            "interactionMode": task.interaction_mode, "workspaceStrategy": task.workspace_strategy,
                            "creationSource": task.creation_source,
                            "initialMessage": {"messageId": request.message_id, "text": request.prompt, "attachments": []},
                        });
                        // The thread and its first message are the task creator's, and the message names the task.
                        let provenance = crate::launch::LaunchProvenance {
                            created_by: serde_json::to_value(task.created_by).ok().and_then(|actor| actor.as_str().map(str::to_owned)),
                            scheduled_task_id: Some(task.id.to_string()),
                            sender_thread_id: None,
                        };
                        let launched = crate::launch::ThreadLaunchService::with_providers(execution.store.clone(), execution.providers.clone())
                            .launch_as(input, provenance, now)
                            .map_err(|error| error.to_string())?;
                        launched["threadId"].as_str().map(str::to_owned).ok_or_else(|| "The launch did not return a thread.".to_owned())
                    }
                    Some(thread_id) => {
                        let projection = execution.store.projection("thread", thread_id.as_str()).map_err(|error| error.to_string())?;
                        let belongs = projection.as_ref().is_some_and(|projection| {
                            projection["thread"]["projectId"] == task.project_id.as_str() && projection["thread"]["deletedAt"].is_null()
                        });
                        if !belongs {
                            return Err(format!("Thread {} was not found in project {}.", thread_id, task.project_id));
                        }
                        let receipt = execution
                            .dispatch(
                                &json!({
                                    "type": "message.dispatch", "commandId": request.command_id, "threadId": thread_id,
                                    "messageId": request.message_id, "text": request.prompt, "attachments": [],
                                    "modelSelection": task.model_selection, "dispatchMode": {"type": "queue_after_active"},
                                    "createdBy": task.created_by, "creationSource": task.creation_source,
                                    "scheduledTaskId": task.id,
                                }),
                                now,
                            )
                            .map_err(|error| error.to_string())?;
                        if receipt.status == "rejected" {
                            return Err(format!("The message was rejected: {}", receipt.error.unwrap_or(Value::Null)));
                        }
                        Ok(thread_id.to_string())
                    }
                }
            })
            .await
            .map_err(|error| error.to_string())?
        })
    }
}

// ---- service ---------------------------------------------------------------------------------------

/// What `triggerWebhook` tells the HTTP route to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookTriggerResult {
    Accepted { delivery_id: String, outcome: &'static str },
    NotFound,
    RejectedSignature,
    Disabled,
    RateLimited { outcome: &'static str },
    Expired,
}

pub struct WebhookTriggerRequest {
    pub hook_id: String,
    pub token: String,
    pub request: WebhookRequest,
    pub body: Vec<u8>,
    /// Set by T3 Connect; the same id is never dispatched twice.
    pub relay_delivery_id: Option<String>,
    /// When the relay received a held request; defaults to now.
    pub received_at: Option<String>,
}

#[derive(Default)]
struct RateWindow {
    accepted: Vec<i64>,
    rejected_logged: bool,
}

struct Inner {
    store: Store,
    clock: Arc<dyn SchedulerClock>,
    zone: Arc<dyn Zone>,
    launcher: Mutex<Option<Arc<dyn RunLauncher>>>,
    relay_hook_base_url: Mutex<Option<String>>,
    /// Counts every change the list subscription shows; the timer is re-armed by `wake`.
    changes: watch::Sender<u64>,
    wake: Notify,
    stop: watch::Sender<bool>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Tasks whose launch is in flight.
    active: Mutex<HashSet<String>>,
    rate_windows: Mutex<HashMap<String, RateWindow>>,
    queued_deliveries: Mutex<HashMap<String, u32>>,
    /// Deliveries accepted and not yet finished, across tasks.
    pending_deliveries: watch::Sender<usize>,
    delivery_permits: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

#[derive(Clone)]
pub struct ScheduledTasks {
    inner: Arc<Inner>,
}

type Registry = Mutex<HashMap<usize, Weak<Inner>>>;
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

struct ActiveGuard {
    inner: Arc<Inner>,
    id: String,
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.inner.active.lock().unwrap().remove(&self.id);
    }
}

enum RunOutcome {
    Done(Box<ScheduledTask>),
    /// A webhook delivery that no longer applies to its task; the reason is shown in the delivery log.
    Skipped(String),
}
enum Trigger {
    Scheduled,
    Manual,
    Webhook { delivery_id: String, prompt: String },
}
impl Trigger {
    fn name(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::Webhook { .. } => "webhook",
        }
    }
}

impl ScheduledTasks {
    /// The service of this store, as it is running in this process: the one `open` made, or a default
    /// one (system clock and zone, no timer, no launcher) created on first use so the methods work on
    /// any store.
    pub fn of(store: &Store) -> Result<Self, ScheduledTaskError> {
        let existing = registry().lock().unwrap().get(&store.identity()).and_then(Weak::upgrade);
        match existing {
            Some(inner) => Ok(Self { inner }),
            None => Self::open(store.clone(), Arc::new(SystemClock), Arc::new(LocalZone)),
        }
    }

    /// Creates the service for this store, replacing any earlier one. Opening recovers runs that an
    /// earlier process left `running`, so call it before the timer starts and before launching anything.
    pub fn open(store: Store, clock: Arc<dyn SchedulerClock>, zone: Arc<dyn Zone>) -> Result<Self, ScheduledTaskError> {
        store
            .transaction(|transaction| Ok(transaction.execute_batch(SCHEMA)?))
            .map_err(|error| storage("Could not prepare scheduled task storage.", None, error))?;
        let inner = Arc::new(Inner {
            store: store.clone(),
            clock,
            zone,
            launcher: Mutex::new(None),
            relay_hook_base_url: Mutex::new(None),
            changes: watch::channel(0).0,
            wake: Notify::new(),
            stop: watch::channel(false).0,
            timer: Mutex::new(None),
            active: Mutex::new(HashSet::new()),
            rate_windows: Mutex::new(HashMap::new()),
            queued_deliveries: Mutex::new(HashMap::new()),
            pending_deliveries: watch::channel(0).0,
            delivery_permits: Mutex::new(HashMap::new()),
        });
        let service = Self { inner };
        service.recover_interrupted_runs()?;
        registry().lock().unwrap().insert(store.identity(), Arc::downgrade(&service.inner));
        Ok(service)
    }

    /// The server's own wiring: this store's service on the system clock and the OS time zone, launching
    /// through `execution`, with the timer running.
    pub fn start(store: &Store, execution: ExecutionService) -> Result<Self, ScheduledTaskError> {
        let service = Self::open(store.clone(), Arc::new(SystemClock), Arc::new(LocalZone))?;
        service.set_launcher(ExecutionLauncher::new(execution));
        service.spawn_timer();
        Ok(service)
    }

    pub fn set_launcher(&self, launcher: Arc<dyn RunLauncher>) {
        *self.inner.launcher.lock().unwrap() = Some(launcher);
    }
    /// Uses `execution` to launch runs unless a launcher is already set (a request handler's hook).
    pub fn ensure_launcher(&self, execution: Option<&ExecutionService>) {
        if let Some(execution) = execution {
            let mut launcher = self.inner.launcher.lock().unwrap();
            if launcher.is_none() {
                *launcher = Some(ExecutionLauncher::new(execution.clone()));
            }
        }
    }
    /// Where a webhook task's public URL points: `{base}/{taskId}/{token}`. None without T3 Connect.
    pub fn set_relay_hook_base_url(&self, base: Option<String>) {
        *self.inner.relay_hook_base_url.lock().unwrap() = base;
        self.notify_changed();
    }

    /// Starts the single timer task: it runs what is due, then sleeps until the next due time on the
    /// injected clock, and looks again early when a task changes.
    pub fn spawn_timer(&self) {
        let mut timer = self.inner.timer.lock().unwrap();
        if timer.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return;
        }
        self.inner.stop.send_replace(false);
        let service = self.clone();
        *timer = Some(tokio::spawn(async move { service.timer().await }));
    }
    /// Stops the timer after the pass in progress. A launch already made is not undone.
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        let handle = self.inner.timer.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    async fn timer(&self) {
        let mut stop = self.inner.stop.subscribe();
        while !*stop.borrow() {
            // Registered before the pass so a change made during it still wakes the next wait.
            let woken = self.inner.wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            // What the pass saw is what the wait is measured from: the clock may move during the pass, and a
            // task that came due meanwhile must be found by the next pass at once, not after a poll interval.
            let pass_started = self.inner.clock.now_ms();
            if let Err(error) = self.run_due_tasks().await {
                tracing::warn!(message = %error.message, "Scheduled task pass failed");
            }
            let deadline = match self.db(|store| next_wake(store)).await {
                // Still due after the pass means it could not run; look again as the original's poll did.
                Ok(Some(due)) if due > pass_started => Some(due),
                Ok(Some(_)) => Some(pass_started + REPOLL_MS),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(message = %error.message, "Could not read the next scheduled task time");
                    Some(pass_started + REPOLL_MS)
                }
            };
            let sleeping = async {
                match deadline {
                    Some(deadline) => self.inner.clock.sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = sleeping => {}
                _ = &mut woken => {}
                _ = stop.changed() => {}
            }
        }
    }

    fn notify_changed(&self) {
        self.inner.changes.send_modify(|revision| *revision += 1);
        self.inner.wake.notify_one();
    }

    async fn db<R: Send + 'static>(&self, body: impl FnOnce(&Store) -> Result<R, ScheduledTaskError> + Send + 'static) -> Result<R, ScheduledTaskError> {
        let store = self.inner.store.clone();
        tokio::task::spawn_blocking(move || body(&store))
            .await
            .map_err(|error| ScheduledTaskError::new("Scheduled task storage stopped.").caused_by(&error))?
    }

    fn relay_base(&self) -> Option<String> {
        self.inner.relay_hook_base_url.lock().unwrap().clone()
    }

    // ---- reads ---------------------------------------------------------------------------------

    /// Strict decode for the API surface: a corrupt row is a visible error.
    pub async fn list(&self) -> Result<ScheduledTaskListResult, ScheduledTaskError> {
        let base = self.relay_base();
        let tasks = self
            .db(move |store| {
                let rows = store
                    .read(|connection| select_rows(connection, "ORDER BY updated_at DESC, task_id ASC", &[]).map_err(StoreError::from))
                    .map_err(|error| storage("Could not read schedule tasks.", None, error))?;
                rows.iter().map(|row| decode_row(row, base.as_deref())).collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(|cause| ScheduledTaskError::new("Could not list schedule tasks.").caused_by_task_error(&cause))?;
        Ok(ScheduledTaskListResult { tasks: t3_contracts::ForwardCompatibleArray(tasks) })
    }

    pub fn subscribe(&self) -> TaskListSubscription {
        // Subscribed before the first snapshot, so a change landing between the two is not lost.
        TaskListSubscription { service: self.clone(), changes: self.inner.changes.subscribe(), first: true }
    }

    async fn find_task(&self, id: &str) -> Result<Option<ScheduledTask>, ScheduledTaskError> {
        let (id, base) = (id.to_owned(), self.relay_base());
        self.db(move |store| find_task(store, &id, base.as_deref())).await
    }
    async fn load_task(&self, id: &str) -> Result<ScheduledTask, ScheduledTaskError> {
        self.find_task(id).await?.ok_or_else(|| ScheduledTaskError::for_task("Schedule task not found.", id))
    }

    /// The runs recorded for a task, newest first, up to the retention limit.
    pub async fn runs(&self, task_id: &str) -> Result<Vec<RunRecord>, ScheduledTaskError> {
        let id = task_id.to_owned();
        self.db(move |store| {
            store
                .read(|connection| {
                    let mut statement = connection.prepare(&format!("SELECT {RUN_COLUMNS} FROM rust_scheduled_task_runs WHERE task_id = ?1 ORDER BY started_at DESC, rowid DESC"))?;
                    Ok(statement.query_map([&id], RunRecord::read)?.collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .map_err(|error| storage("Could not load schedule task runs.", Some(&id), error))
        })
        .await
    }

    // ---- writes --------------------------------------------------------------------------------

    pub async fn upsert(&self, input: ScheduledTaskUpsertInput) -> Result<t3_contracts::ScheduledTaskMutationResult, ScheduledTaskError> {
        let now = self.inner.clock.now_ms();
        let zone = self.inner.zone.clone();
        let (base, uuid) = (self.relay_base(), uuid::Uuid::new_v4().to_string());
        let token = new_webhook_token();
        let task = self.db(move |store| upsert_task(store, input, now, zone.as_ref(), base.as_deref(), &uuid, token)).await?;
        self.notify_changed();
        Ok(t3_contracts::ScheduledTaskMutationResult { task })
    }

    /// Partial update flipping only the enabled flag; never touches other fields.
    pub async fn set_enabled(&self, input: ScheduledTaskSetEnabledInput) -> Result<t3_contracts::ScheduledTaskMutationResult, ScheduledTaskError> {
        let existing = self.load_task(input.id.as_str()).await?;
        if existing.enabled == input.enabled {
            return Ok(t3_contracts::ScheduledTaskMutationResult { task: existing });
        }
        let now = self.inner.clock.now_ms();
        let next = next_run_iso(input.enabled, &existing.schedule, now, self.inner.zone.as_ref());
        let (id, enabled, next_text, now_text) = (input.id.to_string(), input.enabled, next.clone(), iso(now));
        self.db(move |store| {
            let updated = store
                .transaction(|transaction| {
                    Ok(transaction.execute(
                        "UPDATE rust_scheduled_tasks SET enabled = ?1, next_run_at = ?2, updated_at = ?3 WHERE task_id = ?4",
                        params![i64::from(enabled), next_text, now_text, id],
                    )?)
                })
                .map_err(|error| storage("Could not update schedule task.", Some(&id), error))?;
            // A task deleted between the load and this update is a visible not-found error.
            if updated == 0 { Err(ScheduledTaskError::for_task("Schedule task not found.", &id)) } else { Ok(()) }
        })
        .await?;
        self.notify_changed();
        let mut task = existing;
        task.enabled = input.enabled;
        task.next_run_at = next;
        task.updated_at = iso(now);
        Ok(t3_contracts::ScheduledTaskMutationResult { task })
    }

    pub async fn delete(&self, input: ScheduledTaskDeleteInput) -> Result<ScheduledTaskDeleteResult, ScheduledTaskError> {
        let id = input.id.to_string();
        let owned = id.clone();
        self.db(move |store| {
            store
                .transaction(|transaction| {
                    transaction.execute("DELETE FROM rust_scheduled_task_webhook_deliveries WHERE task_id = ?1", [&owned])?;
                    transaction.execute("DELETE FROM rust_scheduled_task_runs WHERE task_id = ?1", [&owned])?;
                    transaction.execute("DELETE FROM rust_scheduled_tasks WHERE task_id = ?1", [&owned])?;
                    Ok(())
                })
                .map_err(|error| storage("Could not delete schedule task.", Some(&owned), error))
        })
        .await?;
        self.inner.rate_windows.lock().unwrap().remove(&id);
        self.inner.delivery_permits.lock().unwrap().remove(&id);
        self.notify_changed();
        Ok(ScheduledTaskDeleteResult { id: input.id })
    }

    pub async fn run_now(&self, input: ScheduledTaskRunNowInput) -> Result<t3_contracts::ScheduledTaskRunNowResult, ScheduledTaskError> {
        let id = input.id.as_str();
        let task = self.load_task(id).await?;
        if matches!(task.schedule, ScheduledTaskSchedule::Webhook(_)) {
            // There is no request to render the prompt from.
            return Err(ScheduledTaskError::for_task("Webhook tasks run when their URL receives a request.", id));
        }
        match self.run_task(&task, Trigger::Manual).await {
            Ok(RunOutcome::Done(task)) => Ok(t3_contracts::ScheduledTaskMutationResult { task: *task }),
            Ok(RunOutcome::Skipped(reason)) => Err(ScheduledTaskError::for_task("Could not run schedule task.", id).caused_by(&reason)),
            Err(cause) => Err(ScheduledTaskError::for_task("Could not run schedule task.", id).caused_by_task_error(&cause)),
        }
    }

    /// Issues a new URL token for a webhook task; the old URL stops working at once.
    pub async fn rotate_webhook_token(&self, input: ScheduledTaskRotateWebhookTokenInput) -> Result<t3_contracts::ScheduledTaskMutationResult, ScheduledTaskError> {
        let id = input.id.as_str();
        let task = self.load_task(id).await?;
        if !matches!(task.schedule, ScheduledTaskSchedule::Webhook(_)) {
            return Err(ScheduledTaskError::for_task("Only webhook tasks have a URL token.", id));
        }
        let (token, now, created_at, owned) = (new_webhook_token(), iso(self.inner.clock.now_ms()), task.created_at.clone(), id.to_owned());
        self.db(move |store| {
            // Matching created_at keeps a rotation from landing on a task deleted and recreated since it was loaded.
            let updated = store
                .transaction(|transaction| {
                    Ok(transaction.execute(
                        "UPDATE rust_scheduled_tasks SET webhook_token = ?1, updated_at = ?2 WHERE task_id = ?3 AND created_at = ?4",
                        params![token, now, owned, created_at],
                    )?)
                })
                .map_err(|error| storage("Could not rotate webhook token.", Some(&owned), error))?;
            if updated == 0 { Err(ScheduledTaskError::for_task("Schedule task was deleted or replaced.", &owned)) } else { Ok(()) }
        })
        .await?;
        self.notify_changed();
        Ok(t3_contracts::ScheduledTaskMutationResult { task: self.load_task(id).await? })
    }

    pub async fn list_webhook_deliveries(&self, input: ScheduledTaskListWebhookDeliveriesInput) -> Result<ScheduledTaskListWebhookDeliveriesResult, ScheduledTaskError> {
        let id = input.id.to_string();
        let rows = self
            .db(move |store| {
                store
                    .read(|connection| {
                        let mut statement = connection.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM rust_scheduled_task_webhook_deliveries WHERE task_id = ?1 ORDER BY received_at DESC, rowid DESC"))?;
                        Ok(statement.query_map([&id], DeliveryRow::read)?.collect::<rusqlite::Result<Vec<_>>>()?)
                    })
                    .map_err(|error| storage("Could not list webhook deliveries.", Some(&id), error))
            })
            .await?;
        let deliveries = rows.iter().map(|row| serde_json::from_value(row.summary_value())).collect::<Result<Vec<_>, _>>();
        Ok(ScheduledTaskListWebhookDeliveriesResult {
            deliveries: deliveries.map_err(|error| storage("Could not list webhook deliveries.", Some(input.id.as_str()), error))?,
        })
    }

    pub async fn get_webhook_delivery(&self, input: ScheduledTaskGetWebhookDeliveryInput) -> Result<ScheduledTaskGetWebhookDeliveryResult, ScheduledTaskError> {
        let (id, delivery_id) = (input.id.to_string(), input.delivery_id.to_string());
        let owned = id.clone();
        let row = self
            .db(move |store| {
                store
                    .read(|connection| {
                        let mut statement = connection.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM rust_scheduled_task_webhook_deliveries WHERE task_id = ?1 AND delivery_id = ?2"))?;
                        Ok(statement.query_map([&owned, &delivery_id], DeliveryRow::read)?.collect::<rusqlite::Result<Vec<_>>>()?)
                    })
                    .map_err(|error| storage("Could not load webhook delivery.", Some(&owned), error))
            })
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ScheduledTaskError::for_task("Webhook delivery not found.", &id))?;
        let delivery = serde_json::from_value(row.detail_value()).map_err(|error| storage("Could not load webhook delivery.", Some(&id), error))?;
        Ok(ScheduledTaskGetWebhookDeliveryResult { delivery })
    }

    // ---- runs ----------------------------------------------------------------------------------

    /// Claims the run durably, launches it, and records the outcome. A launch that fails is a failed
    /// run, not an error; an error is returned only when the run could not be recorded or started.
    async fn run_task(&self, task: &ScheduledTask, trigger: Trigger) -> Result<RunOutcome, ScheduledTaskError> {
        let id = task.id.as_str().to_owned();
        let reserved = self.inner.active.lock().unwrap().insert(id.clone());
        if !reserved {
            if matches!(trigger, Trigger::Scheduled) {
                return Ok(RunOutcome::Done(Box::new(task.clone())));
            }
            return Err(ScheduledTaskError::for_task("Schedule task is already running.", &id));
        }
        let _guard = ActiveGuard { inner: self.inner.clone(), id: id.clone() };

        let started_ms = self.inner.clock.now_ms();
        let started = iso(started_ms);
        // The in-memory snapshot may be stale: re-read before touching run state. The task may have been
        // deleted, paused or postponed since the poll loaded it, and none of those may fire.
        let Some(active) = self.find_task(&id).await? else {
            if matches!(trigger, Trigger::Webhook { .. }) {
                return Ok(RunOutcome::Skipped("The task was deleted before this delivery ran.".into()));
            }
            // A manual run on a just-deleted task fails loudly, not as a successful run that never dispatched.
            if !matches!(trigger, Trigger::Scheduled) {
                return Err(ScheduledTaskError::for_task("Schedule task not found.", &id));
            }
            return Ok(RunOutcome::Done(Box::new(task.clone())));
        };
        if matches!(trigger, Trigger::Scheduled) {
            // A next_run_at corrupted since the poll read is treated as not due.
            let due = active.next_run_at.as_deref().and_then(parse_iso).is_some_and(|due| due <= started_ms);
            if !active.enabled || !due {
                return Ok(RunOutcome::Done(Box::new(active)));
            }
        }
        if matches!(trigger, Trigger::Webhook { .. }) {
            // A queued delivery must not run a task that was paused, deleted and recreated under the same id,
            // or switched to another trigger while it waited for its turn.
            let reason = if active.created_at != task.created_at {
                Some("The task was replaced before this delivery ran.")
            } else if !matches!(active.schedule, ScheduledTaskSchedule::Webhook(_)) {
                Some("The task's trigger changed before this delivery ran.")
            } else if !active.enabled {
                Some("The task was paused before this delivery ran.")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Ok(RunOutcome::Skipped(reason.into()));
            }
        }

        // The claim: the task becomes `running` and its run record, with the ids the launch will use,
        // is written in the same transaction.
        let fire_key = match &trigger {
            Trigger::Webhook { delivery_id, .. } => format!("{id}:webhook:{delivery_id}"),
            other => format!("{id}:{started_ms}:{}", other.name()),
        };
        let delivery_id = match &trigger {
            Trigger::Webhook { delivery_id, .. } => Some(delivery_id.clone()),
            _ => None,
        };
        let claim = {
            let (id, started, trigger_name) = (id.clone(), started.clone(), trigger.name());
            self.db(move |store| claim_run(store, &id, &started, trigger_name, &fire_key, delivery_id.as_deref())).await?
        };
        self.notify_changed();

        let result = self.launch_and_record(&active, &trigger, &claim, started_ms, &started).await;
        if let Err(error) = &result {
            self.release_stuck_run(task, &error.message).await;
        }
        result.map(|task| RunOutcome::Done(Box::new(task)))
    }

    async fn launch_and_record(&self, active: &ScheduledTask, trigger: &Trigger, claim: &Claim, started_ms: i64, started: &str) -> Result<ScheduledTask, ScheduledTaskError> {
        let prompt = match trigger {
            Trigger::Webhook { prompt, .. } => prompt.clone(),
            _ => active.prompt.as_str().to_owned(),
        };
        let launcher = self.inner.launcher.lock().unwrap().clone();
        let outcome = match launcher {
            Some(launcher) => {
                launcher
                    .launch(LaunchRequest { task: active.clone(), command_id: claim.command_id.clone(), message_id: claim.message_id.clone(), prompt })
                    .await
            }
            None => Err("Native provider execution is not configured.".to_owned()),
        };
        let completed_ms = self.inner.clock.now_ms();
        let (status, error, thread_id) = match outcome {
            Ok(thread_id) => (t3_contracts::ScheduledTaskRunStatus::Succeeded, None, Some(thread_id)),
            Err(message) => (t3_contracts::ScheduledTaskRunStatus::Failed, Some(message), None),
        };
        // Re-read so the next run is computed from the schedule as it is now: the user may have edited or
        // deleted the task while the launch ran.
        let current = self.find_task(active.id.as_str()).await?;
        let source = current.clone().unwrap_or_else(|| active.clone());
        let next = next_run_iso(source.enabled, &source.schedule, completed_ms, self.inner.zone.as_ref());
        let mut completed = source;
        completed.updated_at = iso(completed_ms);
        completed.last_run_at = Some(started.to_owned());
        completed.next_run_at = next;
        completed.last_run_status = status;
        completed.last_run_error = error.clone();
        completed.run_count = t3_contracts::NonNegativeInt(completed.run_count.0 + 1);
        let record = (claim.run_id.clone(), thread_id, error, completed.updated_at.clone(), completed.next_run_at.clone());
        let (id, started_text, task_exists) = (active.id.as_str().to_owned(), started.to_owned(), current.is_some());
        let _ = started_ms;
        self.db(move |store| complete_run(store, &id, &started_text, task_exists, status, record)).await?;
        self.notify_changed();
        Ok(completed)
    }

    /// If anything fails between claim and completion, writes a terminal record so the poll neither skips
    /// the task forever (it filters out `running` rows) nor fires it again at once: the launch may already
    /// have gone out, so the next run advances and the attempt counts.
    async fn release_stuck_run(&self, task: &ScheduledTask, message: &str) {
        let now = self.inner.clock.now_ms();
        let (id, message, zone, base) = (task.id.as_str().to_owned(), message.to_owned(), self.inner.zone.clone(), self.relay_base());
        let snapshot = task.clone();
        let released = self
            .db(move |store| {
                // The current row's schedule wins; the run's snapshot is the fallback if the re-read fails.
                let reread = find_task(store, &id, base.as_deref());
                let source = match &reread {
                    Ok(None) => None,
                    Ok(Some(current)) => Some(current.clone()),
                    Err(_) => Some(snapshot),
                };
                let Some(source) = source else { return Ok(()) };
                let next = next_run_iso(source.enabled, &source.schedule, now, zone.as_ref());
                store
                    .transaction(|transaction| {
                        transaction.execute(
                            "UPDATE rust_scheduled_tasks SET last_run_status = 'failed', last_run_error = ?1, next_run_at = ?2, updated_at = ?3, run_count = run_count + 1 WHERE task_id = ?4 AND last_run_status = 'running'",
                            params![message, next, iso(now), id],
                        )?;
                        transaction.execute(
                            "UPDATE rust_scheduled_task_runs SET status = 'failed', error = ?1, completed_at = ?2 WHERE task_id = ?3 AND status = 'running'",
                            params![message, iso(now), id],
                        )?;
                        Ok(())
                    })
                    .map_err(|error| storage("Could not release stuck schedule task run.", Some(&id), error))
            })
            .await;
        if let Err(error) = released {
            tracing::warn!(message = %error.message, "Could not release stuck schedule task run");
        }
        self.notify_changed();
    }

    /// Runs every enabled task whose time has come, one at a time in due order. A fixed-time run more
    /// than ten minutes late is moved to its next occurrence instead of firing stale work.
    pub async fn run_due_tasks(&self) -> Result<(), ScheduledTaskError> {
        let now = self.inner.clock.now_ms();
        let base = self.relay_base();
        let due = self.db(move |store| due_tasks(store, now, base.as_deref())).await?;
        for (due_ms, task) in due {
            let outcome = if is_missed_fixed_time_run(&task.schedule, due_ms, now) {
                self.reschedule_missed_run(&task, now).await.map(|()| ())
            } else {
                self.run_task(&task, Trigger::Scheduled).await.map(|_| ())
            };
            if let Err(error) = outcome {
                tracing::warn!(task_id = %task.id, message = %error.message, "Scheduled task run failed");
            }
        }
        Ok(())
    }

    async fn reschedule_missed_run(&self, task: &ScheduledTask, now: i64) -> Result<(), ScheduledTaskError> {
        let next = next_run_iso(task.enabled, &task.schedule, now, self.inner.zone.as_ref());
        tracing::info!(task_id = %task.id, missed = ?task.next_run_at, rescheduled_to = ?next, "Skipping missed schedule task run");
        let (id, now_text) = (task.id.as_str().to_owned(), iso(now));
        self.db(move |store| {
            store
                .transaction(|transaction| {
                    Ok(transaction.execute("UPDATE rust_scheduled_tasks SET next_run_at = ?1, updated_at = ?2 WHERE task_id = ?3", params![next, now_text, id])?)
                })
                .map(|_| ())
                .map_err(|error| storage("Could not reschedule missed schedule task run.", Some(&id), error))
        })
        .await?;
        self.notify_changed();
        Ok(())
    }

    /// Recovery from a crash or hard shutdown mid-run: rows stuck in `running` would otherwise be skipped
    /// forever. The launch may have gone out before the crash, so the receipt of the run's launch command
    /// decides: a recorded launch is a succeeded run against its thread, otherwise nothing was started and
    /// the run is failed. Either way the next run advances and the attempt counts, so the first poll after
    /// every restart does not fire the interrupted task again.
    fn recover_interrupted_runs(&self) -> Result<(), ScheduledTaskError> {
        let now = self.inner.clock.now_ms();
        let zone = self.inner.zone.clone();
        let store = &self.inner.store;
        let stuck = store
            .read(|connection| select_rows(connection, "WHERE last_run_status = 'running'", &[]).map_err(StoreError::from))
            .map_err(|error| storage("Could not reset interrupted schedule task runs.", None, error))?;
        for row in stuck {
            let runs = store
                .read(|connection| {
                    let mut statement = connection.prepare(&format!("SELECT {RUN_COLUMNS} FROM rust_scheduled_task_runs WHERE task_id = ?1 AND status = 'running' ORDER BY started_at DESC, rowid DESC"))?;
                    Ok(statement.query_map([&row.task_id], RunRecord::read)?.collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .map_err(|error| storage("Could not reset interrupted schedule task runs.", Some(&row.task_id), error))?;
            let launched = runs.iter().find_map(|run| {
                let receipt = store.receipt(&run.command_id).ok().flatten()?;
                (receipt.status == "accepted").then(|| (run.run_id.clone(), receipt.aggregate_id))
            });
            let (status, error, thread) = match &launched {
                Some((_, thread)) => ("succeeded", None, Some(thread.clone())),
                None => ("failed", Some("Run was interrupted by a server restart."), None),
            };
            let decoded = decode_row(&row, None);
            // An undecodable schedule cannot give a next occurrence, so the row is released without one.
            let next = match &decoded {
                Ok(task) => Some(next_run_iso(task.enabled, &task.schedule, now, zone.as_ref())),
                Err(error) => {
                    tracing::warn!(task_id = %row.task_id, message = %error.message, "Recovering undecodable schedule task row without rescheduling");
                    None
                }
            };
            let result = store.transaction(|transaction| {
                match next {
                    Some(next) => transaction.execute(
                        "UPDATE rust_scheduled_tasks SET last_run_status = ?1, last_run_error = ?2, next_run_at = ?3, updated_at = ?4, run_count = run_count + 1 WHERE task_id = ?5 AND last_run_status = 'running'",
                        params![status, error, next, iso(now), row.task_id],
                    )?,
                    None => transaction.execute(
                        "UPDATE rust_scheduled_tasks SET last_run_status = ?1, last_run_error = ?2, updated_at = ?3, run_count = run_count + 1 WHERE task_id = ?4 AND last_run_status = 'running'",
                        params![status, error, iso(now), row.task_id],
                    )?,
                };
                for run in &runs {
                    let this_launched = launched.as_ref().is_some_and(|(id, _)| *id == run.run_id);
                    transaction.execute(
                        "UPDATE rust_scheduled_task_runs SET status = ?1, error = ?2, thread_id = ?3, completed_at = ?4 WHERE run_id = ?5",
                        params![
                            if this_launched { "succeeded" } else { "failed" },
                            if this_launched { None } else { Some("Run was interrupted by a server restart.") },
                            if this_launched { thread.clone() } else { None },
                            iso(now),
                            run.run_id
                        ],
                    )?;
                }
                Ok(())
            });
            result.map_err(|error| storage("Could not reset interrupted schedule task runs.", Some(&row.task_id), error))?;
        }
        Ok(())
    }

    // ---- webhooks ------------------------------------------------------------------------------

    /// Verifies, logs and dispatches one webhook request. Returns as soon as the delivery is logged; the
    /// run itself continues in the background so senders with short timeouts get their answer at once.
    pub async fn trigger_webhook(&self, request: WebhookTriggerRequest) -> Result<WebhookTriggerResult, ScheduledTaskError> {
        let Ok(task_id) = ScheduledTaskId::new(&request.hook_id) else { return Ok(WebhookTriggerResult::NotFound) };
        let id = task_id.to_string();
        let owned = id.clone();
        let row = self
            .db(move |store| {
                store.read(|connection| row_for(connection, &owned).map_err(StoreError::from)).map_err(|error| storage("Could not load schedule task.", Some(&owned), error))
            })
            .await?;
        // A wrong token is indistinguishable from an unknown hook, so the URL does not reveal which hooks exist.
        let Some(row) = row.filter(|row| row.webhook_token.as_deref().is_some_and(|token| constant_time_equals(&request.token, token))) else {
            return Ok(WebhookTriggerResult::NotFound);
        };
        let task = decode_row(&row, self.relay_base().as_deref())?;
        let ScheduledTaskSchedule::Webhook(schedule) = &task.schedule else { return Ok(WebhookTriggerResult::NotFound) };

        let now = self.inner.clock.now_ms();
        // Anyone can reach the tunnel directly and set the relay's header, so a receive time is never later
        // than now: a future one would pin the delivery in the log and slip past the task's max age.
        let received = request.received_at.as_deref().and_then(parse_iso).map_or(now, |received| received.min(now));
        let delivery_id = match &request.relay_delivery_id {
            None => format!("delivery:{}", uuid::Uuid::new_v4()),
            Some(relay) => format!("delivery:relay:{relay}"),
        };
        let release_claim = |service: &Self| {
            let relay = request.relay_delivery_id.clone();
            let service = service.clone();
            async move {
                if let Some(relay) = relay {
                    let _ = service
                        .db(move |store| {
                            store
                                .transaction(|transaction| Ok(transaction.execute("DELETE FROM rust_scheduled_task_webhook_relay_deliveries WHERE relay_delivery_id = ?1", [&relay])?))
                                .map(|_| ())
                                .map_err(|error| storage("Could not release webhook delivery.", None, error))
                        })
                        .await;
                }
            }
        };
        if let Some(relay) = request.relay_delivery_id.clone() {
            let (task_text, seen, cutoff) = (id.clone(), iso(now), iso(now - 48 * 3_600_000));
            let claimed = self
                .db(move |store| {
                    store
                        .transaction(|transaction| {
                            let inserted = transaction.execute(
                                "INSERT INTO rust_scheduled_task_webhook_relay_deliveries (relay_delivery_id, task_id, seen_at) VALUES (?1, ?2, ?3) ON CONFLICT (relay_delivery_id) DO NOTHING",
                                params![relay, task_text, seen],
                            )?;
                            if inserted > 0 {
                                // Older claims can no longer be replayed by the relay.
                                transaction.execute("DELETE FROM rust_scheduled_task_webhook_relay_deliveries WHERE seen_at < ?1", [&cutoff])?;
                            }
                            Ok(inserted > 0)
                        })
                        .map_err(|error| storage("Could not record webhook delivery.", Some(&task_text), error))
                })
                .await?;
            if !claimed {
                // A held request this environment already ran: accepted, not run twice.
                return Ok(WebhookTriggerResult::Accepted { delivery_id, outcome: "duplicate" });
            }
        }
        let log = |outcome: ScheduledTaskWebhookDeliveryOutcome, signature_verified: bool, missing: Vec<String>, rendered: Option<String>, error: Option<String>| {
            let entry = DeliveryEntry {
                id: delivery_id.clone(),
                task_id: id.clone(),
                received_at: iso(received),
                request: &request,
                outcome,
                signature_verified,
                missing,
                rendered_prompt: rendered,
                error,
            };
            let record = entry.into_record();
            let service = self.clone();
            async move { service.db(move |store| record_delivery(store, record)).await }
        };

        // Only the first rejected request in a window is logged, so a flood cannot write rows or push the
        // real deliveries out of the log.
        let slot = self.take_rate_slot(&id, now);
        if slot != RateSlot::Allowed {
            if slot == RateSlot::FirstRejected {
                log(ScheduledTaskWebhookDeliveryOutcome::RateLimited, false, vec![], None, None).await?;
            }
            release_claim(self).await;
            return Ok(WebhookTriggerResult::RateLimited { outcome: "rate_limited" });
        }
        if !task.enabled {
            log(ScheduledTaskWebhookDeliveryOutcome::Disabled, false, vec![], None, None).await?;
            return Ok(WebhookTriggerResult::Disabled);
        }
        let signed = schedule.signature.is_some();
        if let Some(signature) = &schedule.signature {
            let verified = row
                .webhook_secret
                .as_deref()
                .is_some_and(|secret| verify_webhook_signature(signature, secret, &request.request.headers, &request.body));
            if !verified {
                log(ScheduledTaskWebhookDeliveryOutcome::RejectedSignature, false, vec![], None, None).await?;
                return Ok(WebhookTriggerResult::RejectedSignature);
            }
        }
        if let Some(Some(max_age)) = &schedule.max_delivery_age_minutes {
            if now - received > max_age.0 * 60_000 {
                log(ScheduledTaskWebhookDeliveryOutcome::Expired, signed, vec![], None, None).await?;
                return Ok(WebhookTriggerResult::Expired);
            }
        }
        let rendered = render_webhook_prompt(task.prompt.as_str(), &request.request);
        // A provider refuses a turn this long, so it is not started. The delivery is not retryable, so the
        // claim is kept. Providers trim the prompt before checking, so whitespace around it is free.
        if trim_wire_string(&rendered.prompt).encode_utf16().count() > PROVIDER_SEND_TURN_MAX_INPUT_CHARS {
            log(
                ScheduledTaskWebhookDeliveryOutcome::DispatchFailed,
                signed,
                rendered.missing.clone(),
                Some(rendered.prompt.clone()),
                Some("The filled-in prompt is too long.".into()),
            )
            .await?;
            return Ok(WebhookTriggerResult::Accepted { delivery_id, outcome: "prompt_too_long" });
        }
        // Bound the deliveries one task holds, so steady traffic to a stuck task cannot pile up parked
        // runs. A refused request is not logged, so it cannot push real deliveries out of the log.
        let queue_key = format!("{id}\u{0}{}", task.created_at);
        let queued = {
            let mut counts = self.inner.queued_deliveries.lock().unwrap();
            let count = counts.entry(queue_key.clone()).or_insert(0);
            if *count >= WEBHOOK_MAX_QUEUED_PER_TASK {
                false
            } else {
                *count += 1;
                self.inner.pending_deliveries.send_modify(|pending| *pending += 1);
                true
            }
        };
        if !queued {
            release_claim(self).await;
            return Ok(WebhookTriggerResult::RateLimited { outcome: "queue_full" });
        }
        let release = QueueRelease { inner: self.inner.clone(), key: queue_key };
        log(ScheduledTaskWebhookDeliveryOutcome::Accepted, signed, rendered.missing.clone(), Some(rendered.prompt.clone()), None).await?;
        let permit = self.delivery_permit(&id);
        let service = self.clone();
        let (run_task, prompt, run_delivery) = (task.clone(), rendered.prompt, delivery_id.clone());
        tokio::spawn(async move {
            let _release = release;
            // Deliveries for one task dispatch in arrival order rather than being dropped while an earlier one is still dispatching.
            let _permit = permit.acquire_owned().await;
            let outcome = service.run_task(&run_task, Trigger::Webhook { delivery_id: run_delivery.clone(), prompt }).await;
            let failure = match outcome {
                Ok(RunOutcome::Done(done)) if done.last_run_status == t3_contracts::ScheduledTaskRunStatus::Failed => Some("The run failed to start.".to_owned()),
                Ok(RunOutcome::Done(_)) => None,
                Ok(RunOutcome::Skipped(reason)) => Some(reason),
                // The log is readable over RPC, so it gets a fixed reason; the cause, which can carry request
                // data, stays in the server log.
                Err(error) => {
                    tracing::warn!(task_id = %run_task.id, message = %error.message, "Webhook dispatch failed");
                    Some("The run failed to start.".to_owned())
                }
            };
            if let Some(message) = failure {
                let _ = service
                    .db(move |store| {
                        store
                            .transaction(|transaction| {
                                Ok(transaction.execute(
                                    "UPDATE rust_scheduled_task_webhook_deliveries SET outcome = 'dispatch_failed', error = ?1 WHERE delivery_id = ?2",
                                    params![message, run_delivery],
                                )?)
                            })
                            .map(|_| ())
                            .map_err(|error| storage("Could not record webhook delivery.", None, error))
                    })
                    .await;
                service.notify_changed();
            }
        });
        Ok(WebhookTriggerResult::Accepted { delivery_id, outcome: "accepted" })
    }

    /// Resolves once every accepted webhook delivery has finished launching and been recorded.
    pub async fn wait_for_webhook_runs(&self) {
        let mut pending = self.inner.pending_deliveries.subscribe();
        let _ = pending.wait_for(|pending| *pending == 0).await;
    }

    fn delivery_permit(&self, id: &str) -> Arc<tokio::sync::Semaphore> {
        self.inner.delivery_permits.lock().unwrap().entry(id.to_owned()).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
    }

    /// Sliding one-minute window counting every request with a valid token, including ones the signature
    /// check later rejects.
    fn take_rate_slot(&self, id: &str, now: i64) -> RateSlot {
        let mut windows = self.inner.rate_windows.lock().unwrap();
        let window = windows.entry(id.to_owned()).or_default();
        window.accepted.retain(|at| now - at < 60_000);
        if window.accepted.len() >= WEBHOOK_RATE_LIMIT_PER_MINUTE {
            let first = !window.rejected_logged;
            window.rejected_logged = true;
            return if first { RateSlot::FirstRejected } else { RateSlot::Rejected };
        }
        window.accepted.push(now);
        window.rejected_logged = false;
        RateSlot::Allowed
    }
}

#[derive(PartialEq, Eq)]
enum RateSlot {
    Allowed,
    FirstRejected,
    Rejected,
}

struct QueueRelease {
    inner: Arc<Inner>,
    key: String,
}
impl Drop for QueueRelease {
    fn drop(&mut self) {
        let mut counts = self.inner.queued_deliveries.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.key);
            }
            self.inner.pending_deliveries.send_modify(|pending| *pending = pending.saturating_sub(1));
        }
    }
}

/// The list for `scheduledTasks.subscribe`: one snapshot, then a fresh list after every change.
pub struct TaskListSubscription {
    service: ScheduledTasks,
    changes: watch::Receiver<u64>,
    first: bool,
}
impl TaskListSubscription {
    /// None once the service is gone.
    pub async fn next(&mut self) -> Option<Result<ScheduledTaskListResult, ScheduledTaskError>> {
        if self.first {
            self.first = false;
        } else {
            self.changes.changed().await.ok()?;
        }
        Some(self.service.list().await)
    }
}

// ---- synchronous storage operations ----------------------------------------------------------------

fn find_task(store: &Store, id: &str, relay_base: Option<&str>) -> Result<Option<ScheduledTask>, ScheduledTaskError> {
    let row = store
        .read(|connection| row_for(connection, id).map_err(StoreError::from))
        .map_err(|error| storage("Could not load schedule task.", Some(id), error))?;
    row.map(|row| decode_row(&row, relay_base)).transpose()
}

fn new_webhook_token() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// When to look again: the earliest time an enabled task that is not running comes due.
fn next_wake(store: &Store) -> Result<Option<i64>, ScheduledTaskError> {
    let times = store
        .read(|connection| {
            let mut statement = connection.prepare("SELECT next_run_at FROM rust_scheduled_tasks WHERE enabled = 1 AND next_run_at IS NOT NULL AND last_run_status <> 'running'")?;
            Ok(statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .map_err(|error| storage("Could not list schedule tasks.", None, error))?;
    Ok(times.iter().filter_map(|time| parse_iso(time)).min())
}

/// Poll candidates are selected before their schedules or other JSON payloads are decoded, so a corrupt
/// row is skipped with a warning instead of stopping the poll.
fn due_tasks(store: &Store, now: i64, relay_base: Option<&str>) -> Result<Vec<(i64, ScheduledTask)>, ScheduledTaskError> {
    let rows = store
        .read(|connection| {
            select_rows(connection, "WHERE enabled = 1 AND next_run_at IS NOT NULL AND last_run_status <> 'running'", &[]).map_err(StoreError::from)
        })
        .map_err(|error| storage("Could not list schedule tasks.", None, error))?;
    let mut due = Vec::new();
    for row in rows {
        let Some(due_ms) = row.next_run_at.as_deref().and_then(parse_iso) else {
            tracing::warn!(task_id = %row.task_id, "Skipping schedule task row with invalid next_run_at");
            continue;
        };
        if due_ms > now {
            continue;
        }
        match decode_row(&row, relay_base) {
            Ok(task) => due.push((due_ms, task)),
            Err(error) => tracing::warn!(task_id = %row.task_id, message = %error.message, "Skipping undecodable schedule task row"),
        }
    }
    due.sort_by(|a, b| (a.0, a.1.id.as_str()).cmp(&(b.0, b.1.id.as_str())));
    Ok(due)
}

struct Claim {
    run_id: String,
    command_id: String,
    message_id: String,
}

fn claim_run(store: &Store, id: &str, started: &str, trigger: &str, fire_key: &str, delivery_id: Option<&str>) -> Result<Claim, ScheduledTaskError> {
    transact(store, |transaction| {
        let marked = transaction
            .execute(
                "UPDATE rust_scheduled_tasks SET updated_at = ?1, last_run_at = ?1, last_run_status = 'running', last_run_error = NULL WHERE task_id = ?2",
                params![started, id],
            )
            .map_err(|error| storage("Could not mark schedule task as running.", Some(id), error))?;
        // A task deleted after the re-read must not be dispatched from the stale snapshot.
        if marked == 0 {
            return Err(ScheduledTaskError::for_task("Schedule task not found.", id));
        }
        // Two runs claimed in one millisecond (a manual clock) must not share command ids, or the second
        // would resume the first's thread.
        let mut key = fire_key.to_owned();
        let mut suffix = 1;
        while transaction
            .query_row("SELECT 1 FROM rust_scheduled_task_runs WHERE run_id = ?1", [&key], |_| Ok(()))
            .optional()
            .map_err(|error| storage("Could not record schedule task run.", Some(id), error))?
            .is_some()
        {
            suffix += 1;
            key = format!("{fire_key}:{suffix}");
        }
        let claim = Claim { run_id: key.clone(), command_id: format!("scheduled-task:{key}"), message_id: format!("scheduled-task-message:{key}") };
        transaction
            .execute(
                &format!("INSERT INTO rust_scheduled_task_runs ({RUN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, NULL, 'running', ?5, ?6, NULL, ?7, NULL)"),
                params![claim.run_id, id, trigger, started, claim.command_id, claim.message_id, delivery_id],
            )
            .map_err(|error| storage("Could not record schedule task run.", Some(id), error))?;
        Ok(claim)
    })
}

fn complete_run(
    store: &Store,
    id: &str,
    started: &str,
    task_exists: bool,
    status: t3_contracts::ScheduledTaskRunStatus,
    (run_id, thread_id, error, completed_at, next_run_at): (String, Option<String>, Option<String>, String, Option<String>),
) -> Result<(), ScheduledTaskError> {
    transact(store, |transaction| {
        if task_exists {
            // The started time in the guard ensures this writes only to the row this run marked as running:
            // a task deleted mid-run and recreated with the same id must not be stamped.
            transaction
                .execute(
                    "UPDATE rust_scheduled_tasks SET updated_at = ?1, next_run_at = ?2, last_run_status = ?3, last_run_error = ?4, run_count = run_count + 1 WHERE task_id = ?5 AND last_run_status = 'running' AND last_run_at = ?6",
                    params![completed_at, next_run_at, status.as_str(), error, id, started],
                )
                .map_err(|error| storage("Could not record schedule task run.", Some(id), error))?;
        }
        transaction
            .execute(
                "UPDATE rust_scheduled_task_runs SET status = ?1, completed_at = ?2, thread_id = ?3, error = ?4 WHERE run_id = ?5",
                params![status.as_str(), completed_at, thread_id, error, run_id],
            )
            .map_err(|error| storage("Could not record schedule task run.", Some(id), error))?;
        transaction
            .execute(
                "DELETE FROM rust_scheduled_task_runs WHERE task_id = ?1 AND run_id NOT IN (SELECT run_id FROM rust_scheduled_task_runs WHERE task_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2)",
                params![id, RUN_HISTORY_RETENTION],
            )
            .map_err(|error| storage("Could not record schedule task run.", Some(id), error))?;
        Ok(())
    })
}

fn upsert_task(store: &Store, input: ScheduledTaskUpsertInput, now: i64, zone: &dyn Zone, relay_base: Option<&str>, uuid: &str, new_token: String) -> Result<ScheduledTask, ScheduledTaskError> {
    transact(store, |transaction| {
        let id = match (&input.id, &input.command_id) {
            (Some(id), _) => id.as_str().to_owned(),
            (None, Some(command)) => format!("scheduled-task:{}", command.as_str()),
            (None, None) => format!("scheduled-task:{uuid}"),
        };
        let existing_row = row_for(transaction, &id).map_err(|error| storage("Could not load schedule task.", Some(&id), error))?;
        // Look up by the resolved id so idempotent creates (command id replays) keep their run history, and
        // so real load failures propagate instead of silently resetting an existing row.
        let existing = existing_row.as_ref().map(|row| decode_row(row, relay_base)).transpose()?;
        let schedule = match &input.schedule {
            ScheduledTaskUpsertSchedule::Interval(fields) => ScheduledTaskSchedule::Interval(IntervalScheduleFields { every_ms: IntervalMs(fields.every_ms.0) }),
            ScheduledTaskUpsertSchedule::FixedTime(fields) => ScheduledTaskSchedule::FixedTime(fields.clone()),
            ScheduledTaskUpsertSchedule::Webhook(fields) => ScheduledTaskSchedule::Webhook(WebhookScheduleFields {
                signature: match fields.signature.as_ref().and_then(Option::as_ref) {
                    None => None,
                    Some(signature) => Some(ScheduledTaskWebhookSignature {
                        header: TrimmedNonEmptyString::new(signature.header.as_str().to_lowercase()).map_err(|error| storage("Could not save schedule task.", Some(&id), error))?,
                        encoding: signature.encoding,
                        prefix: signature.prefix.clone(),
                    }),
                },
                max_delivery_age_minutes: Some(fields.max_delivery_age_minutes.flatten()),
            }),
        };
        // The URL token and signing secret are kept out of the read model's JSON.
        let (token, secret, secret_changed) = match &input.schedule {
            ScheduledTaskUpsertSchedule::Webhook(fields) => {
                let signature = fields.signature.as_ref().and_then(Option::as_ref);
                // Saving a webhook task keeps its URL; only rotate changes it.
                let token = existing_row.as_ref().and_then(|row| row.webhook_token.clone()).unwrap_or(new_token);
                // A secretRef is a value the user entered for an agent; this save consumes it, so it
                // cannot be used again. A replay of a save that already used it keeps the secret that
                // save stored.
                let replay = input.command_id.is_some() && existing_row.as_ref().is_some_and(|row| row.webhook_secret.is_some());
                let from_ref = match signature.and_then(|signature| signature.secret_ref.as_ref()) {
                    None => None,
                    Some(reference) => {
                        let consumed = crate::secret_requests::SecretRequests::for_store(store)
                            .ok_or_else(|| crate::secret_requests::SecretRequestError { reason: "read_failed", cause: None })
                            .and_then(|secrets| secrets.consume(reference.as_str(), input.project_id.as_str()));
                        match consumed {
                            Ok(secret) => Some(secret),
                            Err(_) if replay => None,
                            Err(error) => return Err(ScheduledTaskError::for_task(error.message(), &id)),
                        }
                    }
                };
                // A ref, when given, is the only source: a plain secret sent alongside it must not
                // replace what a replayed save stored.
                let provided = match signature {
                    Some(signature) if signature.secret_ref.is_some() => from_ref,
                    _ => signature.and_then(|signature| signature.secret.as_ref()).map(|secret| secret.as_str().to_owned()),
                };
                let kept = existing_row.as_ref().and_then(|row| row.webhook_secret.clone());
                let secret = if signature.is_none() { None } else { provided.clone().or(kept) };
                let changed = signature.is_none() || provided.is_some() || existing_row.is_none();
                if signature.is_some() && secret.is_none() {
                    return Err(ScheduledTaskError::for_task("A webhook signature check needs a signing secret.", &id));
                }
                (Some(token), secret, changed)
            }
            _ => (None, None, true),
        };
        // Keep the existing next_run_at when the schedule itself is untouched: editing a title or prompt must
        // not postpone (or resurrect) a due run; only schedule or enabled changes restart the clock.
        let unchanged = existing.as_ref().is_some_and(|existing| existing.enabled == input.enabled && is_same_schedule(&existing.schedule, &schedule));
        let next_run_at = if unchanged { existing.as_ref().and_then(|existing| existing.next_run_at.clone()) } else { next_run_iso(input.enabled, &schedule, now, zone) };
        if input.require_existing == Some(true) && existing.is_none() {
            return Err(ScheduledTaskError::for_task("Schedule task not found.", &id));
        }
        let now_text = iso(now);
        let thread_id = input.thread_id.clone().flatten().map(|thread| thread.to_string());
        let save = |error| storage("Could not save schedule task.", Some(&id), error);
        match &existing {
            // Run-state columns (last_run_*, run_count) are left alone: they belong to the run transitions.
            Some(_) => {
                transaction
                    .execute(
                        "UPDATE rust_scheduled_tasks SET title = ?1, prompt = ?2, enabled = ?3, schedule_json = ?4, project_id = ?5, thread_id = ?6, workspace_strategy_json = ?7, model_selection_json = ?8, runtime_mode = ?9, interaction_mode = ?10, creation_source = ?11, updated_at = ?12, next_run_at = ?13, webhook_token = ?14, webhook_secret = CASE WHEN ?15 = 1 THEN ?16 ELSE webhook_secret END WHERE task_id = ?17",
                        params![
                            input.title.as_str(), input.prompt.as_str(), i64::from(input.enabled), json_text(&schedule), input.project_id.as_str(), thread_id,
                            json_text(&input.workspace_strategy), json_text(&input.model_selection), enum_text(&input.runtime_mode), enum_text(&input.interaction_mode),
                            enum_text(&input.creation_source.unwrap_or(t3_contracts::CreationSource::Web)),
                            now_text, next_run_at, token, i64::from(secret_changed), secret, id
                        ],
                    )
                    .map_err(save)?;
            }
            None => {
                transaction
                    .execute(
                        &format!("INSERT INTO rust_scheduled_tasks ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, NULL, 'never', NULL, 0, ?17, ?18)"),
                        params![
                            id, input.title.as_str(), input.prompt.as_str(), i64::from(input.enabled), json_text(&schedule), input.project_id.as_str(), thread_id,
                            json_text(&input.workspace_strategy), json_text(&input.model_selection), enum_text(&input.runtime_mode),
                            enum_text(&input.interaction_mode), enum_text(&input.created_by.unwrap_or(t3_contracts::Actor::User)),
                            enum_text(&input.creation_source.unwrap_or(t3_contracts::CreationSource::Web)), now_text, now_text, next_run_at, token, secret
                        ],
                    )
                    .map_err(save)?;
            }
        }
        let row = row_for(transaction, &id).map_err(|error| storage("Could not load schedule task.", Some(&id), error))?;
        row.map(|row| decode_row(&row, relay_base)).transpose()?.ok_or_else(|| ScheduledTaskError::for_task("Schedule task not found.", &id))
    })
}

// ---- webhook delivery log --------------------------------------------------------------------------

const DELIVERY_COLUMNS: &str = "delivery_id, task_id, received_at, method, query, headers_json, body, body_bytes, body_truncated, outcome, signature_verified, missing_fields_json, rendered_prompt, error";

struct DeliveryRow {
    delivery_id: String,
    task_id: String,
    received_at: String,
    method: String,
    query: String,
    headers_json: String,
    body: String,
    body_bytes: i64,
    body_truncated: i64,
    outcome: String,
    signature_verified: i64,
    missing_fields_json: String,
    rendered_prompt: Option<String>,
    error: Option<String>,
}
impl DeliveryRow {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            delivery_id: row.get(0)?,
            task_id: row.get(1)?,
            received_at: row.get(2)?,
            method: row.get(3)?,
            query: row.get(4)?,
            headers_json: row.get(5)?,
            body: row.get(6)?,
            body_bytes: row.get(7)?,
            body_truncated: row.get(8)?,
            outcome: row.get(9)?,
            signature_verified: row.get(10)?,
            missing_fields_json: row.get(11)?,
            rendered_prompt: row.get(12)?,
            error: row.get(13)?,
        })
    }
    fn headers(&self) -> serde_json::Map<String, Value> {
        serde_json::from_str::<serde_json::Map<String, Value>>(&self.headers_json).unwrap_or_default()
    }
    fn summary_value(&self) -> Value {
        json!({
            "id": self.delivery_id, "taskId": self.task_id, "receivedAt": self.received_at, "method": self.method,
            "contentType": self.headers().get("content-type").cloned().unwrap_or(Value::Null),
            "bodyBytes": self.body_bytes, "outcome": self.outcome, "signatureVerified": self.signature_verified == 1,
            "missingFields": serde_json::from_str::<Vec<String>>(&self.missing_fields_json).unwrap_or_default(), "error": self.error,
        })
    }
    fn detail_value(&self) -> Value {
        let mut value = self.summary_value();
        value["query"] = json!(self.query);
        value["headers"] = Value::Object(self.headers());
        value["body"] = json!(self.body);
        value["bodyTruncated"] = json!(self.body_truncated == 1);
        value["renderedPrompt"] = json!(self.rendered_prompt);
        value
    }
}

struct DeliveryEntry<'a> {
    id: String,
    task_id: String,
    received_at: String,
    request: &'a WebhookTriggerRequest,
    outcome: ScheduledTaskWebhookDeliveryOutcome,
    signature_verified: bool,
    missing: Vec<String>,
    rendered_prompt: Option<String>,
    error: Option<String>,
}
impl DeliveryEntry<'_> {
    fn into_record(self) -> DeliveryRow {
        let truncated = self.request.body.len() > WEBHOOK_DELIVERY_LOG_BODY_LIMIT;
        // Cut at a byte limit like the original, then read as UTF-8 text.
        let body = if truncated { String::from_utf8_lossy(&self.request.body[..WEBHOOK_DELIVERY_LOG_BODY_LIMIT]).into_owned() } else { self.request.request.body_text.clone() };
        let prompt = self.rendered_prompt.map(|prompt| prompt.chars().take(WEBHOOK_DELIVERY_LOG_PROMPT_LIMIT).collect());
        DeliveryRow {
            delivery_id: self.id,
            task_id: self.task_id,
            received_at: self.received_at,
            method: self.request.request.method.clone(),
            query: redact_query(&self.request.request.query),
            headers_json: serde_json::to_string(&redact_headers(&self.request.request.headers)).unwrap_or_else(|_| "{}".into()),
            body,
            body_bytes: self.request.body.len() as i64,
            body_truncated: i64::from(truncated),
            outcome: self.outcome.as_str().to_owned(),
            signature_verified: i64::from(self.signature_verified),
            missing_fields_json: serde_json::to_string(&self.missing).unwrap_or_else(|_| "[]".into()),
            rendered_prompt: prompt,
            error: self.error,
        }
    }
}

fn record_delivery(store: &Store, row: DeliveryRow) -> Result<(), ScheduledTaskError> {
    let task_id = row.task_id.clone();
    store
        .transaction(|transaction| {
            // Conditional on the task existing, so a delivery racing a delete cannot leave rows that a
            // recreated task with the same id would show. A held request retried after a rate limit reuses
            // its relay delivery id; the newer attempt replaces the logged one.
            transaction.execute(
                &format!(
                    "INSERT INTO rust_scheduled_task_webhook_deliveries ({DELIVERY_COLUMNS}) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14 WHERE EXISTS (SELECT 1 FROM rust_scheduled_tasks WHERE task_id = ?2) ON CONFLICT (delivery_id) DO UPDATE SET outcome = excluded.outcome, signature_verified = excluded.signature_verified, missing_fields_json = excluded.missing_fields_json, rendered_prompt = excluded.rendered_prompt, error = excluded.error"
                ),
                params![
                    row.delivery_id, row.task_id, row.received_at, row.method, row.query, row.headers_json, row.body, row.body_bytes,
                    row.body_truncated, row.outcome, row.signature_verified, row.missing_fields_json, row.rendered_prompt, row.error
                ],
            )?;
            // rowid breaks timestamp ties in arrival order; delivery ids are random.
            transaction.execute(
                "DELETE FROM rust_scheduled_task_webhook_deliveries WHERE task_id = ?1 AND delivery_id NOT IN (SELECT delivery_id FROM rust_scheduled_task_webhook_deliveries WHERE task_id = ?1 ORDER BY received_at DESC, rowid DESC LIMIT ?2)",
                params![row.task_id, WEBHOOK_DELIVERY_RETENTION],
            )?;
            Ok(())
        })
        .map_err(|error| storage("Could not record webhook delivery.", Some(&task_id), error))
}

/// Header map of a request in arrival order, with lowercased names (what the HTTP route builds).
pub fn lowercase_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> IndexMap<String, String> {
    headers.into_iter().map(|(name, value)| (name.to_lowercase(), value.to_owned())).collect()
}
