//! The scheduled tasks page's logic: how a task reads in the list, the form's draft and its checks,
//! and where a webhook task can be called. Port of `scheduledTasksSettings.logic.ts`, of
//! `scheduleLabel` in `ScheduledTasksSettings.tsx` and of `webhookAddress`. Times are shown as
//! absolute instants in the viewer's time zone: a "next run in 4m" label is only true until the
//! next minute, and keeping it true would need a timer that repaints the page.
use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use t3_contracts::{
    MAX_WEBHOOK_DELIVERY_AGE_MINUTES, MIN_SCHEDULED_TASK_INTERVAL_MS, ModelSelection, ProviderInteractionMode, RuntimeMode, ScheduledTask,
    ScheduledTaskListResult, ScheduledTaskRunStatus, ScheduledTaskSchedule, ScheduledTaskUpsertInput, ScheduledTaskWebhookDeliveryOutcome,
    ScheduledTaskWebhookEndpoint, ThreadLaunchWorkspaceStrategy,
};

pub const SUBSCRIBE: &str = "scheduledTasks.subscribe";
pub const UPSERT: &str = "scheduledTasks.upsert";
pub const SET_ENABLED: &str = "scheduledTasks.setEnabled";
pub const RUN_NOW: &str = "scheduledTasks.runNow";
pub const DELETE: &str = "scheduledTasks.delete";
pub const ROTATE_TOKEN: &str = "scheduledTasks.rotateWebhookToken";
pub const LIST_DELIVERIES: &str = "scheduledTasks.listWebhookDeliveries";
pub const GET_DELIVERY: &str = "scheduledTasks.getWebhookDelivery";

/// Prompt a new webhook task starts with: just the body, so request headers stay out unless the
/// user adds them.
pub const DEFAULT_WEBHOOK_PROMPT: &str = "Handle this webhook:\n{{body}}";
/// What a webhook task's prompt can name.
pub const WEBHOOK_HELP: &str = "Each request runs the prompt. Use {{body.path}}, {{headers.name}}, {{query.name}}, {{body}} or {{request}} in the prompt; only what it names reaches the agent.";
/// Fixed-time schedules follow the clock of the machine that runs the server.
pub const TIME_ZONE_NOTE: &str = "Times of day follow this environment's time zone, which may differ from yours.";

/// JS day-of-week (0 = Sunday), as `WEEKDAY_LABELS` indexes it.
const WEEKDAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEKDAY_SHORT: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
/// The week as people read it: Monday first.
pub const WEEKDAY_ORDER: [u8; 7] = [1, 2, 3, 4, 5, 6, 0];
pub fn weekday_label(day: u8) -> &'static str {
    WEEKDAY_LABELS[usize::from(day % 7)]
}
pub fn weekday_short(day: u8) -> &'static str {
    WEEKDAY_SHORT[usize::from(day % 7)]
}

/// What `Number(text)` makes of text, for the numbers a form field holds.
pub fn js_number(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |value| value as f64);
        }
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust also reads "inf", "nan" and "infinity"; JS does not.
    if !text.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}
/// `String(number)` for the whole and simple fractional values a form holds.
pub fn js_number_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 { format!("{}", value as i64) } else { format!("{value}") }
}

/// `parseMaxDeliveryAge`: Ok(None) for blank (no limit), Err for a value that cannot be saved.
pub fn parse_max_delivery_age(value: &str) -> Result<Option<i64>, ()> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let minutes = js_number(value);
    if minutes.fract() == 0.0 && minutes > 0.0 && minutes <= MAX_WEBHOOK_DELIVERY_AGE_MINUTES as f64 { Ok(Some(minutes as i64)) } else { Err(()) }
}

// ---- how a task reads in the list ----

pub fn schedule_label(schedule: &ScheduledTaskSchedule) -> String {
    match schedule {
        ScheduledTaskSchedule::Webhook(_) => "On webhook".into(),
        ScheduledTaskSchedule::Interval(fields) => {
            let every = fields.every_ms.0;
            if every % 60_000 == 0 { format!("Every {} min", every / 60_000) } else { format!("Every {} sec", ((every as f64) / 1000.0).round() as i64) }
        }
        ScheduledTaskSchedule::FixedTime(fields) => {
            let weekdays: Vec<u8> = fields.weekdays.as_ref().map(|days| days.iter().map(|day| day.0 as u8).collect()).unwrap_or_default();
            let days = if weekdays.is_empty() {
                "Daily".to_owned()
            } else if weekdays.len() == 5 && weekdays.iter().all(|day| (1..=5).contains(day)) {
                "Weekdays".to_owned()
            } else {
                weekdays.iter().map(|day| weekday_label(*day)).collect::<Vec<_>>().join(", ")
            };
            format!("{days} at {}", fields.time_of_day.as_str())
        }
    }
}

