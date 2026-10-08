//! Durable app-owned message queues. Provider policy and checkpoint preparation
//! are resolved by the caller before graph allocation; no provider is contacted
//! until the returned start effect is committed.
use crate::{
    execution::{at, checked, event},
    persistence::{Decision, NewEffect, Store, StoreError, read_projection},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::cmp::Ordering;

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) fn blocking(run: &Value) -> bool {
    matches!(
        run["status"].as_str(),
        Some("preparing" | "starting" | "running" | "waiting")
    )
}

/// Automatic child-completion messages precede user messages. Sorting remains
/// stable when both queue positions and ordinals tie, as Array.toSorted does.
pub(crate) fn ordered(projection: &Value) -> Vec<&Value> {
    let automatic = |run: &Value| {
        rows(projection, "messages").iter().any(|message| {
            message["id"] == run["userMessageId"] && message.get("delegatedCompletion").is_some()
        })
    };
    let mut queued: Vec<_> = rows(projection, "runs")
        .iter()
        .filter(|run| run["status"] == "queued")
        .collect();
    queued.sort_by(|left, right| {
        automatic(right)
            .cmp(&automatic(left))
            .then_with(|| position(left).cmp(&position(right)))
            .then_with(|| left["ordinal"].as_u64().cmp(&right["ordinal"].as_u64()))
    });
    queued
}

fn position(run: &Value) -> u64 {
    run["queuePosition"]
        .as_u64()
        .or_else(|| run["ordinal"].as_u64())
        .unwrap_or(0)
}

fn ended(run: &Value) -> Option<DateTime<Utc>> {
    run["completedAt"]
        .as_str()
        .and_then(|value| value.parse().ok())
}

fn execution_order(left: &Value, right: &Value) -> Ordering {
    // An unfinished run is later than any finished run. Ordinal is submission
    // order, not execution order, when an older held message resumes later.
    match (ended(left), ended(right)) {
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (left_end, right_end) => left_end
            .cmp(&right_end)
            .then_with(|| left["ordinal"].as_u64().cmp(&right["ordinal"].as_u64())),
    }
}

fn latest_executed(projection: &Value) -> Option<&Value> {
    rows(projection, "runs")
        .iter()
        .filter(|run| {
            run["status"] != "queued"
                && !(run["status"] == "cancelled" && run["startedAt"].is_null())
        })
        .reduce(|latest, run| {
            if execution_order(run, latest).is_gt() {
                run
            } else {
                latest
            }
        })
}

fn root_failure<'a>(projection: &'a Value, run: &Value) -> Option<&'a Value> {
    if run["status"] != "failed" {
        return None;
    }
    rows(projection, "turnItems")
        .iter()
        .filter(|item| {
            item["type"] == "error"
                && item["status"] == "failed"
                && item["runId"] == run["id"]
                && item["nodeId"] == run["rootNodeId"]
        })
        .max_by(|left, right| {
            left["updatedAt"]
                .as_str()
                .cmp(&right["updatedAt"].as_str())
                .then_with(|| left["ordinal"].as_u64().cmp(&right["ordinal"].as_u64()))
                .then_with(|| {
                    left["id"]
                        .as_str()
                        .unwrap_or("")
                        .encode_utf16()
                        .cmp(right["id"].as_str().unwrap_or("").encode_utf16())
                })
        })
        .and_then(|item| item.get("failure"))
        .filter(|failure| !failure.is_null())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission<'a> {
    Blocked,
    Hold,
    Start(&'a Value),
}

/// Mirror startNextQueuedRun's non-I/O admission. A distinct session error
/// supersedes the root failure's usage-limit classification. Provider failures
/// hold all waiting messages; validation failures do not poison the next run.
pub(crate) fn admission<'a>(projection: &'a Value, failed_run_id: Option<&str>) -> Admission<'a> {
    let runs = rows(projection, "runs");
    if !projection["thread"]["archivedAt"].is_null()
        || !projection["thread"]["deletedAt"].is_null()
        || runs.iter().any(blocking)
        || runs
            .iter()
            .any(|run| run["status"] == "queued" && run["queueHeld"] == true)
    {
        return Admission::Blocked;
    }
    let Some(next) = ordered(projection).first().copied() else {
        return Admission::Blocked;
    };
    if let Some(latest) = latest_executed(projection) {
        let failure = root_failure(projection, latest);
        let session_error = rows(projection, "providerSessions")
            .iter()
            .filter(|session| {
                session["providerInstanceId"] == projection["thread"]["providerInstanceId"]
            })
            .reduce(|latest, session| {
                if session["updatedAt"].as_str() > latest["updatedAt"].as_str() {
                    session
                } else {
                    latest
                }
            })
            .and_then(|session| session["lastError"].as_str());
        if failure.is_some_and(|failure| {
            failure["class"] == "usage_limit"
                && session_error.is_none_or(|error| failure["message"] == error)
        }) {
            return Admission::Blocked;
        }
        if latest["id"].as_str() == failed_run_id
            && latest["providerInstanceId"] == next["providerInstanceId"]
            && failure.is_some_and(|failure| failure["class"] != "validation_error")
        {
            return Admission::Hold;
        }
    }
    Admission::Start(next)
}

