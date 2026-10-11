//! Settling a thread: what writing to a parked thread does, and the sweep that settles a thread nobody
//! has touched (`ThreadSettlementService`).
//!
//! A message to a settled or snoozed thread brings it back first (`unpark`). The sweep looks at idle threads
//! once a minute and whenever a pull request changes: a thread whose pull requests are all merged or closed
//! settles at the time of its last activity, and a thread with no recent activity settles after the days the
//! settings name. It dispatches `thread.auto-settle` with the snapshot it read, which the planner refuses if the
//! thread changed since. Not ported: the lookup of a pull request by the thread's branch for a thread that
//! linked none, and the settle action a project may run.
use crate::{
    execution::{ExecutionService, at, checked, event},
    persistence::Event,
    thread::{ThreadService, shell},
    usage_limits,
};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// `QUEUED_TURN_START_GRACE_MS`.
pub(crate) const QUEUED_TURN_START_GRACE_MS: i64 = 2 * 60 * 1000;
/// How often the sweep looks, as the original's scheduler does.
const SWEEP_INTERVAL_MS: i64 = 60 * 1000;

fn millis(value: &Value) -> Option<i64> {
    value.as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map(|time| time.timestamp_millis())
}
fn latest(values: &[Option<i64>]) -> Option<i64> {
    values.iter().flatten().max().copied()
}

// ---- a message to a parked thread ------------------------------------------------------------------

/// The thread a message finds settled or snoozed is brought back before the message is planned: the
/// projection the message is planned against, and the events that say so.
pub(crate) fn unpark(command_id: &str, projection: &Value, now: DateTime<Utc>) -> Option<(Value, Vec<Event>)> {
    let thread = &projection["thread"];
    if thread["settledOverride"].is_null() && thread["snoozedUntil"].is_null() {
        return None;
    }
    let thread_id = thread["id"].as_str()?;
    let stamp = at(now);
    let mut adjusted = projection.clone();
    let mut events = vec![];
    if !thread["settledOverride"].is_null() {
        let mut next = adjusted["thread"].clone();
        next["settledOverride"] = Value::Null;
        next["settledAt"] = Value::Null;
        next["unsettledAt"] = if thread["settledOverride"] == "active" { thread.get("unsettledAt").cloned().unwrap_or(Value::Null) } else { json!(stamp) };
        next["updatedAt"] = json!(stamp);
        events.push(("thread.unsettled", next.clone()));
        adjusted["thread"] = next;
    }
    if !adjusted["thread"]["snoozedUntil"].is_null() {
        let mut next = adjusted["thread"].clone();
        next["snoozedUntil"] = Value::Null;
        next["snoozedAt"] = Value::Null;
        next["updatedAt"] = json!(stamp);
        events.push(("thread.unsnoozed", next.clone()));
        adjusted["thread"] = next;
    }
    let events = events.into_iter().map(|(kind, payload)| checked::<t3_contracts::AppThread>(payload).ok().map(|payload| event(thread_id, command_id, kind, payload, now))).collect::<Option<Vec<_>>>()?;
    Some((adjusted, events))
}

// ---- which threads settle on their own ---------------------------------------------------------------

/// `threadHasQueuedTurnStart`: a recent user message stays queued until a run adopts its timestamp.
pub(crate) fn has_queued_turn_start(thread: &Value, now_ms: i64) -> bool {
    let Some(message_at) = millis(&thread["latestUserMessageAt"]) else { return false };
    if thread["status"] == "failed" {
        return false;
    }
    if (now_ms - message_at).abs() > QUEUED_TURN_START_GRACE_MS {
        return false;
    }
    if thread["latestRunId"].is_null() {
        return true;
    }
    ["latestRunRequestedAt", "latestRunStartedAt", "latestRunCompletedAt"].iter().all(|field| millis(&thread[*field]).is_none_or(|at| at < message_at))
}

/// `backgroundWorkHoldsCompletion`: work that will wake the agent holds a thread open; a dev server left running does not.
fn background_work_holds(tasks: &Value) -> bool {
    tasks.as_array().into_iter().flatten().any(|task| !matches!(task["kind"].as_str(), Some("command")))
}

