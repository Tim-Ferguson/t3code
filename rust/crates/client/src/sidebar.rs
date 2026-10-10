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
/// `threadRaisedHandWhileSnoozed`: something outranks the snooze, so the thread leaves the shelf
/// early. The agent is blocked on the user, the run failed after the snooze was set, or a run
/// completed after it.
pub fn raised_hand(thread: &ThreadShell) -> bool {
    if has_pending_approval(thread) || has_pending_input(thread) {
        return true;
    }
    let snoozed_at = thread.snoozed_at.flatten().map(|at| at.0);
    if runtime_status(thread) == Some("failed") && snoozed_at.is_none_or(|at| thread.updated_at.0 > at) {
        return true;
    }
    snoozed_at.zip(run_completed_at(thread)).is_some_and(|(snoozed, done)| done > snoozed)
}
/// When the latest run completed, if its status reads "completed" (a idle thread's does).
fn run_completed_at(thread: &ThreadShell) -> Option<DateTime<Utc>> {
    matches!(thread.status, ThreadStatus::Idle | ThreadStatus::Completed).then(|| completed_at(thread)).flatten()
}
/// `effectiveSnoozed`: the wake time is ahead and nothing has raised its hand. A wake time that has
/// passed is no longer a snooze (nothing fires when it does; the row simply stops classifying).
pub fn snoozed_until(thread: &ThreadShell, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    thread.snoozed_until.flatten().map(|at| at.0).filter(|wake| *wake > now).filter(|_| !raised_hand(thread))
}
/// `threadWokeAt` and the dismissal rule of `isWoke` (`Sidebar.tsx`): when the thread woke, or
/// none while it is still snoozed or never was. A timer wake reports the wake time; an early wake
/// reports the event that raised the hand. The mark stays until the thread is visited after that
/// time, or it is settled.
pub fn woke_at(thread: &ThreadShell, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let wake = woke_time(thread, now)?;
    let visited_since = thread.last_visited_at.flatten().is_some_and(|visited| visited.0 >= wake);
    (!visited_since && thread.settled_override != Some(t3_contracts::SettledOverride::Settled)).then_some(wake)
}
/// `threadWokeAt` alone.
pub fn woke_time(thread: &ThreadShell, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let until = thread.snoozed_until.flatten()?.0;
    if raised_hand(thread) {
        let snoozed_at = thread.snoozed_at.flatten().map(|at| at.0);
        if let Some(done) = snoozed_at.zip(run_completed_at(thread)).filter(|(snoozed, done)| done > snoozed).map(|(_, done)| done) {
            return Some(done);
        }
        return if runtime_status(thread).is_some() { Some(thread.updated_at.0) } else { snoozed_at };
    }
    (until <= now).then_some(until)
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

// ---- sections and ordering ----------------------------------------------------------------------
// Port of `resolveSidebarThreadSection` (`Sidebar.logic.ts`) and the sorts of `threadSort.ts`. The
// original's beta "Working" shelf (`sidebarWorkingShelfEnabled`, off unless the user turns it on)
// is not ported: threads that would fold into it stay in Active, which is what the original shows
// with the setting off.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Pinned,
    Active,
    Snoozed,
    Settled,
}
impl Section {
    pub fn as_str(self) -> &'static str {
        match self {
            Section::Pinned => "pinned",
            Section::Active => "active",
            Section::Snoozed => "snoozed",
            Section::Settled => "settled",
        }
    }
    pub fn parse(text: &str) -> Option<Section> {
        Some(match text {
            "pinned" => Section::Pinned,
            "active" => Section::Active,
            "snoozed" => Section::Snoozed,
            "settled" => Section::Settled,
            _ => return None,
        })
    }
}

/// What the server can do with the sections. A server without settlement never classifies a thread
/// as settled (the user could neither unsettle nor pin it), and one without snooze never as snoozed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Support {
    pub settlement: bool,
    pub snooze: bool,
}

