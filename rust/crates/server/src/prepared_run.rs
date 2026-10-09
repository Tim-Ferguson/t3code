//! `prepared-run.progress`, `prepared-run.fail` and `prepared-run.retry`: the commands that
//! carry a run through workspace preparation (`dispatchPreparedRunProgress`,
//! `dispatchPreparedRunFail`, `dispatchPreparedRunRetry` in `Orchestrator.ts`). Release is
//! planned in `execution.rs`. Retry also schedules the preparation again, in the same commit.
use crate::{
    execution::{at, checked, event},
    persistence::{Decision, NewEffect, StoreError},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// The turn item every deferred run shows while its workspace is prepared.
pub(crate) const PREPARING: &str = "Preparing workspace";
/// `ORCHESTRATION_V2_WORKSPACE_PREPARATION_FAILURE_CODE`.
pub(crate) const FAILURE_CODE: &str = "workspace_preparation_failed";

fn reject(detail: String) -> StoreError {
    StoreError::InvalidCommand(detail)
}

struct State<'a> {
    run: &'a Value,
    attempt: &'a Value,
    root: &'a Value,
    provider_thread: &'a Value,
    preparation: &'a Value,
}

/// `preparedRunState`: the run in the status the command needs, with the rows it rewrites.
fn state<'a>(command: &Value, projection: &'a Value) -> Option<State<'a>> {
    let rows = |field: &str| projection[field].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let run = rows("runs").iter().find(|run| run["id"] == command["runId"])?;
    let expected = if command["type"] == "prepared-run.retry" {
        "failed"
    } else {
        "preparing"
    };
    if run["status"] != expected {
        return None;
    }
    let attempt = rows("attempts").iter().find(|row| row["id"] == run["activeAttemptId"])?;
    let root = rows("nodes").iter().find(|row| row["id"] == run["rootNodeId"])?;
    let provider_thread = rows("providerThreads").iter().find(|row| row["id"] == run["providerThreadId"])?;
    let preparation = rows("turnItems").iter().find(|item| {
        item["runId"] == command["runId"] && item["type"] == "command_execution" && item["input"] == PREPARING
    })?;
    Some(State { run, attempt, root, provider_thread, preparation })
}

fn not_awaiting(command: &Value) -> StoreError {
    reject(format!(
        "Run {} is not awaiting workspace preparation.",
        command["runId"].as_str().unwrap_or("")
    ))
}

pub(crate) fn plan_progress(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let state = state(command, projection).ok_or_else(|| not_awaiting(command))?;
    let mut item = state.preparation.clone();
    item["title"] = json!(if command["phase"] == "worktree" { "Preparing worktree" } else { "Starting setup script" });
    item["updatedAt"] = json!(at(now));
    Ok(Decision::Accepted {
        events: vec![event(
            command["threadId"].as_str().unwrap(),
            command["commandId"].as_str().unwrap(),
            "turn-item.updated",
            checked::<t3_contracts::TurnItem>(item)?,
            now,
        )],
        effects: vec![],
    })
}