/// `isAutoSettlementCandidate`: the cheap checks that run before any lookup.
pub(crate) fn is_candidate(thread: &Value, now_ms: i64) -> bool {
    if !thread["archivedAt"].is_null() || !thread["settledOverride"].is_null() || !thread["pinnedAt"].is_null() || !thread["autoSettleDisabledAt"].is_null() {
        return false;
    }
    // Blocked-on-you work must never park behind a settled override.
    if !thread["pendingRuntimeRequest"].is_null() || !thread["activityRunStatus"].is_null() || background_work_holds(&thread["pendingBackgroundTasks"]) || has_queued_turn_start(thread, now_ms) {
        return false;
    }
    let Some(snoozed_until) = millis(&thread["snoozedUntil"]).filter(|until| *until > now_ms) else { return true };
    // A snoozed thread that woke early (error or completed work) can settle; one still parked keeps its stronger statement.
    let (snoozed_at, completed_at) = (millis(&thread["snoozedAt"]), millis(&thread["latestRunCompletedAt"]));
    let _ = snoozed_until;
    let woke_on_error = thread["status"] == "failed" && (snoozed_at.is_none() || completed_at.zip(snoozed_at).is_some_and(|(completed, snoozed)| completed > snoozed));
    let woke_on_completion = snoozed_at.zip(completed_at).is_some_and(|(snoozed, completed)| completed > snoozed);
    woke_on_error || woke_on_completion
}

/// `pullRequestSettles`: a merged or closed pull request settles the thread unless the user wrote to it afterwards.
fn pull_request_settles(thread: &Value, pull_request: &Value, on_merge: bool) -> bool {
    let state = pull_request["state"].as_str();
    if state != Some("closed") && (state != Some("merged") || !on_merge) {
        return false;
    }
    let terminal = if state == Some("merged") { &pull_request["mergedAt"] } else { &pull_request["closedAt"] };
    let Some(terminal_at) = millis(terminal) else { return false };
    let Some(anchor) = latest(&[millis(&thread["createdAt"]), millis(&thread["latestUserAuthoredMessageAt"])]) else { return false };
    terminal_at >= anchor
}

/// `resolveAutoSettlementAt`: when a candidate settles, as the time of its last activity, or none.
pub(crate) fn settlement_at(thread: &Value, pull_request: Option<&Value>, now_ms: i64, after_days: Option<i64>, on_merge: bool) -> Option<i64> {
    let links: Vec<&Value> = thread["pullRequests"].as_array().into_iter().flatten().filter(|link| link["source"] != "stack-dismissed").collect();
    if links.iter().any(|link| link["snapshot"].is_null() || link["snapshot"]["state"] == "open") {
        return None;
    }
    let mut pull_request = pull_request.cloned();
    if !links.is_empty() {
        let terminal_at = |link: &Value| {
            let snapshot = &link["snapshot"];
            let value = if snapshot["state"] == "merged" { &snapshot["mergedAt"] } else { &snapshot["closedAt"] };
            value.as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map_or(i64::MIN, |time| time.timestamp_millis())
        };
        let newest = links.iter().copied().reduce(|current, candidate| if terminal_at(candidate) > terminal_at(current) { candidate } else { current })?;
        pull_request = (!newest["snapshot"].is_null()).then(|| json!({"state":newest["snapshot"]["state"],"mergedAt":newest["snapshot"].get("mergedAt").cloned().unwrap_or(Value::Null),"closedAt":newest["snapshot"].get("closedAt").cloned().unwrap_or(Value::Null)}));
    }
    if !is_candidate(thread, now_ms) {
        return None;
    }
    let activity = latest(&[millis(&thread["latestUserMessageAt"]), millis(&thread["latestRunRequestedAt"]), millis(&thread["latestRunStartedAt"]), millis(&thread["latestRunCompletedAt"])]);
    if pull_request.as_ref().is_some_and(|pull_request| pull_request_settles(thread, pull_request, on_merge)) {
        return Some(activity.or_else(|| millis(&thread["createdAt"]))?);
    }
    let (days, activity) = (after_days?, activity?);
    (activity < now_ms - days * DAY_MS).then_some(activity)
}

/// What the sweep reads of a thread: its shell, the latest run's times, and the user's last own message.
pub(crate) fn candidate(projection: &Value) -> Value {
    let mut thread = shell(projection);
    let (run, _, _, _) = usage_limits::shell_fields(projection);
    for (field, source) in [("latestRunRequestedAt", "requestedAt"), ("latestRunStartedAt", "startedAt"), ("latestRunCompletedAt", "completedAt")] {
        thread[field] = run.map_or(Value::Null, |run| run[source].clone());
    }
    thread["latestRunId"] = run.map_or(Value::Null, |run| run["id"].clone());
    thread["status"] = run.map_or(json!("idle"), |run| run["status"].clone());
    thread["latestUserAuthoredMessageAt"] = projection["messages"].as_array().into_iter().flatten().filter(|message| message["role"] == "user" && message["createdBy"] == "user").map(|message| message["updatedAt"].clone()).max_by(|left, right| left.as_str().cmp(&right.as_str())).unwrap_or(Value::Null);
    thread
}