/// The viewer's offset from UTC in minutes east at an instant (milliseconds since the epoch),
/// however the platform learns it. It is asked per instant so a daylight saving change between
/// now and a task's next run is not ignored.
pub type Zone<'a> = &'a dyn Fn(i64) -> i32;

/// "Wed 9 Sep, 14:30" in the viewer's zone; the text as given when it is not a time.
pub fn instant_label(iso: &str, zone: Zone<'_>) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(iso) else { return iso.to_owned() };
    let offset = FixedOffset::east_opt(zone(parsed.timestamp_millis()).saturating_mul(60)).unwrap_or_else(|| FixedOffset::east_opt(0).expect("utc"));
    parsed.with_timezone(&offset).format("%a %-d %b, %H:%M").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Error,
    Info,
    Outline,
}
impl Tone {
    pub fn class(self) -> &'static str {
        match self {
            Self::Success => "badge success",
            Self::Warning => "badge warning",
            Self::Error => "badge failure",
            Self::Info => "badge info",
            Self::Outline => "badge",
        }
    }
}
pub fn status_tone(status: ScheduledTaskRunStatus) -> Tone {
    match status {
        ScheduledTaskRunStatus::Failed => Tone::Error,
        ScheduledTaskRunStatus::Succeeded => Tone::Success,
        ScheduledTaskRunStatus::Running => Tone::Info,
        ScheduledTaskRunStatus::Never => Tone::Outline,
    }
}
pub fn delivery_outcome_label(outcome: ScheduledTaskWebhookDeliveryOutcome) -> &'static str {
    use ScheduledTaskWebhookDeliveryOutcome::*;
    match outcome {
        Accepted => "Ran",
        DispatchFailed => "Run failed",
        RejectedSignature => "Bad signature",
        Disabled => "Task paused",
        RateLimited => "Rate limited",
        Expired => "Too old",
    }
}
pub fn delivery_tone(outcome: ScheduledTaskWebhookDeliveryOutcome) -> Tone {
    use ScheduledTaskWebhookDeliveryOutcome::*;
    match outcome {
        Accepted => Tone::Success,
        Disabled | RateLimited | Expired => Tone::Warning,
        DispatchFailed | RejectedSignature => Tone::Error,
    }
}

/// One task as its row says it.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub schedule: String,
    /// "Paused", "Listening", "Next run Wed 9 Sep, 14:30" or "Not scheduled".
    pub next: String,
    pub last: Option<String>,
    pub status: Option<ScheduledTaskRunStatus>,
    pub error: Option<String>,
}
pub fn summary(task: &ScheduledTask, zone: Zone<'_>) -> Summary {
    let webhook = matches!(task.schedule, ScheduledTaskSchedule::Webhook(_));
    let next = if !task.enabled {
        "Paused".to_owned()
    } else if webhook {
        "Listening".to_owned()
    } else {
        task.next_run_at.as_deref().map_or_else(|| "Not scheduled".to_owned(), |at| format!("Next run {}", instant_label(at, zone)))
    };
    Summary {
        schedule: schedule_label(&task.schedule),
        next,
        last: task.last_run_at.as_deref().map(|at| format!("Last run {}", instant_label(at, zone))),
        status: (task.last_run_status != ScheduledTaskRunStatus::Never).then_some(task.last_run_status),
        error: task.last_run_error.clone().filter(|error| !error.is_empty()),
    }
}

/// The list the server sent, whole: every change arrives as the full list again.
pub fn decode_tasks(value: &Value) -> Result<Vec<ScheduledTask>, String> {
    serde_json::from_value::<ScheduledTaskListResult>(value.clone()).map(|list| list.tasks.0).map_err(|error| format!("Invalid scheduled task list: {error}"))
}

pub fn is_webhook(task: &ScheduledTask) -> bool {
    matches!(task.schedule, ScheduledTaskSchedule::Webhook(_))
}

// ---- where a webhook can be called ----

