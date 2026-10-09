//! App-owned delegated tasks and what Stop, cancelling and deleting do to the completions they owe.
//! A parent run that delegated work owes its tasks' completions as one follow-up message (a
//! cohort). Ports `disposeDelegatedCompletionCohort`, the cohort part of `holdStoppedThread` and of
//! `planThreadDeletion`. Delivering completions (the mailbox, the wake text, claiming and
//! acknowledging a delivery) is not ported: no run delivers one yet, so a cohort here only ever
//! ends.
use crate::{
    execution::{checked, event},
    persistence::{Decision, Event, StoreError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field].as_array().map_or(&[], Vec::as_slice)
}

fn has_cohort(run: &Value) -> bool {
    run.get("delegatedCompletion").is_some_and(|cohort| !cohort.is_null())
}

fn delivered(task: &Value) -> bool {
    matches!(task["completionDelivery"]["state"].as_str(), Some("acknowledged" | "delivered" | "disposed"))
}

/// The queued run that would deliver a cohort's completion, when it has not started.
fn queued_delivery_run<'a>(projection: &'a Value, cohort: &Value) -> Option<&'a Value> {
    let message = rows(projection, "messages").iter().find(|message| message["id"] == cohort["delivery"]["messageId"])?;
    rows(projection, "runs").iter().find(|run| run["id"] == message["runId"] && run["status"] == "queued")
}

/// Cancels a queued run: the run, its first attempt and its root node (`emitQueuedRunCancellation`).
pub(crate) fn queued_run_cancellation(projection: &Value, run: &Value, timestamp: &str) -> Vec<(&'static str, Value)> {
    let mut events = vec![];
    let mut cancelled = run.clone();
    cancelled["status"] = json!("cancelled");
    cancelled["queuePosition"] = Value::Null;
    cancelled["completedAt"] = json!(timestamp);
    events.push(("run.updated", cancelled));
    let node = rows(projection, "nodes").iter().find(|node| run["rootNodeId"] == node["id"]);
    let attempt = rows(projection, "attempts").iter().find(|attempt| run["activeAttemptId"] == attempt["id"]);
    if let (Some(attempt), Some(_)) = (attempt, node) {
        let mut attempt = attempt.clone();
        attempt["status"] = json!("cancelled");
        attempt["completedAt"] = json!(timestamp);
        events.push(("run-attempt.updated", attempt));
    }
    if let Some(node) = node {
        let mut node = node.clone();
        node["status"] = json!("cancelled");
        node["completedAt"] = json!(timestamp);
        events.push(("node.updated", node));
    }
    events
}

/// Ends a parent run's completion cohort without delivering it: the cohort is `stopped` (a wake the
/// user can still ask for) or `disposed`, each task still owing a delivery is disposed, and a
/// delivery run that is still queued is cancelled. A run with no cohort and no tasks has nothing to
/// end. A cohort already disposed stays disposed.
pub(crate) fn dispose_cohort(projection: &Value, parent_run_id: &str, disposition: &str, cancel_queued_delivery: bool, timestamp: &str) -> Vec<(&'static str, Value)> {
    let Some(parent) = rows(projection, "runs").iter().find(|run| run["id"] == parent_run_id) else { return vec![] };
    let cohort = has_cohort(parent).then(|| &parent["delegatedCompletion"]);
    let tasks: Vec<&Value> = rows(projection, "subagents").iter().filter(|task| task["origin"] == "app_owned" && task["runId"] == parent_run_id).collect();
    if cohort.is_none() && tasks.is_empty() {
        return vec![];
    }
    let next = if cohort.is_some_and(|cohort| cohort["disposition"] == "disposed") { "disposed" } else { disposition };
    let mut run = parent.clone();
    run["delegatedCompletion"] = json!({"disposition":next,"nextGeneration":cohort.map_or(json!(1),|cohort|cohort["nextGeneration"].clone()),"delivery":null});
    let mut events = vec![("run.updated", run)];
    for task in tasks.iter().filter(|task| !delivered(task)) {
        let mut task = (*task).clone();
        task["completionDelivery"] = json!({"state":"disposed","observedByRunId":null});
        task["updatedAt"] = json!(timestamp);
        events.push(("subagent.updated", task));
    }
    if cancel_queued_delivery {
        if let Some(run) = cohort.and_then(|cohort| queued_delivery_run(projection, cohort)) {
            events.extend(queued_run_cancellation(projection, run, timestamp));
        }
    }
    events
}