fn settle_one(execution: &ExecutionService, projection: &Value, now_ms: i64) {
    let settings = serde_json::to_value(execution.providers.settings()).unwrap_or(Value::Null);
    let project = projection["thread"]["projectId"].as_str().unwrap_or_default();
    let overrides = &settings["projectSettingsOverrides"][project];
    let after_days = if overrides.get("sidebarAutoSettleAfterDays").is_some() { &overrides["sidebarAutoSettleAfterDays"] } else { &settings["sidebarAutoSettleAfterDays"] };
    let on_merge = overrides["sidebarAutoSettleOnMerge"].as_bool().unwrap_or(settings["sidebarAutoSettleOnMerge"] == true);
    let thread = candidate(projection);
    let Some(settled_at) = settlement_at(&thread, None, now_ms, after_days.as_i64(), on_merge) else { return };
    let (thread_id, stamp) = (thread["id"].as_str().unwrap_or_default(), Utc.timestamp_millis_opt(settled_at).single().map(crate::execution::at));
    let command = json!({"type":"thread.auto-settle","commandId":format!("server:auto-settle:{thread_id}:{}",uuid::Uuid::new_v4()),"threadId":thread_id,"snapshotAt":thread["updatedAt"],"settledAt":stamp});
    let at = Utc.timestamp_millis_opt(now_ms).single().unwrap_or_else(Utc::now);
    if let Err(error) = ThreadService::new(execution.store.clone()).dispatch(&command, at) {
        tracing::warn!(thread = thread_id, %error, "automatic thread settlement skipped");
    }
}

/// One pass over every thread. Settings that name neither a merge rule nor an idle period settle nothing.
pub(crate) fn sweep(execution: &ExecutionService, now_ms: i64) {
    let settings = serde_json::to_value(execution.providers.settings()).unwrap_or(Value::Null);
    let configured = settings["sidebarAutoSettleOnMerge"] == true || !settings["sidebarAutoSettleAfterDays"].is_null() || settings["projectSettingsOverrides"].as_object().into_iter().flatten().any(|(_, entry)| entry["sidebarAutoSettleOnMerge"] == true || entry["sidebarAutoSettleAfterDays"].is_number());
    if !configured {
        return;
    }
    for projection in execution.store.projections("thread").unwrap_or_default() {
        if projection["thread"]["deletedAt"].is_null() {
            settle_one(execution, &projection, now_ms);
        }
    }
}

