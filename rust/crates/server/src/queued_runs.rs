//! Management of a run waiting behind the active one: cancelling it and editing its message.
//! Ports `dispatchQueuedRunCancel` and `dispatchQueuedRunEdit`. A refusal is a rejected receipt
//! carrying the planner's `OrchestratorDispatchError`, as in the original.
use crate::{
    execution::{at, checked, event},
    persistence::{Decision, StoreError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field].as_array().map_or(&[], Vec::as_slice)
}

fn refuse(command: &Value, cause: String) -> Decision {
    Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command["commandId"],"commandType":command["type"],"cause":cause}))
}

fn queued_run<'a>(command: &Value, projection: &'a Value) -> Option<&'a Value> {
    rows(projection, "runs").iter().find(|run| run["id"] == command["runId"] && run["status"] == "queued")
}

/// `queued-run.cancel`: the run, its first attempt and its root node end as cancelled without ever
/// having started. A run that delivers a delegated task's completion is not cancelled by itself:
/// cancelling it disposes the whole completion cohort (`delegated_tasks`).
pub(crate) fn plan_cancel(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let Some(run) = queued_run(command, projection) else {
        return Ok(refuse(command, format!("Run {} is not queued.", command["runId"].as_str().unwrap_or_default())));
    };
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let command_id = command["commandId"].as_str().unwrap_or_default();
    let message = rows(projection, "messages").iter().find(|message| message["id"] == run["userMessageId"]);
    if let Some(parent) = message.map(|message| &message["delegatedCompletion"]).filter(|cohort| cohort.is_object()) {
        return crate::delegated_tasks::plan_dispose_cohort(command, projection, parent["parentRunId"].as_str().unwrap_or_default(), "disposed", now);
    }
    let timestamp = at(now);
    let mut events = vec![];
    let mut cancelled = run.clone();
    cancelled["status"] = json!("cancelled");
    cancelled["queuePosition"] = Value::Null;
    cancelled["completedAt"] = json!(timestamp);
    events.push(event(thread_id, command_id, "run.updated", checked::<t3_contracts::Run>(cancelled)?, now));
    let attempt = rows(projection, "attempts").iter().find(|attempt| run["activeAttemptId"] == attempt["id"]);
    let node = rows(projection, "nodes").iter().find(|node| run["rootNodeId"] == node["id"]);
    if let (Some(attempt), Some(_)) = (attempt, node) {
        let mut attempt = attempt.clone();
        attempt["status"] = json!("cancelled");
        attempt["completedAt"] = json!(timestamp);
        events.push(event(thread_id, command_id, "run-attempt.updated", checked::<t3_contracts::RunAttempt>(attempt)?, now));
    }
    if let Some(node) = node {
        let mut node = node.clone();
        node["status"] = json!("cancelled");
        node["completedAt"] = json!(timestamp);
        events.push(event(thread_id, command_id, "node.updated", checked::<t3_contracts::ExecutionNode>(node)?, now));
    }
    Ok(Decision::Accepted { events, effects: vec![] })
}

/// `queued-run.edit`: replaces the text (and, when given, the context and attachments) of the
/// message a queued run will send. Automatic completion deliveries are not the user's to edit.
pub(crate) fn plan_edit(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let run_id = command["runId"].as_str().unwrap_or_default();
    if command["text"].as_str().is_none_or(|text| text.trim().is_empty()) {
        return Ok(refuse(command, format!("Queued run {run_id} cannot be edited to an empty message.")));
    }
    let Some(run) = queued_run(command, projection) else {
        return Ok(refuse(command, format!("Run {run_id} is not queued.")));
    };
    let Some(message) = rows(projection, "messages").iter().find(|message| message["id"] == run["userMessageId"]) else {
        return Ok(refuse(command, format!("Queued run {run_id} has no user message.")));
    };
    if message.get("delegatedCompletion").is_some_and(|value| !value.is_null()) || message.get("notification").is_some_and(|value| !value.is_null()) {
        return Ok(refuse(command, "Automatic completion deliveries cannot be edited.".into()));
    }
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let command_id = command["commandId"].as_str().unwrap_or_default();
    let timestamp = at(now);
    let edited = |row: &Value| {
        let mut row = row.clone();
        row["text"] = command["text"].clone();
        if command.get("attachments").is_some_and(|attachments| !attachments.is_null()) {
            row["attachments"] = command["attachments"].clone();
        }
        if command.get("context").is_some_and(|context| !context.is_null()) {
            row["context"] = command["context"].clone();
        }
        row["updatedAt"] = json!(timestamp);
        row
    };
    let mut events = vec![event(thread_id, command_id, "message.updated", checked::<t3_contracts::ConversationMessage>(edited(message))?, now)];
    if let Some(item) = rows(projection, "turnItems").iter().find(|item| item["type"] == "user_message" && item["messageId"] == message["id"]) {
        events.push(event(thread_id, command_id, "turn-item.updated", checked::<t3_contracts::TurnItem>(edited(item))?, now));
    }
    Ok(Decision::Accepted { events, effects: vec![] })
}