/// The shelf a thread belongs to. Snooze is temporary and wins until its wake time; settlement
/// then wins over a stale pin.
pub fn section_of(thread: &ThreadShell, now: DateTime<Utc>, support: Support) -> Section {
    if support.snooze && snoozed_until(thread, now).is_some() {
        Section::Snoozed
    } else if support.settlement && thread.settled_override == Some(t3_contracts::SettledOverride::Settled) {
        Section::Settled
    } else if is_pinned(thread) {
        Section::Pinned
    } else {
        Section::Active
    }
}

fn millis(at: Option<DateTime<Utc>>) -> Option<i64> {
    at.map(|at| at.timestamp_millis())
}
/// `resolveSettledThreadTimestamp`, in milliseconds: when it was settled, else the latest
/// message or run stamp, else when it last changed.
pub fn settled_timestamp(thread: &ThreadShell) -> i64 {
    if let Some(at) = thread.settled_at {
        return at.timestamp_millis();
    }
    let run = thread.latest_run_id.is_some();
    let stamp = |field: Option<Option<t3_contracts::UtcDateTime>>| millis(field.flatten().map(|at| at.0)).filter(|_| run);
    let completed = if !run { None } else { millis(completed_at(thread)) };
    [millis(thread.latest_user_message_at.map(|at| at.0)), stamp(thread.latest_run_requested_at), stamp(thread.latest_run_started_at), completed]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_else(|| thread.updated_at.timestamp_millis())
}
/// `sortSettledThreads`: newest end first.
pub fn sort_settled(threads: &mut [&ThreadShell]) {
    threads.sort_by(|left, right| settled_timestamp(right).cmp(&settled_timestamp(left)).then_with(|| left.id.as_str().cmp(right.id.as_str())));
}
/// `sortPinnedThreadsByOrderKey`: the arranged pins by key, then the keyless newest first.
pub fn sort_pinned(threads: &mut [&ThreadShell]) {
    let key = |thread: &ThreadShell| thread.pin_order_key.clone().flatten().map(|key| key.to_string());
    threads.sort_by(|left, right| match (key(left), key(right)) {
        (Some(a), Some(b)) => a.cmp(&b).then_with(|| left.id.as_str().cmp(right.id.as_str())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.created_at.timestamp_millis().cmp(&left.created_at.timestamp_millis()).then_with(|| left.id.as_str().cmp(right.id.as_str())),
    });
}
/// `activeThreadAnchorTimestampMs`: creation, or the last time it came back from settled.
fn active_anchor(thread: &ThreadShell) -> i64 {
    thread.created_at.timestamp_millis().max(thread.unsettled_at.flatten().map_or(0, |at| at.timestamp_millis()))
}
/// `sortActiveThreadsByOrderKey`: new and reopened threads first (newest anchor first), then the
/// arranged ones by key.
pub fn sort_active(threads: &mut [&ThreadShell]) {
    let key = |thread: &ThreadShell| thread.active_order_key.clone().flatten().map(|key| key.to_string());
    threads.sort_by(|left, right| match (key(left), key(right)) {
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => a.cmp(&b).then_with(|| left.id.as_str().cmp(right.id.as_str())),
        (None, None) => active_anchor(right).cmp(&active_anchor(left)).then_with(|| left.id.as_str().cmp(right.id.as_str())),
    });
}

/// A project's threads, split into the sections the sidebar lists them in.
#[derive(Debug, Default)]
pub struct Sections<'a> {
    pub pinned: Vec<&'a ThreadShell>,
    pub active: Vec<&'a ThreadShell>,
    /// Soonest wake first: what comes back next.
    pub snoozed: Vec<&'a ThreadShell>,
    pub settled: Vec<&'a ThreadShell>,
}
impl<'a> Sections<'a> {
    pub fn of(threads: impl IntoIterator<Item = &'a ThreadShell>, now: DateTime<Utc>, support: Support) -> Self {
        let mut sections = Sections::default();
        for thread in threads {
            match section_of(thread, now, support) {
                Section::Pinned => sections.pinned.push(thread),
                Section::Active => sections.active.push(thread),
                Section::Snoozed => sections.snoozed.push(thread),
                Section::Settled => sections.settled.push(thread),
            }
        }
        sort_pinned(&mut sections.pinned);
        sort_active(&mut sections.active);
        sections.snoozed.sort_by_key(|thread| thread.snoozed_until.flatten().map_or(0, |at| at.timestamp_millis()));
        sort_settled(&mut sections.settled);
        sections
    }
    pub fn section(&self, section: Section) -> &[&'a ThreadShell] {
        match section {
            Section::Pinned => &self.pinned,
            Section::Active => &self.active,
            Section::Snoozed => &self.snoozed,
            Section::Settled => &self.settled,
        }
    }
    pub fn section_mut(&mut self, section: Section) -> &mut Vec<&'a ThreadShell> {
        match section {
            Section::Pinned => &mut self.pinned,
            Section::Active => &mut self.active,
            Section::Snoozed => &mut self.snoozed,
            Section::Settled => &mut self.settled,
        }
    }
}

