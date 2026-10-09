//! What a thread row says: whether it is working, waiting on you or failed, whether it finished
//! while you were away, whether it is pinned or snoozed, and the wake times a snooze offers.
//! Port of `Sidebar.logic.ts` (`resolveSidebarThreadStatus`, `resolveThreadStatusPill`,
//! `hasUnseenCompletion`, `formatWorkingDurationLabel`), `shellRuntime` / `presentThreadShell`
//! (`state/models.ts`) and `resolveSnoozePresets` / `snoozeWakeLabel` (`state/threadSettled.ts`).
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use t3_contracts::{ActivityRunStatus, PendingBackgroundTask, ProviderFailureClass, RuntimeRequestKind, ThreadShell, ThreadStatus};


fn terminal(status: ThreadStatus) -> bool {
    matches!(status, ThreadStatus::Completed | ThreadStatus::Interrupted | ThreadStatus::Failed | ThreadStatus::Cancelled | ThreadStatus::RolledBack)
}
fn status_name(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Preparing => "preparing",
        ThreadStatus::Queued => "queued",
        ThreadStatus::Starting => "starting",
        ThreadStatus::Running => "running",
        ThreadStatus::Waiting => "waiting",
        ThreadStatus::Completed => "completed",
        ThreadStatus::Interrupted => "interrupted",
        ThreadStatus::Failed => "failed",
        ThreadStatus::Cancelled => "cancelled",
        ThreadStatus::RolledBack => "rolled_back",
    }
}

/// `backgroundWorkHoldsCompletion`: work that wakes the agent holds the run's completion, a command
/// left running does not.
pub fn background_holds_completion(tasks: &[PendingBackgroundTask]) -> bool {
    tasks.iter().any(|task| !matches!(task, PendingBackgroundTask::Command { .. }))
}

/// The status the row presents (`shellRuntime`), or none for a thread that never ran.
pub fn runtime_status(thread: &ThreadShell) -> Option<&str> {
    let park = background_holds_completion(&thread.pending_background_tasks) && thread.status != ThreadStatus::Failed;
    if thread.latest_run_id.is_none() && thread.active_provider_thread_id.is_none() && !park {
        return None;
    }
    let activity = thread.activity_run_status.flatten().map(|status| match status {
        ActivityRunStatus::Preparing => "preparing",
        ActivityRunStatus::Starting => "starting",
        ActivityRunStatus::Running => "running",
        ActivityRunStatus::Waiting => "waiting",
    });
    Some(if park { "idle" } else { activity.unwrap_or_else(|| status_name(thread.status)) })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Approval,
    Input,
    Working,
    Waiting,
    Failed,
    Limited,
    Ready,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Approval => "approval",
            Status::Input => "input",
            Status::Working => "working",
            Status::Waiting => "waiting",
            Status::Failed => "failed",
            Status::Limited => "limited",
            Status::Ready => "ready",
        }
    }
}
pub fn has_pending_approval(thread: &ThreadShell) -> bool {
    thread.pending_runtime_request.as_ref().is_some_and(|request| !matches!(request.kind, RuntimeRequestKind::UserInput | RuntimeRequestKind::AuthRefresh))
}
pub fn has_pending_input(thread: &ThreadShell) -> bool {
    thread.pending_runtime_request.as_ref().is_some_and(|request| request.kind == RuntimeRequestKind::UserInput)
}
/// `resolveSidebarThreadStatus`.
pub fn status(thread: &ThreadShell) -> Status {
    if has_pending_approval(thread) {
        return Status::Approval;
    }
    if has_pending_input(thread) {
        return Status::Input;
    }
    match runtime_status(thread) {
        Some("preparing" | "queued" | "starting" | "running" | "waiting") => Status::Working,
        Some("idle") => Status::Waiting,
        Some("failed") => if thread.last_error_class.flatten() == Some(ProviderFailureClass::UsageLimit) { Status::Limited } else { Status::Failed },
        _ => Status::Ready,
    }
}

