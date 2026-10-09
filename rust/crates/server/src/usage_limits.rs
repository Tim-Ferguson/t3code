//! Usage-limit recovery: the thread state that says a provider limit stopped the thread
//! (`orchestrationV2ThreadError`), the recovery a user or the server arms for it, and the sweep that
//! resumes or snoozes limited threads (`UsageLimitRecoveryWorker`).
//!
//! A run stopped by a limit records a failure with the time the limit resets. The thread is then
//! "blocked": queued messages wait, and `thread.metadata.update` with `limitRecovery` arms an automatic
//! resume (and optionally a snooze until the reset). When the reset time passes the sweep sends
//! "Continue where you left off." as a `message.dispatch` carrying `usageLimitContinuationOfRunId`, which
//! the planner honours only while the recovery it names is still current.
use crate::{
    execution::{ExecutionService, at, event},
    message_queue::{latest_executed, root_failure},
    persistence::{Decision, Store, StoreError},
    thread::ThreadService,
};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}
fn millis(text: &Value) -> Option<i64> {
    text.as_str().and_then(|text| DateTime::parse_from_rfc3339(text).ok()).map(|time| time.timestamp_millis())
}

/// The newest provider session of the thread's instance, whose `lastError` supersedes a run's own
/// classification when it differs from the run's message.
pub(crate) fn session_error(projection: &Value) -> Option<String> {
    rows(projection, "providerSessions")
        .iter()
        .filter(|session| session["providerInstanceId"] == projection["thread"]["providerInstanceId"])
        .max_by(|left, right| left["updatedAt"].as_str().cmp(&right["updatedAt"].as_str()))
        .and_then(|session| session["lastError"].as_str().map(str::to_owned))
}

/// `threadErrorSummary`: `(lastError, lastErrorClass, usageLimitResetAt)`.
pub(crate) fn error_summary(failure: Option<&Value>, session_error: Option<&str>) -> (Value, Value, Value) {
    let message = failure.and_then(|failure| failure["message"].as_str());
    let superseded = session_error.is_some_and(|error| Some(error) != message);
    let current = failure.filter(|_| !superseded);
    let reset = current.filter(|failure| failure["class"] == "usage_limit").map_or(Value::Null, |failure| failure.get("resetAt").cloned().unwrap_or(Value::Null));
    let last_error = session_error.or(message).map_or(Value::Null, |text| json!(text));
    let class = if superseded { Value::Null } else { failure.map_or(Value::Null, |failure| failure["class"].clone()) };
    (last_error, class, reset)
}

/// `usageLimitBlockedRun`: the latest run that started, when it failed on a limit.
pub(crate) fn blocked_run<'a>(projection: &'a Value, session_error: Option<&str>) -> Option<&'a Value> {
    let executed = latest_executed(projection)?;
    if executed["status"] != "failed" {
        return None;
    }
    let (_, class, _) = error_summary(root_failure(projection, executed), session_error);
    (class == "usage_limit").then_some(executed)
}

/// `latestUnheldRun`: the newest run that is not waiting in a held queue.
pub(crate) fn latest_unheld_run(projection: &Value) -> Option<&Value> {
    rows(projection, "runs").iter().filter(|run| !(run["status"] == "queued" && run["queueHeld"] == true)).max_by_key(|run| run["ordinal"].as_u64().unwrap_or(0))
}

/// `usageLimitRunPresentedAsLatest`: that limited run, when newer runs were queued or cancelled before starting.
pub(crate) fn presented_as_latest<'a>(projection: &'a Value, session_error: Option<&str>) -> Option<&'a Value> {
    let blocked = blocked_run(projection, session_error)?;
    rows(projection, "runs").iter().any(|run| run["ordinal"].as_u64() > blocked["ordinal"].as_u64()).then_some(blocked)
}

/// What a thread shell says about the thread's last run and why it failed.
pub(crate) fn shell_fields(projection: &Value) -> (Option<&Value>, Value, Value, Value) {
    let error = session_error(projection);
    let latest = presented_as_latest(projection, error.as_deref()).or_else(|| latest_unheld_run(projection));
    let failure = latest.and_then(|run| root_failure(projection, run));
    let (last_error, class, reset) = error_summary(failure, error.as_deref());
    (latest, last_error, class, reset)
}