/// The settled shelf lists this many rows, then this many more per "Show more".
pub const SETTLED_INITIAL: usize = 10;
pub const SETTLED_PAGE: usize = 25;
/// The rows of a collapsed or paged shelf: the thread that is open never hides behind it.
/// `limit` is how many rows the shelf shows when expanded.
pub fn shelf_rows<'a>(shelf: &[&'a ThreadShell], expanded: bool, limit: usize, open: Option<&str>) -> Vec<&'a ThreadShell> {
    let mut rows: Vec<&ThreadShell> = if expanded { shelf.iter().copied().take(limit).collect() } else { Vec::new() };
    if let Some(open) = open.filter(|open| !rows.iter().any(|row| row.id.as_str() == *open)) {
        rows.extend(shelf.iter().copied().find(|row| row.id.as_str() == open));
    }
    rows
}

// ---- dropping a row -------------------------------------------------------------------------------
// Port of `resolveSidebarDropVerb` and `planSidebarThreadDrop` (`Sidebar.logic.ts`).

/// What dropping on a section does, for the badge on the lifted row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropVerb {
    Pin,
    Unpin,
    Settle,
    Unsettle,
    Wake,
}
impl DropVerb {
    pub fn label(self) -> &'static str {
        match self {
            DropVerb::Pin => "Pin",
            DropVerb::Unpin => "Unpin",
            DropVerb::Settle => "Settle",
            DropVerb::Unsettle => "Unsettle",
            DropVerb::Wake => "Wake",
        }
    }
}
/// `resolveSidebarDropVerb`; none while reordering inside one section and for the shelves that
/// cannot be dropped on.
pub fn drop_verb(from: Section, to: Option<Section>) -> Option<DropVerb> {
    let to = to?;
    if to == from || to == Section::Snoozed {
        return None;
    }
    Some(match (to, from) {
        (Section::Pinned, _) => DropVerb::Pin,
        (Section::Settled, _) => DropVerb::Settle,
        (_, Section::Pinned) => DropVerb::Unpin,
        (_, Section::Settled) => DropVerb::Unsettle,
        _ => DropVerb::Wake,
    })
}

/// The order of the section a drop lands in, with the thread moved into place.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTarget {
    pub section: Section,
    pub pinned_order: Vec<String>,
    pub active_order: Vec<String>,
}
/// Where the row lands: `section`, in front of `before` (or last when none).
pub fn drop_target(sections: &Sections, moved: &str, section: Section, before: Option<&str>) -> Option<DropTarget> {
    if section == Section::Snoozed {
        return None;
    }
    let ids = |list: &[&ThreadShell]| -> Vec<String> { list.iter().map(|thread| thread.id.to_string()).filter(|id| id != moved).collect() };
    let (mut pinned_order, mut active_order) = (ids(&sections.pinned), ids(&sections.active));
    let list = match section {
        Section::Pinned => &mut pinned_order,
        Section::Active => &mut active_order,
        _ => return Some(DropTarget { section, pinned_order: ids(&sections.pinned), active_order: ids(&sections.active) }),
    };
    let at = before.and_then(|before| list.iter().position(|id| id == before)).unwrap_or(list.len());
    list.insert(at, moved.to_owned());
    Some(DropTarget { section, pinned_order, active_order })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub id: String,
    pub order_key: String,
}
#[derive(Debug, Clone, PartialEq)]
pub enum DropPlan {
    None,
    /// Within the pinned block: the key writes.
    ReorderPinned { order: Vec<String>, assignments: Vec<Assignment> },
    /// From another section into the pinned block. A fresh pin takes `order_key` on the pin
    /// command; `extra` land afterwards.
    Pin { order: Vec<String>, order_key: Option<String>, extra: Vec<Assignment> },
    MoveActive { order: Vec<String>, assignments: Vec<Assignment>, unpin: bool, unsettle: bool, unsnooze: bool },
    Settle,
}