/// Where a sender can call a webhook task, as a client shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookAddress {
    /// The URL to give a sender, or the bare path when no address is known.
    pub address: String,
    /// Whether `address` is a full URL a sender can call.
    pub copyable: bool,
    /// One line on who can reach `address`; None for a T3 Connect URL.
    pub note: Option<&'static str>,
}
fn loopback_host(host: &str) -> bool {
    let host = host.to_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    if host == "localhost" || host == "::1" {
        return true;
    }
    let parts: Vec<f64> = host.split('.').map(js_number).collect();
    parts.len() == 4 && parts.iter().all(|part| part.fract() == 0.0 && (0.0..=255.0).contains(part)) && parts[0] == 127.0
}
/// `webhookAddress`: `http_base_url` is the address this client reaches the environment at.
pub fn webhook_address(endpoint: &ScheduledTaskWebhookEndpoint, http_base_url: Option<&str>) -> WebhookAddress {
    if let Some(url) = &endpoint.url {
        return WebhookAddress { address: url.as_str().to_owned(), copyable: true, note: None };
    }
    let path = endpoint.path.as_str();
    let resolved = http_base_url.and_then(|base| url::Url::parse(base).ok()).and_then(|base| base.join(path).ok());
    let Some(url) = resolved else {
        return WebhookAddress { address: path.to_owned(), copyable: false, note: Some("Link this environment to T3 Connect for a public URL.") };
    };
    let local = url.host_str().is_some_and(loopback_host);
    WebhookAddress {
        address: url.to_string(),
        copyable: true,
        note: Some(if local {
            "Only this computer can call this address. Link T3 Connect for a public URL."
        } else {
            "Works wherever this environment's address is reachable, for example over Tailscale or your own proxy. Link T3 Connect for a public URL."
        }),
    }
}

// ---- the form ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleMode {
    Fixed,
    Interval,
    Webhook,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    Root,
    Worktree,
    ExistingWorktree,
}
impl WorkspaceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Worktree => "Create a new worktree",
            Self::Root => "Use the project checkout",
            Self::ExistingWorktree => "Use a specific checkout",
        }
    }
    pub fn value(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Worktree => "worktree",
            Self::ExistingWorktree => "existing_worktree",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Worktree, Self::Root, Self::ExistingWorktree].into_iter().find(|mode| mode.value() == value)
    }
}

/// `DraftState`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub editing_id: Option<String>,
    pub title: String,
    pub prompt: String,
    pub enabled: bool,
    pub schedule_mode: ScheduleMode,
    pub interval_minutes: String,
    pub time_of_day: String,
    pub weekdays: BTreeSet<u8>,
    pub project_id: String,
    /// Not editable in the form; kept so editing a task bound to a thread keeps it bound.
    pub thread_id: String,
    pub workspace_mode: WorkspaceMode,
    pub base_ref: String,
    pub start_from_origin: bool,
    pub existing_worktree_path: String,
    /// `instance:model`, empty while the default applies.
    pub model_key: String,
    /// Not editable in the form, but kept so editing an agent-created task keeps its modes.
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
    /// The task's own selection: the picker edits only instance and model, so unchanged choices
    /// keep the provider options this holds.
    pub base_model_selection: Option<ModelSelection>,
    pub signature_enabled: bool,
    pub signature_header: String,
    pub signature_encoding: String,
    pub signature_prefix: String,
    /// Write-only: empty keeps the secret already stored on the server.
    pub signature_secret: String,
    /// Minutes as typed; empty runs every held request regardless of age.
    pub max_delivery_age_minutes: String,
}

/// GitHub's signature settings, the most common sender.
pub const SIGNATURE_DEFAULTS: (&str, &str, &str) = ("x-hub-signature-256", "hex", "sha256=");

impl Draft {
    pub fn new(project_id: &str) -> Self {
        Self {
            editing_id: None,
            title: String::new(),
            prompt: String::new(),
            enabled: true,
            schedule_mode: ScheduleMode::Fixed,
            interval_minutes: "15".into(),
            time_of_day: "09:00".into(),
            weekdays: [1, 2, 3, 4, 5].into(),
            project_id: project_id.to_owned(),
            thread_id: String::new(),
            workspace_mode: WorkspaceMode::Worktree,
            base_ref: "main".into(),
            start_from_origin: true,
            existing_worktree_path: String::new(),
            model_key: String::new(),
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: ProviderInteractionMode::Default,
            base_model_selection: None,
            signature_enabled: false,
            signature_header: SIGNATURE_DEFAULTS.0.into(),
            signature_encoding: SIGNATURE_DEFAULTS.1.into(),
            signature_prefix: SIGNATURE_DEFAULTS.2.into(),
            signature_secret: String::new(),
            max_delivery_age_minutes: String::new(),
        }
    }