// ---- thread.metadata.update { limitRecovery } ------------------------------------------------------

/// Checks a recovery being armed against the thread's current limit; the reasons are the original's.
pub(crate) fn validate_recovery(projection: &Value, recovery: &Value, now: DateTime<Utc>) -> Result<(), String> {
    let run = blocked_run(projection, None);
    let failure = run.and_then(|run| root_failure(projection, run));
    let reset = millis(&recovery["resetAt"]);
    if recovery["snooze"] == true && reset.is_some_and(|reset| reset <= now.timestamp_millis()) {
        return Err("The reset time has passed. Retry the thread manually.".into());
    }
    let thread = &projection["thread"];
    let stale = reset.is_none()
        || !thread["archivedAt"].is_null()
        || thread["settledOverride"] == "settled"
        || run.is_none_or(|run| run["id"] != recovery["runId"])
        || failure.is_none_or(|failure| failure["class"] != "usage_limit" || failure["resetAt"] != recovery["resetAt"])
        || run.is_some_and(|run| reset.zip(millis(if run["completedAt"].is_null() { &run["requestedAt"] } else { &run["completedAt"] })).is_none_or(|(reset, ended)| reset <= ended))
        || rows(projection, "runtimeRequests").iter().any(|request| request["status"] == "pending");
    if stale {
        return Err("The provider limit changed before recovery could be configured.".into());
    }
    Ok(())
}

/// The thread after `thread.metadata.update` named `limitRecovery` (null clears it).
pub(crate) fn apply_recovery(thread: &mut Value, command: &Value, recovery: &Value, now: DateTime<Utc>) {
    let previous = &thread["limitRecovery"];
    let same = previous.is_object() && !recovery.is_null() && previous["runId"] == recovery["runId"] && previous["resetAt"] == recovery["resetAt"];
    let prior_snooze = thread["limitRecovery"]["snooze"] == true;
    let prior_reset = thread["limitRecovery"]["resetAt"].clone();
    let next = if recovery.is_null() {
        Value::Null
    } else {
        let flag = |name: &str| recovery.get(name).filter(|value| !value.is_null()).and_then(Value::as_bool).or_else(|| same.then(|| previous[name].as_bool()).flatten()).unwrap_or(false);
        let mut next = recovery.clone();
        next["autoResume"] = json!(flag("autoResume"));
        next["snooze"] = json!(flag("snooze"));
        next["requestId"] = command["commandId"].clone();
        next
    };
    let stamp = at(now);
    let snoozes = next["snooze"] == true && millis(&next["resetAt"]).is_some_and(|reset| reset > now.timestamp_millis());
    if snoozes {
        thread["snoozedUntil"] = next["resetAt"].clone();
        thread["snoozedAt"] = json!(stamp);
    } else if prior_snooze && millis(&thread["snoozedUntil"]).is_some() && millis(&thread["snoozedUntil"]) == millis(&prior_reset) {
        thread["snoozedUntil"] = Value::Null;
        thread["snoozedAt"] = Value::Null;
    }
    thread["limitRecovery"] = next;
}

// ---- message.dispatch { usageLimitContinuationOfRunId } --------------------------------------------