/// What the drop of a thread asks the server for. `pinned_keys` and `active_keys` hold every
/// thread's key, hidden ones too, so a new key never lands on a taken slot. `pin_writable` and
/// `active_writable` are the threads whose server can store an order.
pub struct DropInput<'a> {
    pub moved: &'a str,
    pub from: Section,
    pub moved_pinned: bool,
    pub moved_settled: bool,
    pub supports_settlement: bool,
    pub target: &'a DropTarget,
    pub pinned_order: &'a [String],
    pub pinned_keys: &'a std::collections::BTreeMap<String, Option<String>>,
    pub pin_writable: Option<&'a std::collections::BTreeSet<String>>,
    pub active_order: &'a [String],
    pub active_keys: &'a std::collections::BTreeMap<String, Option<String>>,
    pub active_writable: Option<&'a std::collections::BTreeSet<String>>,
}
pub fn plan_drop(input: &DropInput) -> DropPlan {
    use std::collections::{BTreeMap, BTreeSet};
    if !input.supports_settlement && (input.target.section == Section::Settled || input.moved_settled) {
        return DropPlan::None;
    }
    // Rows whose server cannot store an order are never written; keyless ones leave the plan, keyed ones stay as bounds.
    let arrange = |order: &[String], keys: &BTreeMap<String, Option<String>>, writable: Option<&BTreeSet<String>>| -> Option<Vec<Assignment>> {
        let to_assignments = |writes: Vec<(String, String)>| writes.into_iter().map(|(id, order_key)| Assignment { id, order_key }).collect::<Vec<_>>();
        let Some(writable) = writable else { return Some(to_assignments(crate::pin_order::plan_reorder(order, keys, input.moved))) };
        if !writable.contains(input.moved) {
            return None;
        }
        let usable: Vec<String> = order.iter().filter(|id| writable.contains(*id) || keys.get(*id).cloned().flatten().is_some()).cloned().collect();
        let assignments = to_assignments(crate::pin_order::plan_reorder(&usable, keys, input.moved));
        assignments.iter().all(|assignment| writable.contains(&assignment.id)).then_some(assignments)
    };
    let same = |order: &[String], current: &[String]| order.len() == current.len() && order.iter().zip(current).all(|(a, b)| a == b);
    match input.target.section {
        Section::Active => {
            let order = &input.target.active_order;
            if input.from == Section::Active && same(order, input.active_order) {
                return DropPlan::None;
            }
            let Some(assignments) = arrange(order, input.active_keys, input.active_writable) else { return DropPlan::None };
            DropPlan::MoveActive { order: order.clone(), assignments, unpin: input.moved_pinned, unsettle: input.moved_settled, unsnooze: input.from == Section::Snoozed }
        }
        Section::Settled => if input.from == Section::Settled { DropPlan::None } else { DropPlan::Settle },
        Section::Pinned => {
            let order = &input.target.pinned_order;
            if input.from == Section::Pinned && same(order, input.pinned_order) {
                return DropPlan::None;
            }
            let Some(assignments) = arrange(order, input.pinned_keys, input.pin_writable) else { return DropPlan::None };
            if input.from == Section::Pinned {
                return if assignments.is_empty() { DropPlan::None } else { DropPlan::ReorderPinned { order: order.clone(), assignments } };
            }
            DropPlan::Pin {
                order: order.clone(),
                order_key: assignments.iter().find(|assignment| assignment.id == input.moved).map(|assignment| assignment.order_key.clone()),
                extra: if input.moved_pinned { assignments } else { assignments.into_iter().filter(|assignment| assignment.id != input.moved).collect() },
            }
        }
        Section::Snoozed => DropPlan::None,
    }
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
