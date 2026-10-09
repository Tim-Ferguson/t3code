//! Durable steering decisions. Actor delivery owns native turn completion.
use crate::{
    execution::{at, checked, event},
    message_queue::model_selections_equal,
    persistence::{Decision, NewEffect, StoreError},
    provider_selection_transition::{SelectionTransition, selection_transition},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(projection: &'a Value, field: &str) -> &'a [Value] {
    projection[field]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn error(message: impl Into<String>) -> StoreError {
    StoreError::InvalidCommand(message.into())
}
fn encode(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}

/// Original CommandPolicy.resolveMessageDispatchIntent uses findLast, not the
/// first run or the greatest ordinal. Auto reads negotiated capabilities only.
pub(crate) fn resolve_dispatch_mode(command: &Value, projection: &Value) -> Value {
    let Some(intent) = command.get("deliveryIntent") else {
        return command["dispatchMode"].clone();
    };
    let Some(active) = rows(projection, "runs")
        .iter()
        .rev()
        .find(|run| crate::message_queue::blocking(run))
    else {
        return json!({"type":"start_immediately"});
    };
    match intent.as_str() {
        Some("steer") => return json!({"type":"steer_active","targetRunId":active["id"]}),
        Some("restart") => return json!({"type":"restart_active","targetRunId":active["id"]}),
        _ => {}
    }
    if matches!(active["status"].as_str(), Some("preparing" | "starting")) {
        return json!({"type":"queue_after_active"});
    }
    let session = rows(projection, "providerThreads")
        .iter()
        .find(|provider| provider["id"] == active["providerThreadId"])
        .and_then(|provider| {
            rows(projection, "providerSessions").iter().find(|session| {
                !provider["providerSessionId"].is_null()
                    && session["id"] == provider["providerSessionId"]
            })
        });
    let turns = session.map(|session| &session["capabilities"]["turns"]);
    if turns.is_some_and(|turns| turns["supportsActiveSteering"] == true) {
        json!({"type":"steer_active","targetRunId":active["id"]})
    } else if turns.is_some_and(|turns| turns["supportsQueuedMessages"] == true) {
        json!({"type":"queue_after_active"})
    } else if turns.is_some_and(|turns| turns["supportsSteeringByInterruptRestart"] == true) {
        json!({"type":"restart_active","targetRunId":active["id"]})
    } else {
        json!({"type":"queue_after_active"})
    }
}

/// Preserve a submission if its explicit steering target completed while the
/// client was still displaying it. A missing/interrupted target is not success.
pub(crate) fn completed_steer_target(projection: &Value, target_id: &Value) -> bool {
    let Some(run) = rows(projection, "runs")
        .iter()
        .find(|run| run["id"] == *target_id)
    else {
        return false;
    };
    if run["status"] == "completed" {
        return true;
    }
    if !matches!(run["status"].as_str(), Some("running" | "waiting"))
        || run["activeAttemptId"].is_null()
    {
        return false;
    }
    let mut latest: Option<&Value> = None;
    for turn in rows(projection, "providerTurns").iter().filter(|turn| {
        turn["nodeId"] == run["rootNodeId"] && turn["runAttemptId"] == run["activeAttemptId"]
    }) {
        if latest.is_none_or(|prior| {
            turn["ordinal"].as_u64().unwrap_or(0) > prior["ordinal"].as_u64().unwrap_or(0)
        }) {
            latest = Some(turn)
        }
    }
    latest.is_some_and(|turn| turn["status"] == "completed")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SteeringPolicy {
    ActiveSteering,
    InterruptRestart,
}
pub(crate) fn steering_policy(
    capabilities: &Value,
    force_restart: bool,
) -> Result<SteeringPolicy, StoreError> {
    let turns = &capabilities["turns"];
    if !force_restart && turns["supportsActiveSteering"] == true {
        Ok(SteeringPolicy::ActiveSteering)
    } else if turns["supportsInterrupt"] == true
        && turns["supportsSteeringByInterruptRestart"] == true
    {
        Ok(SteeringPolicy::InterruptRestart)
    } else {
        Err(error(if force_restart {
            "providerInstanceId cannot satisfy a required interrupt-and-restart"
        } else {
            "providerInstanceId cannot steer active turns directly or by interrupt-and-restart"
        }))
    }
}
fn maintenance(message: &Value) -> Option<String> {
    if !message["attachments"].as_array().is_some_and(Vec::is_empty) {
        return None;
    }
    let text =
        t3_contracts::trim_wire_string(message["text"].as_str().unwrap_or("")).to_lowercase();
    matches!(text.as_str(), "/compact" | "/logout").then_some(text)
}
fn goal(message: &Value) -> bool {
    if !message["attachments"].as_array().is_some_and(Vec::is_empty) {
        return false;
    }
    let text = t3_contracts::trim_wire_string(message["text"].as_str().unwrap_or(""));
    text.strip_prefix("/goal").is_some_and(|rest| {
        rest.is_empty()
            || rest.chars().next().is_some_and(|character| {
                t3_contracts::trim_wire_string(&character.to_string()).is_empty()
            })
    })
}

/// Source CheckpointService.prepareRootRunScope and ensureScope are pure.
/// RuntimePolicy.layerFromProjectStore is resolved in the command transaction.
pub(crate) fn prepare_root_scope(
    projection: &Value,
    run_id: &Value,
    node_id: &Value,
    provider_thread_id: &Value,
    fallback_cwd: Option<&str>,
    now: DateTime<Utc>,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Value, StoreError> {
    let cwd = if let Some(path) = projection["thread"]["worktreePath"].as_str() {
        path.to_owned()
    } else if let Some(transaction) = transaction {
        crate::persistence::read_projection(
            transaction,
            "project",
            projection["thread"]["projectId"].as_str().unwrap(),
        )?
        .filter(|project| project["deletedAt"].is_null())
        .and_then(|project| project["workspaceRoot"].as_str().map(str::to_owned))
        .ok_or_else(|| error("Project not found."))?
    } else {
        fallback_cwd
            .ok_or_else(|| error("No working directory for root checkpoint scope."))?
            .to_owned()
    };
    let thread_id = projection["thread"]["id"].as_str().unwrap();
    checked::<t3_contracts::CheckpointScope>(
        json!({"id":format!("checkpoint-scope:thread:{}:name:root",encode(thread_id)),"threadId":thread_id,"runId":run_id,"nodeId":node_id,"parentScopeId":null,"providerThreadId":provider_thread_id,"kind":"root_run","ordinalWithinParent":0,"advancesAppRunCount":true,"cwd":cwd,"createdAt":at(now)}),
    )
}

pub(crate) fn plan(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    let target_id = &command["dispatchMode"]["targetRunId"];
    let run = rows(projection, "runs")
        .iter()
        .find(|run| run["id"] == *target_id)
        .ok_or_else(|| {
            error(format!(
                "Target run {} was not found.",
                target_id.as_str().unwrap_or("")
            ))
        })?;
    if let Some(maintenance) = maintenance(command) {
        return Err(error(if maintenance == "/compact" {
            "Context compaction must run as a separate turn. Queue it or wait for the active turn to finish."
        } else {
            "Signing out must run as a separate turn. Queue it or wait for the active turn to finish."
        }));
    }
    if goal(command) {
        return Err(error(
            "Goal commands must run as a separate turn. Queue it or wait for the active turn to finish.",
        ));
    }
    if let Some(maintenance) = rows(projection, "messages")
        .iter()
        .find(|message| message["id"] == run["userMessageId"])
        .and_then(maintenance)
    {
        return Err(error(if maintenance == "/compact" {
            "Wait for context compaction to finish before steering the thread."
        } else {
            "Wait for sign-out to finish before steering the thread."
        }));
    }
    if run["rootNodeId"].is_null() {
        return Err(error(format!(
            "Target run {} has no root node.",
            run["id"].as_str().unwrap()
        )));
    }
    if run["status"] != "running" {
        return Err(error(format!(
            "Target run {} is {} and cannot be steered.",
            run["id"].as_str().unwrap(),
            run["status"].as_str().unwrap_or("")
        )));
    }
    let provider = rows(projection, "providerThreads")
        .iter()
        .find(|provider| provider["id"] == run["providerThreadId"])
        .filter(|provider| !provider["providerSessionId"].is_null())
        .ok_or_else(|| {
            error(format!(
                "Provider thread {} has no active provider session for steering.",
                run["providerThreadId"].as_str().unwrap_or("")
            ))
        })?;
    let turn = rows(projection, "providerTurns")
        .iter()
        .find(|turn| turn["runAttemptId"] == run["activeAttemptId"] && turn["status"] == "running")
        .ok_or_else(|| {
            error(format!(
                "No running provider turn found for active run {}.",
                run["id"].as_str().unwrap()
            ))
        })?;
    let session = rows(projection, "providerSessions")
        .iter()
        .find(|session| {
            session["id"] == provider["providerSessionId"]
                && !matches!(session["status"].as_str(), Some("stopped" | "error"))
        })
        .ok_or_else(|| {
            error(format!(
                "Provider session {} is not active.",
                provider["providerSessionId"].as_str().unwrap()
            ))
        })?;
    let model = command
        .get("modelSelection")
        .unwrap_or(&projection["thread"]["modelSelection"]);
    if model["instanceId"] != run["providerInstanceId"] {
        return Err(error(
            "Native provider switching via handoff is not yet available.",
        ));
    }
    let changed = !model_selections_equal(&run["modelSelection"], model);
    if changed {
        if let SelectionTransition::Reject { reason } = selection_transition(
            provider["driver"].as_str().unwrap_or(""),
            &run["modelSelection"],
            model,
            &session["capabilities"],
        ) {
            return Err(error(reason));
        }
    }
    let force_restart = command["dispatchMode"]["type"] == "restart_active"
        || (changed
            && session["capabilities"]["turns"]["supportsInterrupt"] == true
            && session["capabilities"]["turns"]["supportsSteeringByInterruptRestart"] == true);
    let policy = steering_policy(&session["capabilities"], force_restart)?;
    let timestamp = at(now);
    let thread_id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    let mut events = vec![];
    let mut next_run = run.clone();
    let mut message_node = run["rootNodeId"].clone();
    let mut message_turn = turn["id"].clone();
    let request = match policy {
        SteeringPolicy::ActiveSteering => {
            json!({"type":"provider-turn.steer","providerSessionId":session["id"],"providerThreadId":provider["id"],"providerTurnId":turn["id"],"messageId":command["messageId"]})
        }
        SteeringPolicy::InterruptRestart => {
            if run["activeAttemptId"].is_null() {
                return Err(error(format!(
                    "Active run {} has no attempt to interrupt.",
                    run["id"].as_str().unwrap()
                )));
            }
            let ordinal = rows(projection, "attempts")
                .iter()
                .filter(|attempt| attempt["runId"] == run["id"])
                .filter_map(|attempt| attempt["attemptOrdinal"].as_u64())
                .max()
                .unwrap_or(0)
                + 1;
            let encoded = encode(run["id"].as_str().unwrap());
            let attempt_id = format!("run-attempt:run:{encoded}:attempt:{ordinal}");
            let node_id = format!("node:run:{encoded}:attempt:{ordinal}:root");
            if let Some(attempt) = rows(projection, "attempts")
                .iter()
                .find(|attempt| attempt["id"] == run["activeAttemptId"])
            {
                let mut attempt = attempt.clone();
                attempt["status"] = json!("superseded");
                attempt["completedAt"] = json!(timestamp);
                events.push(event(
                    thread_id,
                    command_id,
                    "run-attempt.updated",
                    checked::<t3_contracts::RunAttempt>(attempt)?,
                    now,
                ));
            }
            if let Some(root) = rows(projection, "nodes")
                .iter()
                .find(|root| root["id"] == run["rootNodeId"])
            {
                let mut root = root.clone();
                root["status"] = json!("interrupted");
                root["completedAt"] = json!(timestamp);
                events.push(event(
                    thread_id,
                    command_id,
                    "node.updated",
                    checked::<t3_contracts::ExecutionNode>(root)?,
                    now,
                ));
            }
            let scope = prepare_root_scope(
                projection,
                &run["id"],
                &json!(node_id),
                &provider["id"],
                session["cwd"].as_str(),
                now,
                transaction,
            )?;
            let scope_id = scope["id"].clone();
            next_run["modelSelection"] = model.clone();
            next_run["rootNodeId"] = json!(node_id);
            next_run["activeAttemptId"] = json!(attempt_id);
            next_run["userMessageId"] = command["messageId"].clone();
            next_run["status"] = json!("starting");
            events.push(event(
                thread_id,
                command_id,
                "run.updated",
                checked::<t3_contracts::Run>(next_run.clone())?,
                now,
            ));
            let attempt = checked::<t3_contracts::RunAttempt>(
                json!({"id":attempt_id,"runId":run["id"],"attemptOrdinal":ordinal,"rootNodeId":node_id,"providerInstanceId":run["providerInstanceId"],"providerThreadId":provider["id"],"providerTurnId":null,"reason":"steering_restart","status":"pending","startedAt":null,"completedAt":null}),
            )?;
            events.push(event(
                thread_id,
                command_id,
                "run-attempt.created",
                attempt,
                now,
            ));
            let root = checked::<t3_contracts::ExecutionNode>(
                json!({"id":node_id,"threadId":thread_id,"runId":run["id"],"parentNodeId":null,"rootNodeId":node_id,"kind":"root_turn","status":"pending","countsForRun":true,"providerThreadId":provider["id"],"providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":scope_id,"startedAt":null,"completedAt":null}),
            )?;
            events.push(event(thread_id, command_id, "node.updated", root, now));
            events.push(event(
                thread_id,
                command_id,
                "checkpoint-scope.created",
                scope,
                now,
            ));
            message_node = json!(node_id);
            message_turn = Value::Null;
            json!({"type":"provider-turn.restart","providerSessionId":session["id"],"providerThreadId":provider["id"],"providerTurnId":turn["id"],"interruptedAttemptId":run["activeAttemptId"],"runId":run["id"]})
        }
    };
    // Active steering updates the saved choice while the running run keeps its
    // original selection. Restart updates both. Delegated delivery never does.
    if command.get("delegatedCompletion").is_none()
        && !model_selections_equal(&projection["thread"]["modelSelection"], model)
    {
        let mut thread = projection["thread"].clone();
        thread["modelSelection"] = model.clone();
        thread["updatedAt"] = json!(timestamp);
        events.push(event(
            thread_id,
            command_id,
            "thread.model-selection-updated",
            thread,
            now,
        ));
    }
    let mut message = json!({"id":command["messageId"],"threadId":thread_id,"runId":run["id"],"nodeId":message_node,"role":"user","text":command["text"],"attachments":command["attachments"],"createdBy":command["createdBy"],"creationSource":command["creationSource"],"streaming":false,"createdAt":timestamp,"updatedAt":timestamp});
    let mut item = json!({"id":format!("turn-item:message:{}",encode(command["messageId"].as_str().unwrap())),"threadId":thread_id,"runId":run["id"],"nodeId":message_node,"providerThreadId":provider["id"],"providerTurnId":message_turn,"nativeItemRef":null,"parentItemId":null,"ordinal":rows(projection,"turnItems").iter().filter_map(|item|item["ordinal"].as_u64()).max().map_or(0,|ordinal|ordinal+1),"status":"completed","title":null,"startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"user_message","messageId":command["messageId"],"inputIntent":command.get("inputIntent").cloned().unwrap_or(json!("steer")),"text":command["text"],"attachments":command["attachments"],"createdBy":command["createdBy"],"creationSource":command["creationSource"]});
    for field in [
        "senderThreadId",
        "scheduledTaskId",
        "delegatedCompletion",
        "context",
    ] {
        if let Some(value) = command.get(field) {
            message[field] = value.clone();
            if field != "delegatedCompletion" {
                item[field] = value.clone()
            }
        }
    }
    events.push(event(
        thread_id,
        command_id,
        "message.updated",
        checked::<t3_contracts::ConversationMessage>(message)?,
        now,
    ));
    events.push(event(
        thread_id,
        command_id,
        "turn-item.updated",
        checked::<t3_contracts::TurnItem>(item)?,
        now,
    ));
    let effect = NewEffect {
        id: format!(
            "effect:{command_id}:{}:{}",
            request["type"].as_str().unwrap(),
            turn["id"].as_str().unwrap()
        ),
        command_id: command_id.into(),
        thread_id: thread_id.into(),
        request,
        available_at: timestamp,
    };
    Ok(Decision::Accepted {
        events,
        effects: vec![effect],
    })
}

/// `queued-message.promote-to-steer`: a message waiting in the queue is delivered into the active
/// run as steering instead. The queued run ends cancelled without having started, and the message
/// joins the target run exactly as a steered message does (`dispatchQueuedMessagePromoteToSteer`).
/// Automatic completion deliveries are not the user's to promote.
pub(crate) fn plan_promote(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    transaction: Option<&rusqlite::Transaction<'_>>,
) -> Result<Decision, StoreError> {
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    if !projection["thread"]["archivedAt"].is_null() || !projection["thread"]["deletedAt"].is_null() {
        return Err(error(format!("Thread {thread_id} is not active.")));
    }
    let queued_id = command["queuedRunId"].as_str().unwrap_or_default();
    let run = rows(projection, "runs")
        .iter()
        .find(|run| run["id"] == command["queuedRunId"] && run["status"] == "queued")
        .ok_or_else(|| error(format!("Queued run {queued_id} is not queued.")))?;
    let find = |field: &str, id: &Value| rows(projection, field).iter().find(|row| !id.is_null() && row["id"] == *id);
    let (Some(_), Some(_), Some(message)) = (find("nodes", &run["rootNodeId"]), find("attempts", &run["activeAttemptId"]), find("messages", &run["userMessageId"])) else {
        return Err(error(format!("Queued run {queued_id} is missing message or execution state.")));
    };
    if message.get("delegatedCompletion").is_some_and(|value| !value.is_null()) || message.get("notification").is_some_and(|value| !value.is_null()) {
        return Err(error("Automatic completion deliveries cannot be promoted to Steer."));
    }
    let cancel = json!({"type":"queued-run.cancel","commandId":command["commandId"],"threadId":thread_id,"runId":queued_id});
    let Decision::Accepted { mut events, .. } = crate::queued_runs::plan_cancel(&cancel, projection, now)? else {
        return Err(error(format!("Queued run {queued_id} is not queued.")));
    };
    let mut steer = json!({"commandId":command["commandId"],"threadId":thread_id,"messageId":message["id"],"text":message["text"],"attachments":message["attachments"],"createdBy":message["createdBy"],"creationSource":message["creationSource"],"dispatchMode":{"type":"steer_active","targetRunId":command["targetRunId"]},"inputIntent":"promoted_queued_to_steer"});
    for field in ["scheduledTaskId", "senderThreadId", "context"] {
        if let Some(value) = message.get(field).filter(|value| !value.is_null()) {
            steer[field] = value.clone();
        }
    }
    match plan(&steer, projection, now, transaction)? {
        Decision::Accepted { events: steered, effects } => {
            events.extend(steered);
            Ok(Decision::Accepted { events, effects })
        }
        rejected => Ok(rejected),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution,
        launch::ThreadLaunchService,
        persistence::{Receipt, Store, read_projection},
        project::ProjectService,
        thread,
    };

    #[test]
    fn original_auto_completed_target_and_steering_policy_decisions() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/message-steering.json")).unwrap();
        assert_eq!(cases.len(), 363);
        for (index, case) in cases.iter().enumerate() {
            let actual = match case["operation"].as_str().unwrap() {
                "intent" => resolve_dispatch_mode(&case["command"], &case["projection"]),
                "completed" => {
                    if completed_steer_target(&case["projection"], &case["mode"]["targetRunId"]) {
                        json!({"type":"start_immediately"})
                    } else {
                        case["mode"].clone()
                    }
                }
                "policy" => match steering_policy(
                    &case["input"]["capabilities"],
                    case["input"]["forceRestart"].as_bool().unwrap(),
                ) {
                    Ok(SteeringPolicy::ActiveSteering) => json!({"policy":"active_steering"}),
                    Ok(SteeringPolicy::InterruptRestart) => json!({"policy":"interrupt_restart"}),
                    Err(StoreError::InvalidCommand(message)) => json!({"error":message}),
                    Err(error) => panic!("Unexpected error: {error}"),
                },
                _ => unreachable!(),
            };
            assert_eq!(actual, case["output"], "source witness {index}: {case}");
        }
    }

    fn active_store(driver: &str, capabilities: Value) -> (Store, String) {
        let store = Store::memory().unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":"/private/tmp"}),Utc::now()).unwrap();
        let launched=ThreadLaunchService::new(store.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap();
        let id = launched["threadId"].as_str().unwrap().to_owned();
        let command = json!({"type":"message.dispatch","commandId":"first","threadId":id,"messageId":"message:first","text":"First","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"defer_start","workspaceStrategy":{"type":"root"}}});
        dispatch(&store, &id, &command, driver).unwrap();
        store.dispatch("native-start","thread",&id,"provider.event",Utc::now(),|transaction| {
            let projection=read_projection(transaction,"thread",&id)?.unwrap();
            let timestamp=at(Utc::now());
            let mut run=projection["runs"][0].clone();run["status"]=json!("running");run["startedAt"]=json!(timestamp);
            let mut attempt=projection["attempts"][0].clone();attempt["status"]=json!("running");attempt["providerTurnId"]=json!("turn:first");attempt["startedAt"]=json!(timestamp);
            let mut node=projection["nodes"][0].clone();node["status"]=json!("running");node["providerTurnId"]=json!("turn:first");node["startedAt"]=json!(timestamp);
            let mut provider=projection["providerThreads"][0].clone();provider["providerSessionId"]=json!("session:first");provider["nativeThreadRef"]=json!({"driver":driver,"nativeId":"native-thread","strength":"strong"});provider["status"]=json!("running");
            let session=json!({"id":"session:first","driver":driver,"providerInstanceId":"codex","status":"running","cwd":"/private/tmp","model":"fixture-model","capabilities":capabilities,"createdAt":timestamp,"updatedAt":timestamp,"lastError":null});
            let turn=json!({"id":"turn:first","providerThreadId":provider["id"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":null,"ordinal":1,"status":"running","startedAt":timestamp,"completedAt":null});
            let mut events=vec![];
            for (kind,payload) in [("run.updated",run),("run-attempt.updated",attempt),("node.updated",node),("provider-thread.updated",provider),("provider-session.updated",session),("provider-turn.updated",turn)] {events.push(event(&id,"native-start",kind,payload,Utc::now()))}
            Ok(Decision::Accepted {events,effects:vec![]})
        },thread::reduce).unwrap();
        (store, id)
    }
    fn dispatch(
        store: &Store,
        id: &str,
        command: &Value,
        driver: &str,
    ) -> Result<Receipt, StoreError> {
        store.dispatch(
            command["commandId"].as_str().unwrap(),
            "thread",
            id,
            "message.dispatch",
            Utc::now(),
            |transaction| {
                execution::plan_message_in_transaction(
                    command,
                    &read_projection(transaction, "thread", id)?.unwrap(),
                    transaction,
                    Utc::now(),
                    driver,
                )
            },
            thread::reduce,
        )
    }
    fn steer(store: &Store, id: &str, key: &str, mode: &str) -> Value {
        let projection = store.projection("thread", id).unwrap().unwrap();
        json!({"type":"message.dispatch","commandId":key,"threadId":id,"messageId":format!("message:{key}"),"text":"Continue with this instruction","attachments":[],"createdBy":"agent","creationSource":"mcp","senderThreadId":"trusted-parent","dispatchMode":{"type":mode,"targetRunId":projection["runs"][0]["id"]}})
    }
    fn finish(store: &Store, id: &str) {
        store
            .dispatch(
                "native-finish",
                "thread",
                id,
                "provider.event",
                Utc::now(),
                |transaction| {
                    let projection = read_projection(transaction, "thread", id)?.unwrap();
                    let mut events = vec![];
                    for (field, kind) in [
                        ("runs", "run.updated"),
                        ("attempts", "run-attempt.updated"),
                        ("nodes", "node.updated"),
                        ("providerTurns", "provider-turn.updated"),
                    ] {
                        let mut row = projection[field][0].clone();
                        row["status"] = json!("completed");
                        row["completedAt"] = json!(at(Utc::now()));
                        events.push(event(id, "native-finish", kind, row, Utc::now()));
                    }
                    Ok(Decision::Accepted {
                        events,
                        effects: vec![],
                    })
                },
                thread::reduce,
            )
            .unwrap();
    }

    #[test]
    fn active_steer_joins_one_run_and_receipt_replay_adds_no_effect() {
        let capabilities: Value =
            serde_json::from_str(include_str!("codex-capabilities.json")).unwrap();
        let (store, id) = active_store("codex", capabilities);
        let command = steer(&store, &id, "steer", "steer_active");
        let before = store.projection("thread", &id).unwrap().unwrap();
        let receipt = dispatch(&store, &id, &command, "codex").unwrap();
        assert_eq!(dispatch(&store, &id, &command, "codex").unwrap(), receipt);
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(projection["runs"], before["runs"]);
        assert_eq!(projection["attempts"], before["attempts"]);
        let message = rows(&projection, "messages")
            .iter()
            .find(|message| message["id"] == "message:steer")
            .unwrap();
        assert_eq!(message["runId"], projection["runs"][0]["id"]);
        assert_eq!(message["senderThreadId"], "trusted-parent");
        let item = rows(&projection, "turnItems")
            .iter()
            .find(|item| item["messageId"] == "message:steer")
            .unwrap();
        assert_eq!(item["inputIntent"], "steer");
        assert_eq!(item["providerTurnId"], "turn:first");
        let effect = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            effect.request,
            json!({"type":"provider-turn.steer","providerSessionId":"session:first","providerThreadId":before["providerThreads"][0]["id"],"providerTurnId":"turn:first","messageId":"message:steer"})
        );
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restart_retains_run_and_supersedes_attempt_with_root_scope_and_captured_effect() {
        let capabilities: Value =
            serde_json::from_str(include_str!("acp-capabilities.json")).unwrap();
        let (store, id) = active_store("acpRegistry", capabilities);
        let command = steer(&store, &id, "restart", "restart_active");
        let before = store.projection("thread", &id).unwrap().unwrap();
        let receipt = dispatch(&store, &id, &command, "acpRegistry").unwrap();
        assert_eq!(
            dispatch(&store, &id, &command, "acpRegistry").unwrap(),
            receipt
        );
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(rows(&projection, "runs").len(), 1);
        assert_eq!(projection["runs"][0]["id"], before["runs"][0]["id"]);
        assert_eq!(projection["runs"][0]["status"], "starting");
        assert_eq!(
            projection["runs"][0]["startedAt"],
            before["runs"][0]["startedAt"]
        );
        assert_eq!(projection["attempts"][0]["status"], "superseded");
        assert_eq!(projection["nodes"][0]["status"], "interrupted");
        assert_eq!(projection["attempts"][1]["attemptOrdinal"], 2);
        assert_eq!(projection["attempts"][1]["reason"], "steering_restart");
        assert_eq!(projection["attempts"][1]["status"], "pending");
        assert_eq!(
            projection["nodes"][1]["checkpointScopeId"],
            projection["checkpointScopes"][0]["id"]
        );
        assert_eq!(
            projection["checkpointScopes"][0]["nodeId"],
            projection["nodes"][1]["id"]
        );
        assert_eq!(projection["checkpointScopes"][0]["cwd"], "/private/tmp");
        let item = rows(&projection, "turnItems")
            .iter()
            .find(|item| item["messageId"] == "message:restart")
            .unwrap();
        assert_eq!(item["inputIntent"], "steer");
        assert!(item["providerTurnId"].is_null());
        let effect = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            effect.request,
            json!({"type":"provider-turn.restart","providerSessionId":"session:first","providerThreadId":before["providerThreads"][0]["id"],"providerTurnId":"turn:first","interruptedAttemptId":before["attempts"][0]["id"],"runId":before["runs"][0]["id"]})
        );
    }

    #[test]
    fn changed_turn_selection_restarts_when_supported_or_saves_next_turn_choice() {
        for can_restart in [false, true] {
            let mut capabilities: Value =
                serde_json::from_str(include_str!("codex-capabilities.json")).unwrap();
            capabilities["turns"]["supportsInterrupt"] = json!(can_restart);
            let (store, id) = active_store("codex", capabilities);
            let before = store.projection("thread", &id).unwrap().unwrap();
            let mut command = steer(&store, &id, "changed-options", "steer_active");
            command["modelSelection"] = json!({"instanceId":"codex","model":"fixture-model","options":[{"id":"reasoningEffort","value":"high"}]});
            dispatch(&store, &id, &command, "codex").unwrap();
            let after = store.projection("thread", &id).unwrap().unwrap();
            assert_eq!(after["thread"]["modelSelection"], command["modelSelection"]);
            let effect = store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .unwrap();
            if can_restart {
                assert_eq!(
                    after["runs"][0]["modelSelection"],
                    command["modelSelection"]
                );
                assert_eq!(after["attempts"][0]["status"], "superseded");
                assert_eq!(effect.request["type"], "provider-turn.restart");
            } else {
                assert_eq!(after["runs"], before["runs"]);
                assert_eq!(after["attempts"], before["attempts"]);
                assert_eq!(effect.request["type"], "provider-turn.steer");
            }
        }
    }

    #[test]
    fn restart_scope_reads_current_project_in_same_transaction_not_old_session_cwd() {
        let capabilities: Value =
            serde_json::from_str(include_str!("acp-capabilities.json")).unwrap();
        let (store, id) = active_store("acpRegistry", capabilities);
        let directory = tempfile::tempdir().unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.meta.update","commandId":"move-project","projectId":"project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        let command = steer(&store, &id, "restart-fresh-cwd", "restart_active");
        dispatch(&store, &id, &command, "acpRegistry").unwrap();
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(projection["providerSessions"][0]["cwd"], "/private/tmp");
        assert_eq!(
            projection["checkpointScopes"][0]["cwd"],
            json!(directory.path())
        );
    }

    #[test]
    fn steering_refusals_do_not_mutate_graph_and_completed_target_starts_followup() {
        let capabilities: Value =
            serde_json::from_str(include_str!("acp-capabilities.json")).unwrap();
        let (store, id) = active_store("acpRegistry", capabilities);
        let before = store.projection("thread", &id).unwrap().unwrap();
        for (key, text) in [
            ("compact", "\u{feff}/COMPACT\u{00a0}"),
            ("logout", "/logout"),
            ("goal", "/goal\u{2028}new goal"),
        ] {
            let mut command = steer(&store, &id, key, "steer_active");
            command["text"] = json!(text);
            assert!(dispatch(&store, &id, &command, "acpRegistry").is_err());
            assert_eq!(store.projection("thread", &id).unwrap().unwrap(), before);
        }
        let mut changed = steer(&store, &id, "changed-model", "restart_active");
        changed["modelSelection"] = json!({"instanceId":"codex","model":"another-model"});
        assert!(
            matches!(dispatch(&store,&id,&changed,"acpRegistry"),Err(StoreError::InvalidCommand(message)) if message == "The active ACP session does not expose a model-switch capability.")
        );
        assert_eq!(store.projection("thread", &id).unwrap().unwrap(), before);
        let mut missing = steer(&store, &id, "missing", "steer_active");
        missing["dispatchMode"]["targetRunId"] = json!("does-not-exist");
        assert!(
            matches!(dispatch(&store,&id,&missing,"acpRegistry"),Err(StoreError::InvalidCommand(message)) if message == "Target run does-not-exist was not found.")
        );
        finish(&store, &id);
        let command = steer(&store, &id, "late", "steer_active");
        dispatch(&store, &id, &command, "acpRegistry").unwrap();
        let after = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(after["runs"][0]["status"], "completed");
        assert_eq!(after["runs"][1]["status"], "starting");
        assert_eq!(
            rows(&after, "messages")
                .iter()
                .filter(|message| message["id"] == "message:late")
                .count(),
            1
        );
        let effect = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(effect.request["type"], "provider-turn.start");
    }

    #[test]
    fn late_steer_follow_up_queues_behind_new_active_turn_without_duplicate_item() {
        let capabilities: Value =
            serde_json::from_str(include_str!("codex-capabilities.json")).unwrap();
        let (store, id) = active_store("codex", capabilities);
        let command = steer(&store, &id, "late-queued", "steer_active");
        dispatch(&store, &id, &command, "codex").unwrap();
        let effect = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        finish(&store, &id);
        let mut other = steer(&store, &id, "other", "start_immediately");
        other["dispatchMode"] = json!({"type":"start_immediately"});
        dispatch(&store, &id, &other, "codex").unwrap();
        let before = store.projection("thread", &id).unwrap().unwrap();
        let receipt = execution::follow_up_completed_steer(&store, &effect).unwrap();
        assert_eq!(
            execution::follow_up_completed_steer(&store, &effect).unwrap(),
            receipt
        );
        let after = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(after["runs"][0], before["runs"][0]);
        assert_eq!(after["runs"][1], before["runs"][1]);
        assert_eq!(after["runs"][2]["status"], "queued");
        assert_eq!(after["runs"][2]["userMessageId"], "message:late-queued");
        let item = rows(&after, "turnItems")
            .iter()
            .find(|item| item["messageId"] == "message:late-queued")
            .unwrap();
        assert_eq!(item["runId"], after["runs"][2]["id"]);
        assert_eq!(item["inputIntent"], "queued_turn");
        assert_eq!(
            rows(&after, "turnItems")
                .iter()
                .filter(|item| item["messageId"] == "message:late-queued")
                .count(),
            1
        );
        store
            .finish_effect(&effect.id, "worker", Utc::now(), Ok(()), None)
            .unwrap();
        let start = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(start.request["runId"], after["runs"][1]["id"]);
        store
            .finish_effect(&start.id, "worker", Utc::now(), Ok(()), None)
            .unwrap();
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn late_delivery_reuses_only_durable_steer_message_once_and_refuses_forgery() {
        let capabilities: Value =
            serde_json::from_str(include_str!("codex-capabilities.json")).unwrap();
        let (store, id) = active_store("codex", capabilities);
        let command = steer(&store, &id, "late-delivery", "steer_active");
        dispatch(&store, &id, &command, "codex").unwrap();
        let effect = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert!(execution::follow_up_completed_steer(&store, &effect).is_err());
        finish(&store, &id);
        let mut forged = effect.clone();
        forged.request["messageId"] = json!("message:first");
        assert!(execution::follow_up_completed_steer(&store, &forged).is_err());
        let mut public = command.clone();
        public["commandId"] = json!("public-reuse");
        public["dispatchMode"] = json!({"type":"start_immediately"});
        public["reuseMessage"] = json!(true);
        assert!(
            matches!(dispatch(&store,&id,&public,"codex"),Err(StoreError::InvalidCommand(message)) if message == "Message id already exists.")
        );
        let receipt = execution::follow_up_completed_steer(&store, &effect).unwrap();
        assert_eq!(
            execution::follow_up_completed_steer(&store, &effect).unwrap(),
            receipt
        );
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(rows(&projection, "runs").len(), 2);
        let message = rows(&projection, "messages")
            .iter()
            .find(|message| message["id"] == "message:late-delivery")
            .unwrap();
        assert_eq!(message["runId"], projection["runs"][1]["id"]);
        assert_eq!(
            rows(&projection, "turnItems")
                .iter()
                .filter(|item| item["messageId"] == "message:late-delivery")
                .count(),
            1
        );
        store
            .finish_effect(&effect.id, "worker", Utc::now(), Ok(()), None)
            .unwrap();
        let start = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(start.request["type"], "provider-turn.start");
        assert_eq!(start.request["runId"], projection["runs"][1]["id"]);
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
    }
}