/// A continuation sent by the sweep is honoured only while the recovery that scheduled it is current.
/// A stale one is accepted and changes nothing, so a delayed delivery never restarts a thread; one that
/// arrives while a snooze still runs renews the recovery request instead.
pub(crate) fn admit_continuation(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Option<Decision>, StoreError> {
    let Some(run_id) = command.get("usageLimitContinuationOfRunId") else { return Ok(None) };
    let run = blocked_run(projection, None);
    let failure = run.and_then(|run| root_failure(projection, run));
    let recovery = &projection["thread"]["limitRecovery"];
    let thread = &projection["thread"];
    let snoozed = millis(&thread["snoozedUntil"]).is_some_and(|until| until > now.timestamp_millis());
    let request = command.get("usageLimitRecoveryRequestId").cloned().unwrap_or(Value::Null);
    let current = run.zip(failure).is_some_and(|(run, failure)| {
        run["id"] == *run_id
            && failure["class"] == "usage_limit"
            && shell_fields(projection).2 == "usage_limit"
            && recovery["autoResume"] == true
            && recovery["requestId"] == request
            && recovery["runId"] == run["id"]
            && recovery["resetAt"] == failure["resetAt"]
            && millis(&recovery["resetAt"]).is_some_and(|reset| reset <= now.timestamp_millis())
            && thread["archivedAt"].is_null()
            && thread["deletedAt"].is_null()
            && thread["settledOverride"] != "settled"
            && thread["providerInstanceId"] == run["providerInstanceId"]
            && !rows(projection, "runtimeRequests").iter().any(|request| request["status"] == "pending")
            && !snoozed
    });
    if current {
        return Ok(None);
    }
    let mut payload = thread.clone();
    if recovery["autoResume"] == true && recovery["requestId"] == request && recovery["runId"] == *run_id && snoozed {
        payload["limitRecovery"]["requestId"] = command["commandId"].clone();
    }
    let events = vec![event(thread["id"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), "thread.metadata-updated", crate::execution::checked::<t3_contracts::AppThread>(payload)?, now)];
    Ok(Some(Decision::Accepted { events, effects: vec![] }))
}

// ---- the sweep -------------------------------------------------------------------------------------

/// `limitRecoveryCommand` over a candidate: the thread's shell fields for its limited run. Arms the
/// recovery, sends the resume, or does nothing.
pub(crate) fn candidate_command(candidate: &Value, auto_resume: bool, snooze: bool, now_ms: i64) -> Option<Value> {
    if candidate["status"] != "failed" || candidate["lastErrorClass"] != "usage_limit" || candidate["latestRunId"].as_str().is_none_or(str::is_empty) || candidate["usageLimitResetAt"].as_str().is_none_or(str::is_empty) || !candidate["archivedAt"].is_null() || candidate["settledOverride"] == "settled" || !candidate["pendingRuntimeRequest"].is_null() {
        return None;
    }
    let reset_ms = millis(&candidate["usageLimitResetAt"])?;
    // An already-expired window reported with a fresh failure cannot start a retry loop.
    let ended = millis(if candidate["latestRunCompletedAt"].is_null() { &candidate["updatedAt"] } else { &candidate["latestRunCompletedAt"] })?;
    if reset_ms <= ended {
        return None;
    }
    let (thread_id, run_id) = (candidate["id"].as_str()?, candidate["latestRunId"].as_str()?);
    let identity = format!("{thread_id}:{run_id}:{reset_ms}");
    let recovery = &candidate["limitRecovery"];
    if recovery["runId"] != candidate["latestRunId"] || recovery["resetAt"] != candidate["usageLimitResetAt"] {
        if !auto_resume && (!snooze || reset_ms <= now_ms) {
            return None;
        }
        return Some(json!({"type":"thread.metadata.update","commandId":format!("limit-arm:{identity}"),"threadId":thread_id,"limitRecovery":{"runId":run_id,"resetAt":candidate["usageLimitResetAt"],"autoResume":auto_resume,"snooze":snooze && reset_ms > now_ms}}));
    }
    if recovery["autoResume"] != true || reset_ms > now_ms || millis(&candidate["snoozedUntil"]).is_some_and(|until| until > now_ms) {
        return None;
    }
    let delivery = format!("{identity}:{}", recovery["requestId"].as_str().unwrap_or("legacy"));
    let mut command = json!({"type":"message.dispatch","commandId":format!("limit-resume:{delivery}"),"messageId":format!("limit-resume:{delivery}"),"threadId":thread_id,"usageLimitContinuationOfRunId":run_id,"text":"Continue where you left off.","attachments":[],"dispatchMode":{"type":"start_immediately"},"createdBy":"user","creationSource":"server"});
    if !recovery["requestId"].is_null() {
        command["usageLimitRecoveryRequestId"] = recovery["requestId"].clone();
    }
    Some(command)
}

/// What the sweep does for one thread: the candidate its latest executed run makes, if that run was stopped by a limit.
pub(crate) fn recovery_command(projection: &Value, auto_resume: bool, snooze: bool, now_ms: i64) -> Option<Value> {
    let thread = &projection["thread"];
    let run = blocked_run(projection, session_error(projection).as_deref())?;
    let (_, class, reset) = error_summary(root_failure(projection, run), session_error(projection).as_deref());
    let candidate = json!({"id":thread["id"],"status":"failed","lastErrorClass":class,"usageLimitResetAt":reset,"latestRunId":run["id"],"latestRunCompletedAt":run["completedAt"],"updatedAt":thread["updatedAt"],"archivedAt":thread["archivedAt"],"settledOverride":thread["settledOverride"],
        "pendingRuntimeRequest":rows(projection, "runtimeRequests").iter().find(|request| request["status"] == "pending").map_or(Value::Null, |request| json!({"id":request["id"]})),"limitRecovery":thread.get("limitRecovery").cloned().unwrap_or(Value::Null),"snoozedUntil":thread.get("snoozedUntil").cloned().unwrap_or(Value::Null)});
    candidate_command(&candidate, auto_resume, snooze, now_ms)
}

/// The next time a limited thread needs the sweep: the reset of a recovery that waits for it.
fn next_deadline(projection: &Value) -> Option<i64> {
    let recovery = &projection["thread"]["limitRecovery"];
    let run = blocked_run(projection, session_error(projection).as_deref())?;
    (recovery["autoResume"] == true && recovery["runId"] == run["id"]).then(|| millis(&recovery["resetAt"])).flatten()
}

fn sweep_thread(execution: &ExecutionService, thread_id: &str, now_ms: i64) -> Option<i64> {
    let projection = execution.store.projection("thread", thread_id).ok().flatten()?;
    let settings = execution.providers.settings();
    let command = recovery_command(&projection, settings.auto_resume_limited_threads, settings.snooze_limited_threads, now_ms);
    if let Some(command) = command {
        let at = Utc.timestamp_millis_opt(now_ms).single().unwrap_or_else(Utc::now);
        let outcome = if command["type"] == "message.dispatch" { execution.dispatch(&command, at) } else { ThreadService::new(execution.store.clone()).dispatch(&command, at) };
        if let Err(error) = outcome {
            tracing::warn!(thread = thread_id, %error, "orchestration-v2.limit-recovery.dispatch-failed");
        }
    }
    execution.store.projection("thread", thread_id).ok().flatten().and_then(|projection| next_deadline(&projection))
}

fn limited_thread_ids(store: &Store) -> Vec<String> {
    store
        .projections("thread")
        .unwrap_or_default()
        .iter()
        .filter(|projection| projection["thread"]["deletedAt"].is_null() && latest_executed(projection).is_some_and(|run| run["status"] == "failed"))
        .filter_map(|projection| projection["thread"]["id"].as_str().map(str::to_owned))
        .collect()
}

/// Follows the store: a run that failed, a recovery that changed, or a reset time that passed sends the
/// thread through the sweep. The clock is the one idle release runs on, so a test moves time.
pub(crate) fn start(execution: &ExecutionService) {
    let execution = execution.clone();
    let mut events = execution.store.subscribe();
    tokio::spawn(async move {
        let clock = execution.providers.idle_release().clock();
        let mut deadlines: std::collections::HashMap<String, i64> = Default::default();
        let mut pending: Vec<String> = {
            let store = execution.store.clone();
            tokio::task::spawn_blocking(move || limited_thread_ids(&store)).await.unwrap_or_default()
        };
        loop {
            if !pending.is_empty() {
                let (work, due) = (std::mem::take(&mut pending), clock.now_ms());
                let handler = execution.clone();
                let results = tokio::task::spawn_blocking(move || work.into_iter().map(|id| { let next = sweep_thread(&handler, &id, due); (id, next) }).collect::<Vec<_>>()).await.unwrap_or_default();
                for (id, next) in results {
                    match next {
                        Some(deadline) => deadlines.insert(id, deadline),
                        None => deadlines.remove(&id),
                    };
                }
            }
            let earliest = deadlines.values().min().copied();
            let wait = async {
                match earliest {
                    Some(deadline) => clock.sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                batch = events.recv() => match batch {
                    Ok(batch) => {
                        for stored in batch.iter().filter(|stored| stored.event.aggregate_kind == "thread" && (matches!(stored.event.event_type.as_str(), "run.updated" | "provider-session.updated" | "thread.metadata-updated" | "thread.unsnoozed" | "thread.settled" | "thread.unsettled" | "thread.unarchived") || (stored.event.event_type == "turn-item.updated" && stored.event.payload["type"] == "error"))) {
                            if !pending.contains(&stored.event.aggregate_id) {
                                pending.push(stored.event.aggregate_id.clone());
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let store = execution.store.clone();
                        pending = tokio::task::spawn_blocking(move || limited_thread_ids(&store)).await.unwrap_or_default();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                () = wait => {
                    let due = clock.now_ms();
                    pending = deadlines.iter().filter(|(_, deadline)| **deadline <= due).map(|(id, _)| id.clone()).collect();
                    for id in &pending {
                        deadlines.remove(id);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_thread(reset: &str, recovery: Value) -> Value {
        json!({"thread":{"id":"t","projectId":"p","title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":null,
            "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"t"},"forkedFrom":null,"createdAt":"2026-01-01T00:00:00.000Z","createdBy":"user","creationSource":"web","archivedAt":null,"deletedAt":null,"settledAt":null,"settledOverride":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"updatedAt":"2026-01-01T00:00:00.000Z","limitRecovery":recovery},
            "runs":[{"id":"r1","ordinal":1,"status":"failed","providerInstanceId":"codex","rootNodeId":"n1","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":"2026-01-01T00:00:00.000Z","completedAt":"2026-01-01T00:10:00.000Z"}],
            "turnItems":[{"id":"e1","type":"error","status":"failed","runId":"r1","nodeId":"n1","ordinal":1,"updatedAt":"2026-01-01T00:10:00.000Z","failure":{"class":"usage_limit","message":"limit","resetAt":reset}}],
            "providerSessions":[],"runtimeRequests":[]})
    }

    /// The original's `limitRecoveryCommand` and thread-error functions, over the rows the generator wrote.
    #[test]
    fn the_candidate_command_and_error_summary_match_the_original() {
        let (mut commands, mut summaries) = (0, 0);
        for line in include_str!("../tests/fixtures/usage-limit-recovery.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["kind"] == "command" {
                let actual = candidate_command(&row["input"], row["autoResume"] == true, row["snooze"] == true, row["nowMs"].as_i64().unwrap());
                assert_eq!(actual.unwrap_or(Value::Null), row["output"], "{row}");
                commands += 1;
                continue;
            }
            let projection = json!({"thread":{"id":"t","providerInstanceId":"codex"},"runs":row["runs"],"turnItems":row["items"],"providerSessions":[]});
            let error = row["sessionError"].as_str();
            let blocked = blocked_run(&projection, error).map(|run| run["id"].clone());
            assert_eq!(blocked.unwrap_or(Value::Null), row["blockedRunId"], "{row}");
            let presented = presented_as_latest(&projection, error).map(|run| run["id"].clone());
            assert_eq!(presented.unwrap_or(Value::Null), row["presentedAsLatestRunId"], "{row}");
            let (last_error, class, reset) = error_summary(latest_executed(&projection).and_then(|run| root_failure(&projection, run)), error);
            assert_eq!(json!({"usageLimitResetAt":reset,"lastError":last_error,"lastErrorClass":class}), row["summary"], "{row}");
            summaries += 1;
        }
        assert_eq!((commands, summaries), (336, 11));
    }

    #[test]
    fn an_unarmed_limit_is_armed_and_an_armed_one_resumes_after_its_reset() {
        let reset = "2026-01-01T05:00:00.000Z";
        let ms = millis(&json!(reset)).unwrap();
        let arm = recovery_command(&failed_thread(reset, Value::Null), true, false, ms - 3_600_000).unwrap();
        assert_eq!((arm["type"].as_str(), arm["limitRecovery"]["autoResume"].clone(), arm["limitRecovery"]["snooze"].clone()), (Some("thread.metadata.update"), json!(true), json!(false)));
        assert!(recovery_command(&failed_thread(reset, Value::Null), false, false, ms - 1).is_none());
        let armed = json!({"runId":"r1","resetAt":reset,"autoResume":true,"snooze":false,"requestId":"c1"});
        assert!(recovery_command(&failed_thread(reset, armed.clone()), true, false, ms - 1).is_none());
        let resume = recovery_command(&failed_thread(reset, armed), true, false, ms).unwrap();
        assert_eq!((resume["type"].as_str(), resume["usageLimitContinuationOfRunId"].as_str(), resume["usageLimitRecoveryRequestId"].as_str()), (Some("message.dispatch"), Some("r1"), Some("c1")));
        assert_eq!(resume["commandId"], format!("limit-resume:t:r1:{ms}:c1"));
    }

    #[test]
    fn a_reset_already_past_when_the_run_ended_never_starts_a_retry_loop() {
        assert!(recovery_command(&failed_thread("2026-01-01T00:05:00.000Z", Value::Null), true, true, 0).is_none());
    }

    #[test]
    fn arming_a_recovery_that_snoozes_sleeps_the_thread_until_the_reset_and_the_validation_names_why_it_fails() {
        let reset = "2026-01-01T05:00:00.000Z";
        let now: DateTime<Utc> = "2026-01-01T01:00:00Z".parse().unwrap();
        let projection = failed_thread(reset, Value::Null);
        let recovery = json!({"runId":"r1","resetAt":reset,"autoResume":true,"snooze":true});
        validate_recovery(&projection, &recovery, now).unwrap();
        let mut thread = projection["thread"].clone();
        apply_recovery(&mut thread, &json!({"commandId":"arm"}), &recovery, now);
        assert_eq!((thread["snoozedUntil"].as_str(), thread["limitRecovery"]["requestId"].as_str()), (Some(reset), Some("arm")));
        let late: DateTime<Utc> = "2026-01-01T06:00:00Z".parse().unwrap();
        assert_eq!(validate_recovery(&projection, &recovery, late).unwrap_err(), "The reset time has passed. Retry the thread manually.");
        let other = json!({"runId":"r9","resetAt":reset,"autoResume":true});
        assert_eq!(validate_recovery(&projection, &other, now).unwrap_err(), "The provider limit changed before recovery could be configured.");
        // Clearing the recovery that slept the thread wakes it.
        let mut cleared = thread.clone();
        apply_recovery(&mut cleared, &json!({"commandId":"clear"}), &Value::Null, now);
        assert_eq!((cleared["limitRecovery"].is_null(), cleared["snoozedUntil"].is_null()), (true, true));
    }

    #[test]
    fn a_stale_continuation_changes_nothing_and_a_current_one_proceeds() {
        let reset = "2026-01-01T05:00:00.000Z";
        let armed = json!({"runId":"r1","resetAt":reset,"autoResume":true,"snooze":false,"requestId":"c1"});
        let projection = failed_thread(reset, armed);
        let command = json!({"commandId":"go","usageLimitContinuationOfRunId":"r1","usageLimitRecoveryRequestId":"c1"});
        let after: DateTime<Utc> = "2026-01-01T05:00:01Z".parse().unwrap();
        assert!(admit_continuation(&command, &projection, after).unwrap().is_none());
        let before: DateTime<Utc> = "2026-01-01T04:00:00Z".parse().unwrap();
        let Some(Decision::Accepted { events, .. }) = admit_continuation(&command, &projection, before).unwrap() else { panic!("a continuation before the reset is a no-op") };
        assert_eq!(events[0].event_type, "thread.metadata-updated");
        let stale = json!({"commandId":"go","usageLimitContinuationOfRunId":"r1","usageLimitRecoveryRequestId":"old"});
        assert!(matches!(admit_continuation(&stale, &projection, after).unwrap(), Some(Decision::Accepted { .. })));
    }
}