fn percent_encode(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

pub(crate) fn plan_fail(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let state = state(command, projection).ok_or_else(|| not_awaiting(command))?;
    let (thread_id, command_id) = (command["threadId"].as_str().unwrap(), command["commandId"].as_str().unwrap());
    let timestamp = at(now);
    let run_id = state.run["id"].as_str().unwrap();
    let mut attempt = state.attempt.clone();
    attempt["status"] = json!("failed");
    attempt["completedAt"] = json!(timestamp);
    let mut root = state.root.clone();
    root["status"] = json!("failed");
    root["completedAt"] = json!(timestamp);
    let mut preparation = state.preparation.clone();
    preparation["status"] = json!("failed");
    preparation["title"] = json!("Workspace preparation failed");
    preparation["output"] = command["failure"]["message"].clone();
    preparation["exitCode"] = json!(1);
    preparation["completedAt"] = json!(timestamp);
    preparation["updatedAt"] = json!(timestamp);
    let ordinal = projection["turnItems"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["ordinal"].as_u64())
        .max()
        .map_or(0, |ordinal| ordinal + 1);
    let driver = state.provider_thread["driver"].as_str().unwrap_or("");
    let mut failure = command["failure"].clone();
    failure["code"] = json!(FAILURE_CODE);
    let error = json!({
        "id": format!("turn-item:provider:{}:native-item:{}", percent_encode(driver), percent_encode(&format!("workspace-preparation-failure:{run_id}"))),
        "threadId": thread_id, "runId": run_id, "nodeId": state.root["id"],
        "providerThreadId": state.provider_thread["id"], "providerTurnId": null, "nativeItemRef": null,
        "parentItemId": null, "ordinal": ordinal, "status": "failed", "title": "Workspace preparation failed",
        "startedAt": timestamp, "completedAt": timestamp, "updatedAt": timestamp, "type": "error", "failure": failure,
    });
    let mut run = state.run.clone();
    run["status"] = json!("failed");
    run["completedAt"] = json!(timestamp);
    let events = vec![
        ("run-attempt.updated", checked::<t3_contracts::RunAttempt>(attempt)?),
        ("node.updated", checked::<t3_contracts::ExecutionNode>(root)?),
        ("turn-item.updated", checked::<t3_contracts::TurnItem>(preparation)?),
        ("turn-item.updated", checked::<t3_contracts::TurnItem>(error)?),
        ("run.updated", checked::<t3_contracts::Run>(run)?),
    ];
    Ok(Decision::Accepted {
        events: events
            .into_iter()
            .map(|(kind, payload)| event(thread_id, command_id, kind, payload, now))
            .collect(),
        effects: vec![],
    })
}

/// Returns a run whose preparation failed to `preparing`. The failure item turns cancelled so
/// clients stop offering the retry, and the preparation is scheduled again with the commit.
pub(crate) fn plan_retry(command: &Value, projection: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
    let state = state(command, projection);
    let failure_item = projection["turnItems"].as_array().unwrap().iter().find(|item| {
        item["runId"] == command["runId"]
            && item["type"] == "error"
            && item["status"] == "failed"
            && item["failure"]["code"] == FAILURE_CODE
    });
    let (Some(state), Some(failure_item)) = (state, failure_item) else {
        return Err(reject(format!(
            "Run {} has no failed workspace preparation to retry.",
            command["runId"].as_str().unwrap_or("")
        )));
    };
    if state.run.get("workspacePreparation").is_none_or(Value::is_null)
        || !projection["thread"]["archivedAt"].is_null()
        || !projection["thread"]["deletedAt"].is_null()
    {
        return Err(reject(format!(
            "Run {} has no failed workspace preparation to retry.",
            command["runId"].as_str().unwrap_or("")
        )));
    }
    if projection["runs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|run| run["id"] != state.run["id"] && crate::message_queue::blocking(run))
    {
        return Err(reject("Another run is active on this thread.".into()));
    }
    let (thread_id, command_id) = (command["threadId"].as_str().unwrap(), command["commandId"].as_str().unwrap());
    let timestamp = at(now);
    let mut cancelled = failure_item.clone();
    cancelled["status"] = json!("cancelled");
    cancelled["updatedAt"] = json!(timestamp);
    let mut preparation = state.preparation.clone();
    if let Some(object) = preparation.as_object_mut() {
        for field in ["output", "exitCode", "outputIndicatesFailure"] {
            object.remove(field);
        }
    }
    preparation["status"] = json!("running");
    preparation["title"] = json!(PREPARING);
    preparation["completedAt"] = Value::Null;
    preparation["updatedAt"] = json!(timestamp);
    let mut attempt = state.attempt.clone();
    attempt["status"] = json!("pending");
    attempt["completedAt"] = Value::Null;
    let mut root = state.root.clone();
    root["status"] = json!("pending");
    root["completedAt"] = Value::Null;
    let mut run = state.run.clone();
    run["status"] = json!("preparing");
    run["completedAt"] = Value::Null;
    let run_id = run["id"].clone();
    let events = vec![
        ("turn-item.updated", checked::<t3_contracts::TurnItem>(cancelled)?),
        ("turn-item.updated", checked::<t3_contracts::TurnItem>(preparation)?),
        ("run-attempt.updated", checked::<t3_contracts::RunAttempt>(attempt)?),
        ("node.updated", checked::<t3_contracts::ExecutionNode>(root)?),
        ("run.updated", checked::<t3_contracts::Run>(run.clone())?),
    ];
    Ok(Decision::Accepted {
        events: events
            .into_iter()
            .map(|(kind, payload)| event(thread_id, command_id, kind, payload, now))
            .collect(),
        effects: vec![NewEffect {
            id: format!("effect:{command_id}:workspace.prepare:{}", run_id.as_str().unwrap()),
            command_id: command_id.into(),
            thread_id: thread_id.into(),
            request: json!({"type":"workspace.prepare","baseCommandId":command_id,"runId":run_id,"workspaceStrategy":run["workspacePreparation"],"retry":true}),
            available_at: timestamp,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Value> {
        include_str!("../tests/fixtures/workspace-preparation.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Admission and events of the three commands, replayed from the original's source.
    #[test]
    fn preparation_commands_match_the_original() {
        let now: DateTime<Utc> = "2030-01-01T00:00:00.000Z".parse().unwrap();
        let mut compared = 0;
        for row in fixtures().iter().filter(|row| row["operation"] == "prepared-run") {
            let (command, projection, name) = (&row["command"], &row["projection"], row["name"].as_str().unwrap());
            let planned = match command["type"].as_str().unwrap() {
                "prepared-run.progress" => plan_progress(command, projection, now),
                "prepared-run.fail" => plan_fail(command, projection, now),
                _ => plan_retry(command, projection, now),
            };
            match (&row["output"]["error"], planned) {
                (Value::String(expected), Err(StoreError::InvalidCommand(actual))) => assert_eq!(&actual, expected, "{name}"),
                (Value::Null, Ok(Decision::Accepted { events, effects })) => {
                    let actual: Vec<Value> = events.iter().map(|event| json!({"type":event.event_type,"payload":event.payload})).collect();
                    assert_eq!(Value::Array(actual), row["output"]["events"], "{name}");
                    // Only a retry schedules work, and it is the preparation of the same run.
                    if command["type"] == "prepared-run.retry" {
                        assert_eq!(effects.len(), 1, "{name}");
                        assert_eq!(effects[0].request["type"], "workspace.prepare", "{name}");
                        assert_eq!(effects[0].request["runId"], "run:1", "{name}");
                        assert_eq!(effects[0].request["retry"], true, "{name}");
                        assert_eq!(effects[0].request["workspaceStrategy"], projection["runs"][0]["workspacePreparation"], "{name}");
                    } else {
                        assert!(effects.is_empty(), "{name}");
                    }
                }
                (expected, planned) => panic!("{name}: expected {expected}, got {:?}", planned.map(|_| ())),
            }
            compared += 1;
        }
        assert_eq!(compared, 23);
    }
}