/// When the latest run finished, as a shell lets a client tell (`presentThreadShell`).
fn completed_at(thread: &ThreadShell) -> Option<chrono::DateTime<Utc>> {
    thread.latest_run_id.as_ref()?;
    match thread.latest_run_completed_at {
        Some(completed) => completed.map(|at| at.0),
        None => (thread.status == ThreadStatus::Idle || terminal(thread.status)).then_some(thread.updated_at.0),
    }
}
/// `hasUnseenCompletion`: the latest run finished after the last time the thread was visited. A
/// server that does not track visits (`lastVisitedAt` absent) never reports it.
pub fn has_unseen_completion(thread: &ThreadShell) -> bool {
    match (completed_at(thread), thread.last_visited_at.flatten()) {
        (Some(completed), Some(visited)) => completed > visited.0,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Amber,
    Indigo,
    Sky,
    Muted,
    Violet,
    Emerald,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pill {
    pub label: &'static str,
    pub tone: Tone,
    /// Whether the dot says "in progress"; drawn still.
    pub busy: bool,
}
impl Pill {
    fn priority(self) -> f32 {
        match self.label {
            "Pending Approval" => 5.0,
            "Awaiting Input" => 4.0,
            "Working" | "Connecting" => 3.0,
            "Waiting" => 2.5,
            "Plan Ready" => 2.0,
            _ => 1.0,
        }
    }
}
/// `isLatestRunSettled` for the shell's latest run.
fn latest_run_settled(thread: &ThreadShell) -> bool {
    let Some(latest) = thread.latest_run_id.as_ref() else { return false };
    let status = if thread.status == ThreadStatus::Idle { ThreadStatus::Completed } else { thread.status };
    if matches!(status, ThreadStatus::Preparing | ThreadStatus::Queued | ThreadStatus::Starting | ThreadStatus::Running | ThreadStatus::Waiting) {
        return false;
    }
    thread.active_run_id.as_ref() != Some(latest)
}
/// `resolveThreadStatusPill`.
pub fn pill(thread: &ThreadShell) -> Option<Pill> {
    if has_pending_approval(thread) {
        return Some(Pill { label: "Pending Approval", tone: Tone::Amber, busy: false });
    }
    if has_pending_input(thread) {
        return Some(Pill { label: "Awaiting Input", tone: Tone::Indigo, busy: false });
    }
    match runtime_status(thread) {
        Some("running" | "waiting") => return Some(Pill { label: "Working", tone: Tone::Sky, busy: true }),
        Some("preparing" | "starting" | "queued") => return Some(Pill { label: "Connecting", tone: Tone::Sky, busy: true }),
        _ => {}
    }
    if background_holds_completion(&thread.pending_background_tasks) {
        return Some(Pill { label: "Waiting", tone: Tone::Muted, busy: false });
    }
    let plan_ready = !has_pending_input(thread) && serde_json::to_value(thread.interaction_mode).ok().as_ref().and_then(|mode| mode.as_str()) == Some("plan") && latest_run_settled(thread) && thread.has_actionable_proposed_plan;
    if plan_ready {
        return Some(Pill { label: "Plan Ready", tone: Tone::Violet, busy: false });
    }
    has_unseen_completion(thread).then_some(Pill { label: "Completed", tone: Tone::Emerald, busy: false })
}
/// `resolveProjectStatusIndicator`: the most urgent pill among a project's threads.
pub fn highest(pills: impl IntoIterator<Item = Option<Pill>>) -> Option<Pill> {
    pills.into_iter().flatten().fold(None, |best: Option<Pill>, next| match best {
        Some(best) if next.priority() <= best.priority() => Some(best),
        _ => Some(next),
    })
}
/// `formatWorkingDurationLabel`.
pub fn working_label(elapsed_ms: i64) -> String {
    let seconds = (elapsed_ms / 1000).max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 { format!("{minutes}m") } else { format!("{}h {}m", minutes / 60, minutes % 60) }
}

// ---- pinning, snoozing --------------------------------------------------------------------------

pub fn is_pinned(thread: &ThreadShell) -> bool {
    thread.pinned_at.flatten().is_some()
}
/// The pin order key; pinned threads list by it, and a pin the server gave none sorts by when it was made.
pub fn pin_key(thread: &ThreadShell) -> (String, i64) {
    (thread.pin_order_key.clone().flatten().map(|key| key.to_string()).unwrap_or_default(), thread.pinned_at.flatten().map_or(0, |at| at.timestamp_millis()))
}
/// The wake time of a thread that is still snoozed at `now`.
pub fn snoozed_until(thread: &ThreadShell, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    thread.snoozed_until.flatten().map(|at| at.0).filter(|wake| *wake > now)
}
/// `threadWokeAt` for a snooze that ended on its timer: when the thread woke, or none while it is
/// still snoozed or never was. The "Woke" mark stays until the thread is visited after that time,
/// or it is settled (`isWoke` of `Sidebar.tsx`). A thread that raised its hand early is not read.
pub fn woke_at(thread: &ThreadShell, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let wake = thread.snoozed_until.flatten().map(|at| at.0).filter(|wake| *wake <= now)?;
    let visited_since = thread.last_visited_at.flatten().is_some_and(|visited| visited.0 >= wake);
    (!visited_since && thread.settled_override != Some(t3_contracts::SettledOverride::Settled)).then_some(wake)
}
/// `snoozeWakeLabel`: "2h", "18h", "3d"; minutes round up so a snooze never reads "0m".
pub fn wake_label(until: &str, now: DateTime<Utc>) -> String {
    let Ok(wake) = DateTime::parse_from_rfc3339(until) else { return "now".into() };
    let remaining = (wake.with_timezone(&Utc) - now).num_milliseconds();
    const HOUR: i64 = 3_600_000;
    if remaining <= 0 {
        "now".into()
    } else if remaining < HOUR {
        format!("{}m", ((remaining + 59_999) / 60_000).max(1))
    } else if remaining < 24 * HOUR {
        format!("{}h", (remaining + HOUR - 1) / HOUR)
    } else {
        format!("{}d", (remaining + 24 * HOUR - 1) / (24 * HOUR))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnoozePreset {
    pub id: &'static str,
    pub label: &'static str,
    /// ISO wake time.
    pub until: String,
}
/// `resolveSnoozePresets`, for a viewer `offset_minutes` east of UTC. "This evening" appears only
/// while it is more than an hour away; "Next week" is dropped when it falls on "Tomorrow".
pub fn snooze_presets(now: DateTime<Utc>, offset_minutes: i32) -> Vec<SnoozePreset> {
    let shift = Duration::minutes(offset_minutes as i64);
    let local = now + shift;
    let at_hour = |day: DateTime<Utc>, hour: u32| Utc.with_ymd_and_hms(day.year(), day.month(), day.day(), hour, 0, 0).single().unwrap_or(day) - shift;
    let iso = |at: DateTime<Utc>| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut presets = vec![
        SnoozePreset { id: "hour", label: "In 1 hour", until: iso(now + Duration::hours(1)) },
        SnoozePreset { id: "three-hours", label: "In 3 hours", until: iso(now + Duration::hours(3)) },
    ];
    let evening = at_hour(local, 18);
    if (evening - now).num_milliseconds() > 3_600_000 {
        presets.push(SnoozePreset { id: "evening", label: "This evening", until: iso(evening) });
    }
    let tomorrow = at_hour(local + Duration::days(1), 9);
    presets.push(SnoozePreset { id: "tomorrow", label: "Tomorrow", until: iso(tomorrow) });
    let weekday = local.weekday().num_days_from_sunday() as i64;
    let to_monday = (1 - weekday + 7) % 7;
    let next_week = at_hour(local + Duration::days(if to_monday == 0 { 7 } else { to_monday }), 9);
    if next_week != tomorrow {
        presets.push(SnoozePreset { id: "next-week", label: "Next week", until: iso(next_week) });
    }
    let _ = local.hour();
    presets
}

#[cfg(test)]
mod woke_tests {
    use super::*;
    use serde_json::json;

    fn thread(extra: serde_json::Value) -> ThreadShell {
        let mut base = json!({"createdBy":"user","creationSource":"web","id":"t","projectId":"p","title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"t"},"forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle","pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,"hasActionableProposedPlan":false,"itemCount":1,"visibleItemCount":1,"pendingBackgroundTasks":[],"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,"settledOverride":null,"settledAt":null,"deletedAt":null});
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }
    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_thread_wakes_once_its_timer_has_passed_until_it_is_visited_or_settled() {
        let now = at("2026-01-02T12:00:00Z");
        // Still snoozed, or never snoozed: not woke.
        assert_eq!(woke_at(&thread(json!({"snoozedUntil":"2026-01-02T13:00:00Z"})), now), None);
        assert_eq!(woke_at(&thread(json!({})), now), None);
        // The timer passed.
        let woke = thread(json!({"snoozedUntil":"2026-01-02T09:00:00Z"}));
        assert_eq!(woke_at(&woke, now), Some(at("2026-01-02T09:00:00Z")));
        // A visit before the wake does not clear it; one after does.
        assert!(woke_at(&thread(json!({"snoozedUntil":"2026-01-02T09:00:00Z","lastVisitedAt":"2026-01-02T08:00:00Z"})), now).is_some());
        assert_eq!(woke_at(&thread(json!({"snoozedUntil":"2026-01-02T09:00:00Z","lastVisitedAt":"2026-01-02T09:30:00Z"})), now), None);
        // Settling the thread ends it.
        assert_eq!(woke_at(&thread(json!({"snoozedUntil":"2026-01-02T09:00:00Z","settledOverride":"settled"})), now), None);
    }
}