    /// `taskToDraft`.
    pub fn from_task(task: &ScheduledTask) -> Self {
        let mut draft = Self::new(task.project_id.as_str());
        draft.editing_id = Some(task.id.as_str().to_owned());
        draft.title = task.title.as_str().to_owned();
        draft.prompt = task.prompt.as_str().to_owned();
        draft.enabled = task.enabled;
        draft.weekdays = (0..7).collect();
        draft.thread_id = task.thread_id.as_ref().map(|id| id.as_str().to_owned()).unwrap_or_default();
        draft.model_key = format!("{}:{}", task.model_selection.instance_id, task.model_selection.model);
        draft.runtime_mode = task.runtime_mode;
        draft.interaction_mode = task.interaction_mode;
        draft.base_model_selection = Some(task.model_selection.clone());
        draft.interval_minutes = "15".into();
        match &task.schedule {
            ScheduledTaskSchedule::Interval(fields) => {
                draft.schedule_mode = ScheduleMode::Interval;
                draft.interval_minutes = js_number_string((fields.every_ms.0 as f64 / 60_000.0).max(1.0));
            }
            ScheduledTaskSchedule::FixedTime(fields) => {
                draft.schedule_mode = ScheduleMode::Fixed;
                draft.time_of_day = fields.time_of_day.as_str().to_owned();
                if let Some(days) = fields.weekdays.as_ref().filter(|days| !days.is_empty()) {
                    draft.weekdays = days.iter().map(|day| day.0 as u8).collect();
                }
            }
            ScheduledTaskSchedule::Webhook(fields) => {
                draft.schedule_mode = ScheduleMode::Webhook;
                if let Some(signature) = &fields.signature {
                    draft.signature_enabled = true;
                    draft.signature_header = signature.header.as_str().to_owned();
                    draft.signature_encoding = serde_json::to_value(signature.encoding).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default();
                    draft.signature_prefix = signature.prefix.clone();
                }
                if let Some(Some(minutes)) = &fields.max_delivery_age_minutes {
                    draft.max_delivery_age_minutes = minutes.0.to_string();
                }
            }
        }
        match &task.workspace_strategy.0 {
            ThreadLaunchWorkspaceStrategy::Root(_) => {
                draft.workspace_mode = WorkspaceMode::Root;
            }
            ThreadLaunchWorkspaceStrategy::ExistingWorktree(worktree) => {
                draft.workspace_mode = WorkspaceMode::ExistingWorktree;
                draft.existing_worktree_path = worktree.worktree_path.as_str().to_owned();
            }
            ThreadLaunchWorkspaceStrategy::Worktree(worktree) => {
                draft.workspace_mode = WorkspaceMode::Worktree;
                draft.base_ref = worktree.base_ref.as_str().to_owned();
                draft.start_from_origin = matches!(worktree.start_from_origin, Some(Some(true)));
            }
        }
        // A mode the task does not use keeps the form's defaults, as the original's draft does.
        if draft.workspace_mode != WorkspaceMode::Worktree {
            draft.base_ref = "main".into();
            draft.start_from_origin = true;
        }
        draft
    }

    /// Switching to a webhook gives a blank prompt the default one.
    pub fn set_mode(&mut self, mode: ScheduleMode) {
        if mode == ScheduleMode::Webhook && self.prompt.trim().is_empty() {
            self.prompt = DEFAULT_WEBHOOK_PROMPT.into();
        }
        self.schedule_mode = mode;
    }

    /// `scheduleFromDraft`: None when the webhook age limit is invalid.
    pub fn schedule(&self) -> Option<Value> {
        match self.schedule_mode {
            ScheduleMode::Webhook => {
                let max = parse_max_delivery_age(&self.max_delivery_age_minutes).ok()?;
                let secret = self.signature_secret.trim();
                let signature = if self.signature_enabled {
                    let mut signature = json!({"header":self.signature_header.trim(),"encoding":self.signature_encoding,"prefix":self.signature_prefix});
                    if !secret.is_empty() {
                        signature["secret"] = json!(secret);
                    }
                    signature
                } else {
                    Value::Null
                };
                Some(json!({"type":"webhook","signature":signature,"maxDeliveryAgeMinutes":max}))
            }
            ScheduleMode::Interval => {
                let every = (js_number(&self.interval_minutes) * 60_000.0).round();
                // NaN has no JSON form; the caller's interval check rejects it first.
                Some(json!({"type":"interval","everyMs": if every.is_finite() && every.abs() <= 9_007_199_254_740_991.0 { json!(every as i64) } else { Value::Null }}))
            }
            ScheduleMode::Fixed => {
                let every_day = self.weekdays.is_empty() || self.weekdays.len() == 7;
                let mut schedule = json!({"type":"fixed_time","timeOfDay": if self.time_of_day.is_empty() { "09:00" } else { self.time_of_day.as_str() }});
                if !every_day {
                    schedule["weekdays"] = json!(self.weekdays.iter().collect::<Vec<_>>());
                }
                Some(schedule)
            }
        }
    }
}

/// `${instanceId}:${model}` back into its parts.
pub fn split_model_key(value: &str) -> Option<(&str, &str)> {
    let index = value.find(':')?;
    (index > 0 && index + 1 < value.len()).then(|| (&value[..index], &value[index + 1..]))
}