/// Sweeps every minute of the clock idle release runs on, so a test moves time.
pub(crate) fn start(execution: &ExecutionService) {
    let execution = execution.clone();
    tokio::spawn(async move {
        let clock = execution.providers.idle_release().clock();
        let mut due = clock.now_ms() + SWEEP_INTERVAL_MS;
        loop {
            clock.sleep_until(due).await;
            let (handler, now) = (execution.clone(), clock.now_ms());
            // The next pass is an interval after the reading this one acted on, not after it finished: a clock that moved
            // during the pass is then already past the next one, which runs at once instead of an interval later.
            due = now + SWEEP_INTERVAL_MS;
            let _ = tokio::task::spawn_blocking(move || sweep(&handler, now)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(overrides: Value) -> Value {
        let mut thread = json!({"id":"t","projectId":"p","createdAt":"2026-01-01T00:00:00.000Z","archivedAt":null,"settledOverride":null,"pinnedAt":null,"autoSettleDisabledAt":null,"pendingRuntimeRequest":null,"activityRunStatus":null,"pendingBackgroundTasks":[],
            "snoozedUntil":null,"snoozedAt":null,"latestRunId":"r","latestRunRequestedAt":"2026-01-02T00:00:00.000Z","latestRunStartedAt":"2026-01-02T00:00:01.000Z","latestRunCompletedAt":"2026-01-02T00:10:00.000Z","latestUserMessageAt":"2026-01-02T00:00:00.000Z","latestUserAuthoredMessageAt":"2026-01-02T00:00:00.000Z","status":"completed","pullRequests":[]});
        for (key, value) in overrides.as_object().unwrap() {
            thread[key] = value.clone();
        }
        thread
    }

    /// The original's decisions over the threads the generator scripted.
    #[test]
    fn the_settlement_decisions_match_the_original() {
        let mut threads: Vec<Value> = vec![];
        let (mut queued, mut candidates, mut settles) = (0, 0, 0);
        for line in include_str!("../tests/fixtures/thread-settlement.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["kind"] == "threads" {
                threads = row["threads"].as_array().unwrap().clone();
                continue;
            }
            let (thread, now) = (&threads[row["thread"].as_u64().unwrap() as usize], row["nowMs"].as_i64().unwrap());
            match row["kind"].as_str().unwrap() {
                "queued" => {
                    assert_eq!(json!(has_queued_turn_start(thread, now)), row["output"], "{row}");
                    queued += 1;
                }
                "candidate" => {
                    assert_eq!(json!(is_candidate(thread, now)), row["output"], "{row}");
                    candidates += 1;
                }
                _ => {
                    let pull_request = (!row["pullRequest"].is_null()).then(|| &row["pullRequest"]);
                    let actual = settlement_at(thread, pull_request, now, row["afterDays"].as_i64(), row["onMerge"] == true);
                    assert_eq!(actual.map_or(Value::Null, |at| json!(at)), row["output"], "{row}");
                    settles += 1;
                }
            }
        }
        assert_eq!((queued, candidates, settles), (128, 128, 5120));
    }

    #[test]
    fn an_idle_thread_settles_at_its_last_activity_after_the_configured_days_and_a_busy_one_does_not() {
        let now = millis(&json!("2026-01-10T00:00:00.000Z")).unwrap();
        let activity = millis(&json!("2026-01-02T00:10:00.000Z"));
        assert_eq!(settlement_at(&thread(json!({})), None, now, Some(3), true), activity);
        assert_eq!(settlement_at(&thread(json!({})), None, now, Some(30), true), None);
        assert_eq!(settlement_at(&thread(json!({})), None, now, None, true), None);
        for busy in [json!({"pinnedAt":"2026-01-03T00:00:00.000Z"}), json!({"autoSettleDisabledAt":"2026-01-03T00:00:00.000Z"}), json!({"activityRunStatus":"running"}), json!({"pendingRuntimeRequest":{"id":"q"}}), json!({"pendingBackgroundTasks":[{"kind":"subagent"}]}), json!({"settledOverride":"active"})] {
            assert_eq!(settlement_at(&thread(busy.clone()), None, now, Some(3), true), None, "{busy}");
        }
        // A dev server left running is not work the agent waits for.
        assert_eq!(settlement_at(&thread(json!({"pendingBackgroundTasks":[{"kind":"command"}]})), None, now, Some(3), true), activity);
    }

    #[test]
    fn a_merged_pull_request_settles_the_thread_unless_the_user_wrote_after_it() {
        let now = millis(&json!("2026-01-10T00:00:00.000Z")).unwrap();
        let link = |state: &str, merged: &str| json!({"source":"manual","snapshot":{"state":state,"mergedAt":merged,"closedAt":null}});
        let merged = thread(json!({"pullRequests":[link("merged", "2026-01-03T00:00:00.000Z")]}));
        assert_eq!(settlement_at(&merged, None, now, None, true), millis(&json!("2026-01-02T00:10:00.000Z")));
        assert_eq!(settlement_at(&merged, None, now, None, false), None);
        let written_after = thread(json!({"pullRequests":[link("merged", "2026-01-01T12:00:00.000Z")]}));
        assert_eq!(settlement_at(&written_after, None, now, None, true), None);
        let open = thread(json!({"pullRequests":[link("open", "2026-01-03T00:00:00.000Z")]}));
        assert_eq!(settlement_at(&open, None, now, Some(1), true), None);
    }

    #[test]
    fn a_recent_message_no_run_has_adopted_yet_holds_the_thread() {
        let message = millis(&json!("2026-01-10T00:00:30.000Z")).unwrap();
        let waiting = thread(json!({"latestUserMessageAt":"2026-01-10T00:00:30.000Z"}));
        assert!(has_queued_turn_start(&waiting, message + 1000));
        assert!(!has_queued_turn_start(&waiting, message + QUEUED_TURN_START_GRACE_MS + 1));
        assert!(!has_queued_turn_start(&thread(json!({"latestUserMessageAt":"2026-01-10T00:00:30.000Z","status":"failed"})), message + 1000));
    }
}