/// The runs whose cohorts are open or owed: every run with a cohort and every run of an app-owned task.
pub(crate) fn cohort_run_ids(projection: &Value) -> Vec<String> {
    let mut ids: Vec<String> = vec![];
    let mut add = |id: &Value| {
        if let Some(id) = id.as_str() {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_owned());
            }
        }
    };
    for run in rows(projection, "runs").iter().filter(|run| has_cohort(run)) {
        add(&run["id"]);
    }
    for task in rows(projection, "subagents").iter().filter(|task| task["origin"] == "app_owned") {
        add(&task["runId"]);
    }
    ids
}

/// Applies run and task updates to a projection copy, so the next disposal reads what the ones
/// before it wrote.
fn apply(projection: &mut Value, events: &[(&'static str, Value)]) {
    for (kind, payload) in events {
        let field = match *kind {
            "run.updated" => "runs",
            "subagent.updated" => "subagents",
            "run-attempt.updated" => "attempts",
            "node.updated" => "nodes",
            _ => continue,
        };
        if let Some(rows) = projection[field].as_array_mut() {
            match rows.iter_mut().find(|row| row["id"] == payload["id"]) {
                Some(existing) => *existing = payload.clone(),
                None => rows.push(payload.clone()),
            }
        }
    }
}

/// Disposes the cohorts of `run_ids` in order, each reading the events of the ones before it.
pub(crate) fn dispose_cohorts(projection: &Value, run_ids: &[String], disposition: &str, cancel_queued_delivery: bool, timestamp: &str) -> Vec<(&'static str, Value)> {
    let mut current = projection.clone();
    let mut all = vec![];
    for id in run_ids {
        let events = dispose_cohort(&current, id, disposition, cancel_queued_delivery, timestamp);
        apply(&mut current, &events);
        all.extend(events);
    }
    all
}

pub(crate) fn to_events(thread_id: &str, command_id: &str, planned: Vec<(&'static str, Value)>, now: DateTime<Utc>) -> Result<Vec<Event>, StoreError> {
    planned
        .into_iter()
        .map(|(kind, payload)| {
            let payload = match kind {
                "run.updated" => checked::<t3_contracts::Run>(payload)?,
                "run-attempt.updated" => checked::<t3_contracts::RunAttempt>(payload)?,
                "node.updated" => checked::<t3_contracts::ExecutionNode>(payload)?,
                "subagent.updated" => checked::<t3_contracts::Subagent>(payload)?,
                "thread.pull-request-synced" => t3_contracts::normalize_event_payload(kind, payload).map_err(|error| StoreError::InvalidCommand(error.to_string()))?,
                _ => payload,
            };
            Ok(event(thread_id, command_id, kind, payload, now))
        })
        .collect()
}

/// `queued-run.cancel` of a run that delivers a cohort's completion: the whole cohort is disposed,
/// which cancels that run with it.
pub(crate) fn plan_dispose_cohort(command: &Value, projection: &Value, parent_run_id: &str, disposition: &str, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let timestamp = crate::execution::at(now);
    let planned = dispose_cohort(projection, parent_run_id, disposition, true, &timestamp);
    let events = to_events(command["threadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), planned, now)?;
    Ok(Decision::Accepted { events, effects: vec![] })
}

/// What Stop holds besides the run it interrupts: the thread's pull request watches end, and every
/// wake its delegated tasks still owe is dropped as `stopped`. `cohort_run_ids` are disposed even
/// when they owe nothing, like a plain interrupt's.
pub(crate) fn hold_stopped_thread(projection: &Value, cohort_run_ids: &[String], timestamp: &str) -> Vec<(&'static str, Value)> {
    let mut events = vec![];
    let thread = &projection["thread"];
    let links = thread["pullRequests"].as_array().map_or(&[][..], Vec::as_slice);
    if links.iter().any(|link| link.get("watch").is_some_and(|watch| !watch.is_null())) {
        let mut thread = thread.clone();
        thread["pullRequests"] = Value::Array(links.iter().map(|link| {
            let mut link = link.clone();
            link.as_object_mut().map(|object| object.remove("watch"));
            link
        }).collect());
        events.push(("thread.pull-request-synced", thread));
    }
    let mut ids: Vec<String> = cohort_run_ids.to_vec();
    for run in rows(projection, "runs") {
        if run["delegatedCompletion"]["delivery"].is_object() && !ids.iter().any(|id| run["id"] == id.as_str()) {
            ids.push(run["id"].as_str().unwrap_or_default().to_owned());
        }
    }
    for task in rows(projection, "subagents") {
        if task["origin"] == "app_owned" && !task["runId"].is_null() && !delivered(task) {
            if let Some(id) = task["runId"].as_str().filter(|id| !ids.iter().any(|known| known == id)) {
                ids.push(id.to_owned());
            }
        }
    }
    let mut current = projection.clone();
    apply(&mut current, &events);
    events.extend(dispose_cohorts(&current, &ids, "stopped", true, timestamp));
    events
}

/// What Stop (or a plain interrupt) leaves behind for the run it reaches: the completion cohort of
/// the run, or of the run it woke, is stopped. Stop also holds the rest of the thread.
pub(crate) fn stop_remaining(command: &Value, projection: &Value, run: &Value, now: DateTime<Utc>) -> Result<Vec<Event>, StoreError> {
    let timestamp = crate::execution::at(now);
    let message = rows(projection, "messages").iter().find(|message| message["id"] == run["userMessageId"]);
    let cohort = message.and_then(|message| message["delegatedCompletion"]["parentRunId"].as_str()).or_else(|| run["id"].as_str()).unwrap_or_default().to_owned();
    let planned = if command["holdQueue"] == true {
        hold_stopped_thread(projection, &[cohort], &timestamp)
    } else {
        dispose_cohorts(projection, &[cohort], "stopped", true, &timestamp)
    };
    to_events(command["threadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default(), planned, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> Value {
        json!({
            "thread":{"id":"thread","pullRequests":[]},
            "runs":[
                {"id":"parent","status":"completed","rootNodeId":"n","delegatedCompletion":{"disposition":"open","nextGeneration":2,"delivery":{"generation":1,"messageId":"wake","taskIds":["t1"]}}},
                {"id":"wake-run","status":"queued","rootNodeId":"wn","activeAttemptId":"wa"}],
            "messages":[{"id":"wake","runId":"wake-run"}],
            "nodes":[{"id":"wn","status":"pending"}],
            "attempts":[{"id":"wa","status":"pending"}],
            "subagents":[
                {"id":"t1","origin":"app_owned","runId":"parent","completionDelivery":{"state":"pending","observedByRunId":null}},
                {"id":"t2","origin":"app_owned","runId":"parent","completionDelivery":{"state":"delivered","observedByRunId":"x"}}]})
    }

    #[test]
    fn disposing_a_cohort_ends_its_owed_tasks_and_cancels_the_queued_delivery() {
        let events = dispose_cohort(&projection(), "parent", "stopped", true, "2026-01-01T00:00:00.000Z");
        let kinds: Vec<(&str, &str)> = events.iter().map(|(kind, payload)| (*kind, payload["id"].as_str().unwrap())).collect();
        assert_eq!(kinds, vec![("run.updated", "parent"), ("subagent.updated", "t1"), ("run.updated", "wake-run"), ("run-attempt.updated", "wa"), ("node.updated", "wn")]);
        assert_eq!(events[0].1["delegatedCompletion"], json!({"disposition":"stopped","nextGeneration":2,"delivery":null}));
        assert_eq!(events[1].1["completionDelivery"], json!({"state":"disposed","observedByRunId":null}));
        assert_eq!(events[2].1["status"], "cancelled");
        // Nothing to end for a run that delegated nothing, and a disposed cohort stays disposed.
        assert!(dispose_cohort(&projection(), "wake-run", "stopped", true, "t").is_empty());
        let mut again = projection();
        again["runs"][0]["delegatedCompletion"]["disposition"] = json!("disposed");
        assert_eq!(dispose_cohort(&again, "parent", "stopped", false, "t")[0].1["delegatedCompletion"]["disposition"], "disposed");
        assert_eq!(dispose_cohort(&again, "parent", "stopped", false, "t").len(), 2);
    }

    #[test]
    fn stop_ends_watches_and_every_cohort_still_owing() {
        let mut stopped = projection();
        stopped["thread"]["pullRequests"] = json!([{"number":1,"watch":{"startedAt":"x"}},{"number":2}]);
        let events = hold_stopped_thread(&stopped, &[], "2026-01-01T00:00:00.000Z");
        assert_eq!(events[0].0, "thread.pull-request-synced");
        assert!(events[0].1["pullRequests"].as_array().unwrap().iter().all(|link| link.get("watch").is_none()));
        assert!(events.iter().any(|(kind, payload)| *kind == "run.updated" && payload["id"] == "parent" && payload["delegatedCompletion"]["disposition"] == "stopped"));
    }
}