/// What the form's save checks need from the page around it.
pub struct Context<'a> {
    pub project_ids: &'a [String],
    /// The model the picker shows: the draft's own choice, else the default.
    pub selection: Option<ModelSelection>,
    /// Whether the task being edited already holds a signing secret on the server.
    pub has_saved_secret: bool,
}

/// A refusal to save, worded as the original's toast: a title and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub title: &'static str,
    pub message: String,
}
fn invalid(title: &'static str, message: impl Into<String>) -> Invalid {
    Invalid { title, message: message.into() }
}

impl Draft {
    /// The `scheduledTasks.upsert` payload, or why the form cannot be saved yet.
    pub fn upsert_input(&self, context: &Context<'_>, creation_source: &str) -> Result<Value, Invalid> {
        let selection = match &context.selection {
            Some(selection) if !self.title.trim().is_empty() && !self.prompt.trim().is_empty() && context.project_ids.contains(&self.project_id) => selection,
            _ => return Err(invalid("Scheduled task is incomplete", "Add a title, prompt, project, and model.")),
        };
        let Some(schedule) = self.schedule() else {
            return Err(invalid("Invalid age limit", format!("Enter whole minutes from 1 to {MAX_WEBHOOK_DELIVERY_AGE_MINUTES}, or leave it blank.")));
        };
        if self.schedule_mode == ScheduleMode::Webhook && schedule["signature"].is_object() {
            let missing_secret = schedule["signature"]["secret"].is_null() && !context.has_saved_secret;
            if schedule["signature"]["header"].as_str().is_none_or(str::is_empty) || missing_secret {
                return Err(invalid("Signing secret is required", "Enter the signature header and secret."));
            }
        }
        if self.schedule_mode == ScheduleMode::Interval {
            let valid = schedule["everyMs"].as_i64().is_some_and(|every| every >= MIN_SCHEDULED_TASK_INTERVAL_MS);
            if !valid {
                return Err(invalid("Invalid interval", "Enter an interval of at least one minute."));
            }
        }
        if self.schedule_mode == ScheduleMode::Fixed && t3_contracts::parse_time_of_day(schedule["timeOfDay"].as_str().unwrap_or_default()).is_none() {
            return Err(invalid("Invalid time", "Enter a time of day such as 09:30."));
        }
        if self.workspace_mode == WorkspaceMode::ExistingWorktree && self.existing_worktree_path.trim().is_empty() {
            return Err(invalid("Checkout path is required", "Enter the path of the checkout to run in."));
        }
        // The original selection object (with provider options) stays when the picker still
        // points at the same instance and model.
        let model_selection = match &self.base_model_selection {
            Some(base) if base.instance_id == selection.instance_id && base.model == selection.model => base.clone(),
            _ => selection.clone(),
        };
        let workspace = match self.workspace_mode {
            WorkspaceMode::Root => json!({"type":"root"}),
            WorkspaceMode::ExistingWorktree => json!({"type":"existing_worktree","worktreePath":self.existing_worktree_path.trim()}),
            WorkspaceMode::Worktree => json!({"type":"worktree","baseRef": if self.base_ref.trim().is_empty() { "main" } else { self.base_ref.trim() },"startFromOrigin":self.start_from_origin}),
        };
        let mut input = json!({
            "title": self.title.trim(), "prompt": self.prompt.trim(), "enabled": self.enabled, "schedule": schedule,
            "projectId": self.project_id, "threadId": if self.thread_id.is_empty() { Value::Null } else { json!(self.thread_id) },
            "workspaceStrategy": workspace, "modelSelection": model_selection,
            "runtimeMode": self.runtime_mode, "interactionMode": self.interaction_mode, "creationSource": creation_source,
        });
        if let Some(id) = &self.editing_id {
            input["id"] = json!(id);
            input["requireExisting"] = json!(true);
        }
        // A payload the server could not decode comes back as a defect; say what is wrong here.
        serde_json::from_value::<ScheduledTaskUpsertInput>(input.clone()).map_err(|error| invalid("Scheduled task is invalid", error.to_string()))?;
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(schedule: Value, extra: Value) -> ScheduledTask {
        let mut value = json!({
            "id":"scheduled-task:one","title":"Review","prompt":"Review the open pull requests.","enabled":true,"schedule":schedule,"projectId":"project","threadId":null,
            "workspaceStrategy":{"type":"root"},"modelSelection":{"instanceId":"codex","model":"fixture-model","options":[{"id":"reasoningEffort","value":"high"}]},
            "runtimeMode":"approval-required","interactionMode":"plan","createdBy":"user","creationSource":"web","createdAt":"2026-09-09T12:00:00.000Z","updatedAt":"2026-09-09T12:00:00.000Z",
            "nextRunAt":"2026-09-09T12:05:00.000Z","lastRunAt":null,"lastRunStatus":"never","lastRunError":null,"runCount":0
        });
        for (key, extra) in extra.as_object().cloned().unwrap_or_default() {
            value[key] = extra;
        }
        serde_json::from_value(value).unwrap()
    }
    fn context(selection: bool) -> (Vec<String>, Option<ModelSelection>) {
        (vec!["project".into()], selection.then(|| serde_json::from_value(json!({"instanceId":"codex","model":"fixture-model"})).unwrap()))
    }

    /// Rows the original produced: `node tools/generate_client_scheduled_task_fixtures.mjs`.
    #[test]
    fn the_original_labels_drafts_schedules_and_addresses_are_reproduced() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/scheduled-tasks.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "label" => {
                    let schedule: ScheduledTaskSchedule = serde_json::from_value(row["schedule"].clone()).unwrap();
                    assert_eq!(schedule_label(&schedule), row["label"].as_str().unwrap(), "{row}");
                }
                "age" => {
                    let parsed = parse_max_delivery_age(row["input"].as_str().unwrap());
                    let expected = match &row["result"] {
                        Value::Null => Ok(None),
                        Value::String(text) if text == "invalid" => Err(()),
                        number => Ok(number.as_i64()),
                    };
                    assert_eq!(parsed, expected, "{row}");
                }
                "draft" => {
                    let task: ScheduledTask = serde_json::from_value(row["task"].clone()).unwrap();
                    let draft = Draft::from_task(&task);
                    assert_eq!(serde_json::to_value(&draft).unwrap(), row["draft"], "{row}");
                    // The same draft makes the same schedule the original sends.
                    assert_eq!(draft.schedule(), row["schedule"].as_object().map(|_| row["schedule"].clone()).or(None), "{row}");
                }
                "schedule" => {
                    let mut draft = Draft::new("project");
                    draft.schedule_mode = match row["draft"]["scheduleMode"].as_str().unwrap() {
                        "interval" => ScheduleMode::Interval,
                        "webhook" => ScheduleMode::Webhook,
                        _ => ScheduleMode::Fixed,
                    };
                    let text = |name: &str| row["draft"][name].as_str().unwrap().to_owned();
                    draft.interval_minutes = text("intervalMinutes");
                    draft.time_of_day = text("timeOfDay");
                    draft.weekdays = row["draft"]["weekdays"].as_array().unwrap().iter().map(|day| day.as_u64().unwrap() as u8).collect();
                    draft.signature_enabled = row["draft"]["signatureEnabled"].as_bool().unwrap();
                    draft.signature_header = text("signatureHeader");
                    draft.signature_encoding = text("signatureEncoding");
                    draft.signature_prefix = text("signaturePrefix");
                    draft.signature_secret = text("signatureSecret");
                    draft.max_delivery_age_minutes = text("maxDeliveryAgeMinutes");
                    let expected = (!row["schedule"].is_null()).then(|| row["schedule"].clone());
                    // The original lets a bad interval through to its own check as NaN or a small
                    // number; both mean the same here, so compare the cases that make a schedule.
                    let actual = draft.schedule();
                    match (&expected, &actual) {
                        (Some(expected), Some(actual)) if expected["type"] == "interval" && !expected["everyMs"].is_number() => assert!(actual["everyMs"].is_null(), "{row}"),
                        _ => assert_eq!(actual, expected, "{row}"),
                    }
                }
                "address" => {
                    let endpoint: ScheduledTaskWebhookEndpoint = serde_json::from_value(row["endpoint"].clone()).unwrap();
                    let actual = webhook_address(&endpoint, row["base"].as_str());
                    assert_eq!((actual.address.as_str(), actual.copyable, actual.note), (row["address"].as_str().unwrap(), row["copyable"].as_bool().unwrap(), row["note"].as_str()), "{row}");
                }
                other => panic!("unknown fixture row {other}"),
            }
            checked += 1;
        }
        assert!(checked > 60, "{checked}");
    }

    #[test]
    fn a_task_reads_with_its_schedule_next_run_last_run_and_error() {
        let offset = |_: i64| 120;
        let mut interval = task(json!({"type":"interval","everyMs":300000}), json!({}));
        assert_eq!(summary(&interval, &offset), Summary { schedule: "Every 5 min".into(), next: "Next run Wed 9 Sep, 14:05".into(), last: None, status: None, error: None });
        interval.enabled = false;
        assert_eq!(summary(&interval, &offset).next, "Paused");
        let failed = task(
            json!({"type":"fixed_time","timeOfDay":"9:30","weekdays":[1,2,3,4,5]}),
            json!({"lastRunAt":"2026-09-09T12:00:00.000Z","lastRunStatus":"failed","lastRunError":"Worktree setup is not available.","nextRunAt":null}),
        );
        let failed = summary(&failed, &|_| 0);
        assert_eq!((failed.schedule.as_str(), failed.next.as_str(), failed.last.as_deref()), ("Weekdays at 9:30", "Not scheduled", Some("Last run Wed 9 Sep, 12:00")));
        assert_eq!((failed.status, failed.error.as_deref()), (Some(ScheduledTaskRunStatus::Failed), Some("Worktree setup is not available.")));
        let hook = task(json!({"type":"webhook","signature":null,"maxDeliveryAgeMinutes":null}), json!({"nextRunAt":null}));
        assert_eq!(summary(&hook, &|_| 0).next, "Listening");
        // An instant that is not a time is shown as given, and a negative offset moves the date.
        assert_eq!(instant_label("not a time", &|_| 0), "not a time");
        assert_eq!(instant_label("2026-09-09T01:00:00.000Z", &|_| -300), "Tue 8 Sep, 20:00");
    }

    #[test]
    fn the_list_the_server_sends_decodes_and_one_task_this_client_cannot_read_is_dropped() {
        let good = serde_json::to_value(task(json!({"type":"interval","everyMs":60000}), json!({}))).unwrap();
        let tasks = decode_tasks(&json!({"tasks":[good, {"id":"future","schedule":{"type":"cron"}}]})).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(decode_tasks(&json!({"nope":true})).is_err());
    }

    #[test]
    fn a_draft_saves_only_when_the_original_would_and_says_why_not() {
        let (projects, selection) = context(true);
        let ready = |draft: &Draft| draft.upsert_input(&Context { project_ids: &projects, selection: selection.clone(), has_saved_secret: false }, "web");
        let mut draft = Draft::new("project");
        assert_eq!(ready(&draft).unwrap_err().title, "Scheduled task is incomplete");
        draft.title = " Review ".into();
        draft.prompt = " Look ".into();
        let input = ready(&draft).unwrap();
        assert_eq!((input["title"].as_str(), input["prompt"].as_str(), input["threadId"].is_null(), input.get("id")), (Some("Review"), Some("Look"), true, None));
        assert_eq!(input["schedule"], json!({"type":"fixed_time","timeOfDay":"09:00","weekdays":[1,2,3,4,5]}));
        assert_eq!(input["workspaceStrategy"], json!({"type":"worktree","baseRef":"main","startFromOrigin":true}));
        // No model, or a project the page does not list, is incomplete too.
        let none = Context { project_ids: &projects, selection: None, has_saved_secret: false };
        assert_eq!(draft.upsert_input(&none, "web").unwrap_err().message, "Add a title, prompt, project, and model.");
        draft.project_id = "elsewhere".into();
        assert_eq!(ready(&draft).unwrap_err().title, "Scheduled task is incomplete");
        draft.project_id = "project".into();

        draft.set_mode(ScheduleMode::Interval);
        for (text, ok) in [("15", true), ("1", true), ("1.5", true), ("0.5", false), ("", false), ("abc", false), ("-3", false)] {
            draft.interval_minutes = text.into();
            let result = ready(&draft);
            assert_eq!(result.is_ok(), ok, "{text}: {result:?}");
            if !ok {
                assert_eq!((result.as_ref().unwrap_err().title, result.unwrap_err().message.as_str()), ("Invalid interval", "Enter an interval of at least one minute."));
            }
        }
        draft.interval_minutes = "1.5".into();
        assert_eq!(ready(&draft).unwrap()["schedule"], json!({"type":"interval","everyMs":90000}));

        draft.set_mode(ScheduleMode::Fixed);
        draft.time_of_day = "25:00".into();
        assert_eq!(ready(&draft).unwrap_err().title, "Invalid time");
        draft.time_of_day = String::new();
        assert_eq!(ready(&draft).unwrap()["schedule"]["timeOfDay"], "09:00");
        draft.weekdays = (0..7).collect();
        assert_eq!(ready(&draft).unwrap()["schedule"].get("weekdays"), None, "every day is the same as none");

        draft.workspace_mode = WorkspaceMode::ExistingWorktree;
        assert_eq!(ready(&draft).unwrap_err().title, "Checkout path is required");
        draft.existing_worktree_path = " /work/tree ".into();
        assert_eq!(ready(&draft).unwrap()["workspaceStrategy"], json!({"type":"existing_worktree","worktreePath":"/work/tree"}));
        draft.workspace_mode = WorkspaceMode::Root;
        assert_eq!(ready(&draft).unwrap()["workspaceStrategy"], json!({"type":"root"}));
    }

    #[test]
    fn a_webhook_draft_needs_its_header_and_a_secret_unless_one_is_stored_and_checks_the_age_limit() {
        let (projects, selection) = context(true);
        let mut draft = Draft::new("project");
        draft.title = "Hook".into();
        draft.set_mode(ScheduleMode::Webhook);
        assert_eq!(draft.prompt, DEFAULT_WEBHOOK_PROMPT, "a blank prompt gets the webhook prompt");
        draft.prompt = "mine".into();
        draft.set_mode(ScheduleMode::Webhook);
        assert_eq!(draft.prompt, "mine");
        let save = |draft: &Draft, saved: bool| draft.upsert_input(&Context { project_ids: &projects, selection: selection.clone(), has_saved_secret: saved }, "web");
        assert_eq!(save(&draft, false).unwrap()["schedule"], json!({"type":"webhook","signature":null,"maxDeliveryAgeMinutes":null}));
        draft.signature_enabled = true;
        assert_eq!(save(&draft, false).unwrap_err().title, "Signing secret is required");
        assert!(save(&draft, true).is_ok(), "a stored secret is kept when the field stays empty");
        draft.signature_secret = " s3cret ".into();
        assert_eq!(save(&draft, false).unwrap()["schedule"]["signature"], json!({"header":"x-hub-signature-256","encoding":"hex","prefix":"sha256=","secret":"s3cret"}));
        draft.signature_header = "  ".into();
        assert_eq!(save(&draft, false).unwrap_err().title, "Signing secret is required");
        draft.signature_header = "X-Sig".into();
        for (text, ok) in [("", true), ("60", true), ("1440", true), ("1441", false), ("0", false), ("1.5", false), ("soon", false)] {
            draft.max_delivery_age_minutes = text.into();
            let result = save(&draft, false);
            assert_eq!(result.is_ok(), ok, "{text}");
            if !ok {
                assert_eq!(result.unwrap_err().message, "Enter whole minutes from 1 to 1440, or leave it blank.");
            }
        }
    }

    #[test]
    fn editing_keeps_what_the_form_does_not_show_and_the_model_options_while_the_model_is_unchanged() {
        let (projects, selection) = context(true);
        let existing = task(json!({"type":"interval","everyMs":300000}), json!({"threadId":"thread-1"}));
        let draft = Draft::from_task(&existing);
        let save = |draft: &Draft, selection: &Option<ModelSelection>| draft.upsert_input(&Context { project_ids: &projects, selection: selection.clone(), has_saved_secret: false }, "web").unwrap();
        let input = save(&draft, &selection);
        assert_eq!((input["id"].as_str(), input["requireExisting"].as_bool(), input["threadId"].as_str()), (Some("scheduled-task:one"), Some(true), Some("thread-1")));
        assert_eq!((input["runtimeMode"].as_str(), input["interactionMode"].as_str()), (Some("approval-required"), Some("plan")));
        assert_eq!(input["modelSelection"]["options"][0]["value"], "high");
        let other: Option<ModelSelection> = serde_json::from_value(json!({"instanceId":"codex","model":"other-model"})).unwrap();
        assert_eq!(save(&draft, &other)["modelSelection"], json!({"instanceId":"codex","model":"other-model"}));
        // A legacy interval under a minute shows as one minute and saves as one.
        let legacy = Draft::from_task(&task(json!({"type":"interval","everyMs":30000}), json!({})));
        assert_eq!(legacy.interval_minutes, "1");
        assert_eq!(split_model_key("codex:gpt-5:mini"), Some(("codex", "gpt-5:mini")));
        assert_eq!(split_model_key(":x"), None);
        assert_eq!(split_model_key("codex:"), None);
    }

    #[test]
    fn outcomes_and_statuses_have_the_originals_words_and_tones() {
        use ScheduledTaskWebhookDeliveryOutcome::*;
        let all = [(Accepted, "Ran", Tone::Success), (DispatchFailed, "Run failed", Tone::Error), (RejectedSignature, "Bad signature", Tone::Error), (Disabled, "Task paused", Tone::Warning), (RateLimited, "Rate limited", Tone::Warning), (Expired, "Too old", Tone::Warning)];
        for (outcome, label, tone) in all {
            assert_eq!((delivery_outcome_label(outcome), delivery_tone(outcome)), (label, tone));
        }
        assert_eq!(status_tone(ScheduledTaskRunStatus::Never), Tone::Outline);
        assert_eq!(status_tone(ScheduledTaskRunStatus::Running), Tone::Info);
    }
}