/// A run that delivers a delegated completion is the app's, not the user's, and is not reordered.
fn is_automatic(projection: &Value, run: &Value) -> bool {
    rows(projection, "messages").iter().any(|message| message["id"] == run["userMessageId"] && message.get("delegatedCompletion").is_some_and(|value| !value.is_null()))
}

/// The queued runs in the order they are delivered: automatic completion deliveries first, then by
/// queue position and ordinal (`queuedRunsInDeliveryOrder`).
pub(crate) fn delivery_order(projection: &Value) -> Vec<&Value> {
    let mut queued: Vec<&Value> = rows(projection, "runs").iter().filter(|run| run["status"] == "queued").collect();
    queued.sort_by(|left, right| {
        let priority = |run: &Value| is_automatic(projection, run);
        let position = |run: &Value| run["queuePosition"].as_u64().or_else(|| run["ordinal"].as_u64());
        priority(right).cmp(&priority(left)).then_with(|| position(left).cmp(&position(right))).then_with(|| left["ordinal"].as_u64().cmp(&right["ordinal"].as_u64()))
    });
    queued
}

/// `queued-run.reorder`: moves a queued message ahead of another, or to the end. Automatic
/// completion deliveries stay first and cannot be moved or passed.
pub(crate) fn plan_reorder(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let run_id = command["runId"].as_str().unwrap_or_default();
    let queued = delivery_order(projection);
    let Some(moving) = queued.iter().copied().find(|run| run["id"] == command["runId"]) else {
        return Ok(refuse(command, format!("Run {run_id} is not queued.")));
    };
    if rows(projection, "messages").iter().any(|message| message["id"] == moving["userMessageId"] && (message.get("delegatedCompletion").is_some_and(|value| !value.is_null()) || message.get("notification").is_some_and(|value| !value.is_null()))) {
        return Ok(refuse(command, "Automatic completion deliveries cannot be reordered.".into()));
    }
    let before = command.get("beforeRunId").filter(|id| !id.is_null());
    let (automatic, reorderable): (Vec<&Value>, Vec<&Value>) = queued.iter().copied().partition(|run| is_automatic(projection, run));
    if before.is_some_and(|before| automatic.iter().any(|run| run["id"] == *before)) {
        return Ok(refuse(command, "Queued messages cannot be reordered ahead of automatic completion delivery.".into()));
    }
    let without: Vec<&Value> = reorderable.into_iter().filter(|run| run["id"] != command["runId"]).collect();
    let index = match before {
        None => without.len(),
        Some(before) => match without.iter().position(|run| run["id"] == *before) {
            Some(index) => index,
            None => return Ok(refuse(command, format!("Queue target {} is not queued.", before.as_str().unwrap_or_default()))),
        },
    };
    let mut reordered = automatic;
    reordered.extend(&without[..index]);
    reordered.push(moving);
    reordered.extend(&without[index..]);
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let command_id = command["commandId"].as_str().unwrap_or_default();
    let mut events = vec![];
    for (position, run) in reordered.into_iter().enumerate() {
        let position = position as u64 + 1;
        if run["queuePosition"].as_u64() == Some(position) {
            continue;
        }
        let mut run = run.clone();
        run["queuePosition"] = json!(position);
        events.push(event(thread_id, command_id, "run.updated", checked::<t3_contracts::Run>(run)?, now));
    }
    Ok(Decision::Accepted { events, effects: vec![] })
}

/// `queue.resume`: releases a held queue. A thread stopped by its usage limit must be continued
/// first. With nothing queued the command still records itself, as a metadata update of the thread.
pub(crate) fn plan_resume(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let command_id = command["commandId"].as_str().unwrap_or_default();
    if !projection["thread"]["archivedAt"].is_null() || !projection["thread"]["deletedAt"].is_null() {
        return Ok(refuse(command, format!("Thread {thread_id} is not active.")));
    }
    let session_error = rows(projection, "providerSessions")
        .iter()
        .filter(|session| session["providerInstanceId"] == projection["thread"]["providerInstanceId"])
        .reduce(|latest, session| if session["updatedAt"].as_str() > latest["updatedAt"].as_str() { session } else { latest })
        .and_then(|session| session["lastError"].as_str());
    // `usageLimitBlockedRun`: the latest executed run failed on the limit, and no later session error replaced it.
    let blocked = crate::message_queue::latest_executed(projection)
        .and_then(|run| crate::message_queue::root_failure(projection, run))
        .is_some_and(|failure| failure["class"] == "usage_limit" && session_error.is_none_or(|error| failure["message"] == error));
    if blocked {
        return Ok(refuse(command, "Continue the limited thread before resuming its queue.".into()));
    }
    let mut events = vec![];
    for run in rows(projection, "runs").iter().filter(|run| run["status"] == "queued") {
        let mut run = run.clone();
        run["queueHeld"] = json!(false);
        events.push(event(thread_id, command_id, "run.updated", checked::<t3_contracts::Run>(run)?, now));
    }
    if events.is_empty() {
        events.push(event(thread_id, command_id, "thread.metadata-updated", checked::<t3_contracts::AppThread>(projection["thread"].clone())?, now));
    }
    Ok(Decision::Accepted { events, effects: vec![] })
}