/// The resolved target provider thread and optional prepared root checkpoint
/// scope stay in the same command transaction as the new execution graph.
pub(crate) struct QueueContext<'a> {
    pub provider_thread: &'a Value,
    pub new_provider_thread: bool,
    pub checkpoint_scope: Option<Value>,
}

pub(crate) fn allocate(
    command: &Value,
    projection: &Value,
    context: QueueContext<'_>,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    let thread_id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    let timestamp = at(now);
    let model = command
        .get("modelSelection")
        .unwrap_or(&projection["thread"]["modelSelection"]);
    if !rows(projection, "runs").iter().any(blocking) {
        return Err(StoreError::InvalidCommand(
            "Queued allocation requires an active run.".into(),
        ));
    }
    if rows(projection, "messages")
        .iter()
        .any(|message| message["id"] == command["messageId"])
    {
        return Err(StoreError::InvalidCommand(
            "Message id already exists.".into(),
        ));
    }
    if context.provider_thread["providerInstanceId"] != model["instanceId"] {
        return Err(StoreError::InvalidCommand(
            "Queued provider thread belongs to another instance.".into(),
        ));
    }
    let ordinal = rows(projection, "runs")
        .iter()
        .filter_map(|run| run["ordinal"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let run_id = format!("run:thread:{}:ordinal:{ordinal}", encode(thread_id));
    let attempt_id = format!("run-attempt:run:{}:attempt:1", encode(&run_id));
    let node_id = format!("node:run:{}:root", encode(&run_id));
    let provider_thread_id = &context.provider_thread["id"];
    let queued = rows(projection, "runs")
        .iter()
        .filter(|run| run["status"] == "queued");
    let mut run = json!({"id":run_id,"threadId":thread_id,"ordinal":ordinal,"providerInstanceId":model["instanceId"],"modelSelection":model,"providerThreadId":provider_thread_id,"userMessageId":command["messageId"],"rootNodeId":node_id,"activeAttemptId":attempt_id,"status":"queued","queuePosition":queued.clone().map(position).max().unwrap_or(0)+1,"requestedAt":timestamp,"startedAt":null,"completedAt":null,"checkpointId":null,"contextHandoffId":null});
    if queued.clone().any(|run| run["queueHeld"] == true) {
        run["queueHeld"] = json!(true);
    }
    for field in ["sourcePlanRef", "restartContinuationOfRunId"] {
        if let Some(value) = command.get(field) {
            run[field] = value.clone();
        }
    }
    let attempt = json!({"id":attempt_id,"runId":run_id,"attemptOrdinal":1,"rootNodeId":node_id,"providerInstanceId":model["instanceId"],"providerThreadId":provider_thread_id,"providerTurnId":null,"reason":"initial","status":"pending","startedAt":null,"completedAt":null});
    let node = json!({"id":node_id,"threadId":thread_id,"runId":run_id,"parentNodeId":null,"rootNodeId":node_id,"kind":"root_turn","status":"pending","countsForRun":true,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":context.checkpoint_scope.as_ref().map(|scope|scope["id"].clone()),"startedAt":null,"completedAt":null});
    let mut message = json!({"createdBy":command["createdBy"],"creationSource":command["creationSource"],"id":command["messageId"],"threadId":thread_id,"runId":run_id,"nodeId":node_id,"role":"user","text":command["text"],"attachments":command["attachments"],"streaming":false,"createdAt":timestamp,"updatedAt":timestamp});
    for field in [
        "scheduledTaskId",
        "senderThreadId",
        "context",
        "delegatedCompletion",
        "notification",
    ] {
        if let Some(value) = command.get(field) {
            message[field] = value.clone();
        }
    }
    let mut events = vec![];
    if context.new_provider_thread {
        events.push(event(
            thread_id,
            command_id,
            "provider-thread.updated",
            checked::<t3_contracts::ProviderThread>(context.provider_thread.clone())?,
            now,
        ));
    }
    for (kind, payload) in [
        ("run.created", checked::<t3_contracts::Run>(run)?),
        (
            "run-attempt.created",
            checked::<t3_contracts::RunAttempt>(attempt)?,
        ),
        (
            "node.updated",
            checked::<t3_contracts::ExecutionNode>(node)?,
        ),
        (
            "message.updated",
            checked::<t3_contracts::ConversationMessage>(message)?,
        ),
    ] {
        events.push(event(thread_id, command_id, kind, payload, now));
    }
    if let Some(scope) = context.checkpoint_scope {
        events.push(event(
            thread_id,
            command_id,
            "checkpoint-scope.updated",
            scope,
            now,
        ));
    }
    // A fresh queued message deliberately has no user transcript item or effect.
    Ok(Decision::Accepted {
        events,
        effects: vec![],
    })
}

/// Promote a validated same-provider queued graph. Handoff/checkpoint services
/// must resolve their additional events before this planner is admitted.
pub(crate) fn promote(
    projection: &Value,
    now: DateTime<Utc>,
    failed_run_id: Option<&str>,
) -> Result<Decision, StoreError> {
    let thread_id = projection["thread"]["id"].as_str().unwrap();
    let mut events = vec![];
    let run = match admission(projection, failed_run_id) {
        Admission::Blocked => {
            return Ok(Decision::Accepted {
                events,
                effects: vec![],
            });
        }
        Admission::Hold => {
            for run in rows(projection, "runs")
                .iter()
                .filter(|run| run["status"] == "queued")
            {
                let mut run = run.clone();
                run["queueHeld"] = json!(true);
                events.push(event(
                    thread_id,
                    &format!("command:system:hold-queue:{}", failed_run_id.unwrap()),
                    "run.updated",
                    checked::<t3_contracts::Run>(run)?,
                    now,
                ));
            }
            return Ok(Decision::Accepted {
                events,
                effects: vec![],
            });
        }
        Admission::Start(run) => run,
    };
    let command_id = format!(
        "command:system:start-queued:{}",
        run["id"].as_str().unwrap()
    );
    let missing = || {
        StoreError::InvalidCommand(format!(
            "Queued run {} is missing projection state.",
            run["id"].as_str().unwrap()
        ))
    };
    if run["rootNodeId"].is_null()
        || run["activeAttemptId"].is_null()
        || run["providerThreadId"].is_null()
    {
        return Err(StoreError::InvalidCommand(format!(
            "Queued run {} is missing execution identity.",
            run["id"].as_str().unwrap()
        )));
    }
    let root = rows(projection, "nodes")
        .iter()
        .find(|row| row["id"] == run["rootNodeId"])
        .ok_or_else(missing)?;
    rows(projection, "attempts")
        .iter()
        .find(|row| row["id"] == run["activeAttemptId"])
        .ok_or_else(missing)?;
    let message = rows(projection, "messages")
        .iter()
        .find(|row| row["id"] == run["userMessageId"])
        .ok_or_else(missing)?;
    let provider = rows(projection, "providerThreads")
        .iter()
        .find(|row| row["id"] == run["providerThreadId"])
        .ok_or_else(missing)?;
    if !root["checkpointScopeId"].is_null()
        && !rows(projection, "checkpointScopes")
            .iter()
            .any(|row| row["id"] == root["checkpointScopeId"])
    {
        return Err(missing());
    }
    if run["providerInstanceId"] != projection["thread"]["providerInstanceId"] {
        return Err(StoreError::InvalidCommand(
            "Native queued provider handoff is not yet available.".into(),
        ));
    }
    let timestamp = at(now);
    if !model_selections_equal(
        &run["modelSelection"],
        &projection["thread"]["modelSelection"],
    ) {
        if let crate::provider_selection_transition::SelectionTransition::Reject { reason } =
            crate::provider_selection_transition::classify_selection(
                projection,
                &run["modelSelection"],
            )
        {
            return Err(StoreError::InvalidCommand(reason));
        }
        let mut thread = projection["thread"].clone();
        thread["modelSelection"] = run["modelSelection"].clone();
        thread["updatedAt"] = json!(timestamp);
        events.push(event(
            thread_id,
            &command_id,
            "thread.model-selection-updated",
            thread,
            now,
        ));
    }
    let mut provider = provider.clone();
    provider["status"] = json!("not_loaded");
    if provider["firstRunOrdinal"].is_null() {
        provider["firstRunOrdinal"] = run["ordinal"].clone();
    }
    provider["lastRunOrdinal"] = run["ordinal"].clone();
    provider["updatedAt"] = json!(timestamp);
    let mut starting = run.clone();
    starting["status"] = json!("starting");
    starting["queuePosition"] = Value::Null;
    starting["startedAt"] = Value::Null;
    let mut item = rows(projection,"turnItems").iter()
        .find(|row|row["type"] == "user_message" && row["runId"] == run["id"] && row["messageId"] == message["id"])
        .cloned().unwrap_or_else(|| json!({"id":format!("turn-item:message:{}",encode(message["id"].as_str().unwrap())),"threadId":thread_id,"runId":run["id"],"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":run["ordinal"].as_u64().unwrap()*100,"status":"completed","title":null,"type":"user_message","messageId":message["id"],"text":message["text"],"attachments":message["attachments"],"createdBy":message["createdBy"],"creationSource":message["creationSource"]}));
    for field in ["context", "scheduledTaskId", "senderThreadId"] {
        if let Some(value) = message.get(field) {
            item[field] = value.clone();
        }
    }
    item["inputIntent"] = json!("queued_turn");
    item["startedAt"] = json!(timestamp);
    item["completedAt"] = json!(timestamp);
    item["updatedAt"] = json!(timestamp);
    for (kind, payload) in [
        (
            "provider-thread.updated",
            checked::<t3_contracts::ProviderThread>(provider)?,
        ),
        (
            "turn-item.updated",
            checked::<t3_contracts::TurnItem>(item)?,
        ),
        ("run.updated", checked::<t3_contracts::Run>(starting)?),
    ] {
        events.push(event(thread_id, &command_id, kind, payload, now));
    }
    let effects = vec![NewEffect {
        id: format!(
            "effect:{command_id}:provider-turn.start:{}",
            run["id"].as_str().unwrap()
        ),
        command_id,
        thread_id: thread_id.into(),
        request: json!({"type":"provider-turn.start","runId":run["id"]}),
        available_at: timestamp,
    }];
    Ok(Decision::Accepted { events, effects })
}

fn settlement_command(projection: &Value, failed_run_id: Option<&str>) -> Option<String> {
    match admission(projection, failed_run_id) {
        Admission::Blocked => None,
        Admission::Hold => Some(format!(
            "command:system:hold-queue:{}",
            failed_run_id.unwrap()
        )),
        Admission::Start(run) => Some(format!(
            "command:system:start-queued:{}",
            run["id"].as_str().unwrap()
        )),
    }
}

/// Complete selections compare options canonically, as shared/model.ts does.
/// Stable locale sorting also preserves the source's typed-value distinction
/// when a boolean and an identically spelled string have equal sort keys.
fn model_selections_equal(left: &Value, right: &Value) -> bool {
    if left["instanceId"] != right["instanceId"] || left["model"] != right["model"] {
        return false;
    }
    let options = |selection: &Value| {
        let mut values: Vec<_> = selection["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| (option["id"].clone(), option["value"].clone()))
            .collect();
        let string = |value: &Value| match value.as_str() {
            Some(value) => value.to_owned(),
            None => value.to_string(),
        };
        values.sort_by(|(left_id, left_value), (right_id, right_value)| {
            crate::workspace_entries::collate(left_id.as_str().unwrap(), right_id.as_str().unwrap())
                .then_with(|| {
                    crate::workspace_entries::collate(&string(left_value), &string(right_value))
                })
        });
        values
    };
    options(left) == options(right)
}

/// The original catches delivery planning errors while it still owns the
/// selected queue entry. It fails that graph without emitting its user item or
/// starting a provider; subsequent waiting messages retain their own identity.
fn failed_start(
    projection: &Value,
    run: &Value,
    cause: &StoreError,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    let thread_id = projection["thread"]["id"].as_str().unwrap();
    let run_id = run["id"].as_str().unwrap();
    let command_id = format!("command:system:start-queued:{run_id}");
    let timestamp = at(now);
    let root = rows(projection, "nodes")
        .iter()
        .find(|root| root["id"] == run["rootNodeId"]);
    let attempt = rows(projection, "attempts")
        .iter()
        .find(|attempt| attempt["id"] == run["activeAttemptId"]);
    let provider = rows(projection, "providerThreads")
        .iter()
        .find(|provider| provider["id"] == run["providerThreadId"]);
    let mut events = vec![];
    if let Some(root) = root {
        if let Some(attempt) = attempt {
            let mut attempt = attempt.clone();
            attempt["status"] = json!("failed");
            attempt["completedAt"] = json!(timestamp);
            events.push(event(
                thread_id,
                &command_id,
                "run-attempt.updated",
                checked::<t3_contracts::RunAttempt>(attempt)?,
                now,
            ));
        }
        let mut root = root.clone();
        root["status"] = json!("failed");
        root["completedAt"] = json!(timestamp);
        events.push(event(
            thread_id,
            &command_id,
            "node.updated",
            checked::<t3_contracts::ExecutionNode>(root)?,
            now,
        ));
    }
    if let (Some(root), Some(provider)) = (root, provider) {
        let driver = provider["driver"].as_str().unwrap();
        let item = json!({"id":format!("turn-item:provider:{}:native-item:{}", encode(driver), encode(&format!("queued-start-failure:{run_id}"))),"threadId":thread_id,"runId":run_id,"nodeId":root["id"],"providerThreadId":provider["id"],"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":rows(projection,"turnItems").iter().filter_map(|item|item["ordinal"].as_u64()).map(|ordinal|ordinal+1).max().unwrap_or(0),"status":"failed","title":"Queued provider could not start","startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"error","failure":crate::provider_failure::make(&json!({"cause":cause.to_string(),"code":"queued_start_failed","class":"unknown"}))});
        events.push(event(
            thread_id,
            &command_id,
            "turn-item.updated",
            checked::<t3_contracts::TurnItem>(item)?,
            now,
        ));
    }
    let mut run = run.clone();
    run["status"] = json!("failed");
    run["queuePosition"] = Value::Null;
    run["completedAt"] = json!(timestamp);
    events.push(event(
        thread_id,
        &command_id,
        "run.updated",
        checked::<t3_contracts::Run>(run)?,
        now,
    ));
    Ok(Decision::Accepted {
        events,
        effects: vec![],
    })
}

/// Commit the next queued transition with its immutable system receipt and
/// deterministic outbox identity. Selection is checked again inside the same
/// transaction as planning; a concurrent higher-priority enqueue cannot cause
/// the wrong run to consume this receipt. Blocked checks persist no receipt.
/// Call only after terminal/error projection and provider ownership are settled.
pub(crate) fn settle(
    store: &Store,
    thread_id: &str,
    failed_run_id: Option<&str>,
) -> Result<bool, StoreError> {
    loop {
        let command_id = store.read(|connection| {
            let projection = read_projection(connection, "thread", thread_id)?
                .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            Ok(settlement_command(&projection, failed_run_id))
        })?;
        let Some(command_id) = command_id else {
            return Ok(false);
        };
        let now = Utc::now();
        let mut selection_changed = false;
        let result = store.dispatch_resolved(
            &command_id,
            "thread",
            "message.dispatch",
            now,
            |transaction, _| {
                let projection = read_projection(transaction, "thread", thread_id)?
                    .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                if settlement_command(&projection, failed_run_id).as_ref() != Some(&command_id) {
                    selection_changed = true;
                    return Err(StoreError::InvalidCommand(
                        "Queued admission changed before commit.".into(),
                    ));
                }
                Ok(thread_id.into())
            },
            |transaction, _| {
                let projection = read_projection(transaction, "thread", thread_id)?.unwrap();
                match promote(&projection, now, failed_run_id) {
                    Ok(decision) => Ok(decision),
                    Err(cause) => {
                        let selected = ordered(&projection).first().copied().unwrap();
                        tracing::warn!(thread_id, run_id = selected["id"].as_str().unwrap(), %cause, "Queued provider could not start");
                        failed_start(&projection, selected, &cause, now)
                    }
                }
            },
            crate::thread::reduce,
        );
        match result {
            Err(_) if selection_changed => continue,
            Ok((_, replayed)) => return Ok(!replayed),
            Err(error) => return Err(error),
        }
    }
}

fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(encoded, "%{byte:02X}").unwrap();
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution,
        launch::ThreadLaunchService,
        persistence::{Store, read_projection},
        project::ProjectService,
        thread,
    };

    #[test]
    fn original_queue_admission_and_delivery_order() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/message-queue.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let projection = &row["projection"];
            let order: Vec<_> = ordered(projection)
                .into_iter()
                .map(|run| run["id"].clone())
                .collect();
            assert_eq!(json!(order), row["order"], "queue ordering witness {count}");
            let result = match admission(projection, row["failedRunId"].as_str()) {
                Admission::Blocked => json!({"type":"blocked"}),
                Admission::Hold => {
                    json!({"type":"hold","runIds":rows(projection,"runs").iter().filter(|run|run["status"] == "queued").map(|run|run["id"].clone()).collect::<Vec<_>>()})
                }
                Admission::Start(run) => json!({"type":"start","runId":run["id"]}),
            };
            assert_eq!(result, row["result"], "queue admission witness {count}");
            count += 1;
        }
        assert_eq!(count, 150);
    }

    fn fixture() -> (Store, String) {
        let store = Store::memory().unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":"/private/tmp"}),Utc::now()).unwrap();
        let launched = ThreadLaunchService::new(store.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap();
        let id = launched["threadId"].as_str().unwrap().to_owned();
        let command = json!({"type":"message.dispatch","commandId":"prepared","threadId":id,"messageId":"prepared-message","text":"First","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"defer_start","workspaceStrategy":{"type":"root"}}});
        store
            .dispatch(
                "prepared",
                "thread",
                &id,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    execution::plan_message(
                        &command,
                        &read_projection(transaction, "thread", &id)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        (store, id)
    }

    fn enqueue(store: &Store, id: &str, key: &str) -> crate::persistence::Receipt {
        let command = json!({"type":"message.dispatch","commandId":key,"threadId":id,"messageId":format!("message:{key}"),"text":format!("Queued {key}"),"attachments":[],"createdBy":"agent","creationSource":"mcp","senderThreadId":"sender","dispatchMode":{"type":"queue_after_active"}});
        store
            .dispatch(
                key,
                "thread",
                id,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    let projection = read_projection(transaction, "thread", id)?.unwrap();
                    allocate(
                        &command,
                        &projection,
                        QueueContext {
                            provider_thread: &projection["providerThreads"][0],
                            new_provider_thread: false,
                            checkpoint_scope: None,
                        },
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap()
    }

    fn complete_first(store: &Store, id: &str) {
        store
            .dispatch(
                "complete",
                "thread",
                id,
                "provider.event",
                Utc::now(),
                |transaction| {
                    let projection = read_projection(transaction, "thread", id)?.unwrap();
                    let mut run = projection["runs"][0].clone();
                    run["status"] = json!("completed");
                    run["completedAt"] = json!(at(Utc::now()));
                    Ok(Decision::Accepted {
                        events: vec![event(id, "complete", "run.updated", run, Utc::now())],
                        effects: vec![],
                    })
                },
                thread::reduce,
            )
            .unwrap();
    }

    #[test]
    fn queued_graph_has_no_user_item_or_effect_until_atomic_promotion_and_replay() {
        let (store, id) = fixture();
        let receipt = enqueue(&store, &id, "queued");
        assert_eq!(receipt.status, "accepted");
        let projection = store.projection("thread", &id).unwrap().unwrap();
        let queued = &projection["runs"][1];
        assert_eq!(queued["status"], "queued");
        assert_eq!(queued["queuePosition"], 1);
        assert_eq!(projection["attempts"][1]["status"], "pending");
        assert_eq!(projection["nodes"][1]["status"], "pending");
        assert_eq!(projection["messages"][1]["senderThreadId"], "sender");
        assert!(
            !rows(&projection, "turnItems")
                .iter()
                .any(|item| item["messageId"] == "message:queued")
        );
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
        // Replaying enqueue does not allocate a second graph.
        assert_eq!(
            enqueue(&store, &id, "queued").result_sequence,
            receipt.result_sequence
        );
        complete_first(&store, &id);
        let command_id = format!(
            "command:system:start-queued:{}",
            queued["id"].as_str().unwrap()
        );
        let deliver = || {
            store
                .dispatch(
                    &command_id,
                    "thread",
                    &id,
                    "queued-run.start",
                    Utc::now(),
                    |transaction| {
                        promote(
                            &read_projection(transaction, "thread", &id)?.unwrap(),
                            Utc::now(),
                            None,
                        )
                    },
                    thread::reduce,
                )
                .unwrap()
        };
        let promoted = deliver();
        assert_eq!(promoted.status, "accepted");
        assert_eq!(deliver().result_sequence, promoted.result_sequence);
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(projection["runs"][1]["status"], "starting");
        assert!(projection["runs"][1]["queuePosition"].is_null());
        let item = rows(&projection, "turnItems")
            .iter()
            .find(|item| item["messageId"] == "message:queued")
            .unwrap();
        assert_eq!(item["inputIntent"], "queued_turn");
        assert_eq!(item["ordinal"], 200);
        assert_eq!(item["senderThreadId"], "sender");
        let claimed = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.request["runId"], queued["id"]);
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn held_queue_inherits_hold_and_does_not_materialize_any_transcript_item() {
        let (store, id) = fixture();
        enqueue(&store, &id, "first");
        store
            .dispatch(
                "hold",
                "thread",
                &id,
                "queue.hold",
                Utc::now(),
                |transaction| {
                    let projection = read_projection(transaction, "thread", &id)?.unwrap();
                    let mut run = projection["runs"][1].clone();
                    run["queueHeld"] = json!(true);
                    Ok(Decision::Accepted {
                        events: vec![event(&id, "hold", "run.updated", run, Utc::now())],
                        effects: vec![],
                    })
                },
                thread::reduce,
            )
            .unwrap();
        enqueue(&store, &id, "second");
        complete_first(&store, &id);
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(projection["runs"][2]["queueHeld"], true);
        assert_eq!(projection["runs"][2]["queuePosition"], 2);
        let Decision::Accepted { events, effects } =
            promote(&projection, Utc::now(), None).unwrap()
        else {
            panic!("wrong decision")
        };
        assert!(events.is_empty() && effects.is_empty());
        assert_eq!(rows(&projection, "turnItems").len(), 2);
    }

    fn acp_session(store: &Store, id: &str, command: &str, can_switch: bool) {
        store.dispatch(command, "thread", id, "provider.event", Utc::now(), |transaction| {
            let projection = read_projection(transaction, "thread", id)?.unwrap();
            let mut provider = projection["providerThreads"][0].clone();
            provider["driver"] = json!("acpRegistry");
            provider["providerSessionId"] = json!("session:acp");
            let session = json!({"id":"session:acp","providerInstanceId":projection["thread"]["modelSelection"]["instanceId"],"driver":"acpRegistry","status":"ready","updatedAt":at(Utc::now()),"capabilities":{"sessions":{"supportsModelSwitchInSession":can_switch},"turns":{"supportsQueuedMessages":true}}});
            Ok(Decision::Accepted { events:vec![event(id,command,"provider-thread.updated",provider,Utc::now()),event(id,command,"provider-session.updated",session,Utc::now())],effects:vec![] })
        }, thread::reduce).unwrap();
    }

    fn enqueue_model(store: &Store, id: &str, key: &str, model: Value) {
        let command = json!({"type":"message.dispatch","commandId":key,"threadId":id,"messageId":format!("message:{key}"),"text":key,"attachments":[],"createdBy":"user","creationSource":"web","modelSelection":model,"dispatchMode":{"type":"queue_after_active"}});
        store
            .dispatch(
                key,
                "thread",
                id,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    execution::plan_message_for_driver(
                        &command,
                        &read_projection(transaction, "thread", id)?.unwrap(),
                        Utc::now(),
                        "acpRegistry",
                    )
                },
                thread::reduce,
            )
            .unwrap();
    }

    #[test]
    fn changed_acp_capability_fails_only_selected_queue_graph_without_starting() {
        let (store, id) = fixture();
        acp_session(&store, &id, "initial-capabilities", true);
        enqueue_model(
            &store,
            &id,
            "rejected",
            json!({"instanceId":"codex","model":"changed-model"}),
        );
        enqueue_model(
            &store,
            &id,
            "later",
            json!({"instanceId":"codex","model":"fixture-model"}),
        );
        complete_first(&store, &id);
        // Delivery must use freshly negotiated capabilities, not enqueue-time consent.
        acp_session(&store, &id, "capabilities-changed", false);
        let before = store.projection("thread", &id).unwrap().unwrap();
        let selected = &before["runs"][1];
        let command = format!(
            "command:system:start-queued:{}",
            selected["id"].as_str().unwrap()
        );
        assert!(settle(&store, &id, None).unwrap());
        let after = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(after["runs"][0], before["runs"][0]);
        assert_eq!(after["runs"][2], before["runs"][2]);
        assert_eq!(
            after["thread"]["modelSelection"],
            before["thread"]["modelSelection"]
        );
        assert_eq!(after["runs"][1]["status"], "failed");
        assert!(after["runs"][1]["queuePosition"].is_null());
        assert!(after["runs"][1]["completedAt"].is_string());
        assert_eq!(after["attempts"][1]["status"], "failed");
        assert_eq!(after["nodes"][1]["status"], "failed");
        assert!(
            !rows(&after, "turnItems")
                .iter()
                .any(|item| item["messageId"] == "message:rejected"
                    || item["messageId"] == "message:later")
        );
        let error = rows(&after, "turnItems")
            .iter()
            .find(|item| item["runId"] == selected["id"] && item["type"] == "error")
            .unwrap();
        assert_eq!(
            error["failure"],
            json!({"class":"unknown","code":"queued_start_failed","message":"Provider turn failed.","retryable":null})
        );
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
        let receipt = store.receipt(&command).unwrap().unwrap();
        assert_eq!(receipt.status, "accepted");
        let sequence = store.latest_sequence().unwrap();
        let replay = store
            .dispatch(
                &command,
                "thread",
                &id,
                "message.dispatch",
                Utc::now(),
                |_| panic!("accepted failed-start receipt must not be replanned"),
                thread::reduce,
            )
            .unwrap();
        assert_eq!(receipt, replay);
        assert_eq!(sequence, store.latest_sequence().unwrap());
        assert_eq!(store.projection("thread", &id).unwrap().unwrap(), after);
    }

    #[test]
    fn queued_selection_compares_canonical_options_and_permits_options_only_change() {
        let (store, id) = fixture();
        enqueue(&store, &id, "queued");
        complete_first(&store, &id);
        let projection = store.projection("thread", &id).unwrap().unwrap();
        for (current, target) in [
            (
                json!({"instanceId":"codex","model":"fixture-model"}),
                json!({"instanceId":"codex","model":"fixture-model","options":[]}),
            ),
            (
                json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"b","value":"low"},{"id":"a","value":true}]}),
                json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"a","value":true},{"id":"b","value":"low"}]}),
            ),
        ] {
            let mut projection = projection.clone();
            projection["thread"]["modelSelection"] = current;
            projection["runs"][1]["modelSelection"] = target;
            let Decision::Accepted { events, effects } =
                promote(&projection, Utc::now(), None).unwrap()
            else {
                panic!("equivalent selection rejected")
            };
            assert!(
                !events
                    .iter()
                    .any(|event| event.event_type == "thread.model-selection-updated")
            );
            assert_eq!(effects.len(), 1);
        }
        let mut projection = projection;
        projection["providerSessions"] = json!([{"id":"session:acp","driver":"acpRegistry","providerInstanceId":"codex","status":"ready","capabilities":{"sessions":{"supportsModelSwitchInSession":false}}}]);
        projection["runs"][1]["modelSelection"] = json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"reasoningEffort","value":"high"}]});
        let Decision::Accepted { events, effects } =
            promote(&projection, Utc::now(), None).unwrap()
        else {
            panic!("turn-scoped options rejected")
        };
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "thread.model-selection-updated")
                .count(),
            1
        );
        assert_eq!(effects.len(), 1);
        assert!(!model_selections_equal(
            &json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"a","value":true}]}),
            &json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"a","value":"true"}]})
        ));
    }

    #[test]
    fn concurrent_settlement_commits_one_system_receipt_and_one_start_effect() {
        let (store, id) = fixture();
        enqueue(&store, &id, "queued");
        let sequence = store.latest_sequence().unwrap();
        assert!(!settle(&store, &id, None).unwrap());
        assert_eq!(store.latest_sequence().unwrap(), sequence);
        complete_first(&store, &id);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let jobs: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let id = id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    settle(&store, &id, None).unwrap()
                })
            })
            .collect();
        barrier.wait();
        let changed = jobs
            .into_iter()
            .map(|job| usize::from(job.join().unwrap()))
            .sum::<usize>();
        assert_eq!(changed, 1);
        let projection = store.projection("thread", &id).unwrap().unwrap();
        let queued = &projection["runs"][1];
        assert_eq!(queued["status"], "starting");
        let command_id = format!(
            "command:system:start-queued:{}",
            queued["id"].as_str().unwrap()
        );
        let receipt = store.receipt(&command_id).unwrap().unwrap();
        assert_eq!(receipt.status, "accepted");
        let sequence = store.latest_sequence().unwrap();
        assert!(!settle(&store, &id, None).unwrap());
        assert_eq!(store.latest_sequence().unwrap(), sequence);
        assert_eq!(store.receipt(&command_id).unwrap().unwrap(), receipt);
        let claimed = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.command_id, command_id);
        assert_eq!(claimed.request["runId"], queued["id"]);
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            rows(&projection, "turnItems")
                .iter()
                .filter(|item| item["messageId"] == "message:queued")
                .count(),
            1
        );
    }
}
