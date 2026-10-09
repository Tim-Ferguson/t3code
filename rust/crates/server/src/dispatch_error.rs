//! The error a refused `orchestration.dispatchCommand` or `orchestration.launchThread` reaches
//! the client as (`ws.ts`): an `OrchestrationV2DispatchCommandError` whose message is the
//! reason the command was refused, or an `OrchestrationV2ThreadLaunchError`. One place builds
//! both so no refusal reaches the wire as a `NativeServiceError` with the store's own
//! `Invalid command: ` prefix.
//!
//! Divergence: the original's `ThreadLaunchError` carries the constant message "Failed to
//! launch thread" and keeps the reason in its cause chain. Here the reason is the message and
//! the chain is omitted, so a client that reads only `message` shows the reason.
use crate::persistence::{Receipt, StoreError};
use serde_json::{Value, json};

/// The reason a store error gives, without the store's own prefix.
fn reason(error: &StoreError) -> String {
    match error {
        StoreError::InvalidCommand(reason) => reason.clone(),
        other => other.to_string(),
    }
}

/// `OrchestratorDispatchError`'s message: the generic text `ws.ts` filters out when it looks
/// for the user-facing reason.
fn generic(command_type: &Value, command_id: &Value) -> String {
    format!(
        "Failed to dispatch orchestration command {} ({}).",
        command_type.as_str().unwrap_or(""),
        command_id.as_str().unwrap_or("")
    )
}

fn envelope(command_id: &Value, command_type: &Value, message: &str, cause: Value) -> Value {
    if message.trim().is_empty() {
        return json!({"_tag":"OrchestrationV2DispatchCommandError","commandId":command_id,"commandType":command_type,"message":"Failed to dispatch orchestration V2 command","cause":cause});
    }
    json!({"_tag":"OrchestrationV2DispatchCommandError","commandId":command_id,"commandType":command_type,"message":message.trim(),"detail":message.trim(),"cause":cause})
}

/// A command refused with a reason: the planner's own `OrchestratorDispatchError`.
fn refused(command_id: &Value, command_type: &Value, reason: &str) -> Value {
    let mut cause = json!({"name":"OrchestratorDispatchError","message":generic(command_type, command_id)});
    if !reason.is_empty() {
        cause["cause"] = json!(reason);
    }
    envelope(command_id, command_type, reason, cause)
}

/// A dispatched command the store refused or could not run.
pub(crate) fn dispatch(command: &Value, error: &StoreError) -> Value {
    refused(&command["commandId"], &command["type"], &reason(error))
}

/// A rejected receipt. The first rejection fails with the planner's error; a repeat of the same
/// command id fails as `OrchestratorCommandPreviouslyRejectedError`, whose detail is the
/// message the receipt kept.
pub(crate) fn rejected(receipt: &Receipt, first_time: bool) -> Value {
    let (command_id, command_type) = (json!(receipt.command_id), json!(receipt.command_type));
    let error = receipt.error.clone().unwrap_or(Value::Null);
    let planner = error["_tag"] == "OrchestratorDispatchError";
    let text = if planner {
        match &error["cause"] {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    } else {
        ["detail", "message", "cause"].iter().find_map(|key| error[*key].as_str()).map(str::to_owned).unwrap_or_else(|| error.to_string())
    };
    if first_time {
        return if planner {
            refused(&command_id, &command_type, &text)
        } else {
            envelope(&command_id, &command_type, &text, json!({"name":error["_tag"].as_str().unwrap_or("Error"),"message":text}))
        };
    }
    let detail = if planner { generic(&command_type, &command_id) } else { text };
    let message = format!("Command {} was previously rejected: {detail}", receipt.command_id);
    envelope(&command_id, &command_type, &message, json!({"name":"OrchestratorCommandPreviouslyRejectedError","message":message}))
}

/// A launch the store refused or could not run.
pub(crate) fn launch(input: &Value, error: &StoreError) -> Value {
    json!({"_tag":"OrchestrationV2ThreadLaunchError","commandId":input["commandId"],"projectId":input["projectId"],"message":reason(error)})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(name: &str) -> impl Iterator<Item = Value> + '_ {
        include_str!("../tests/fixtures/dispatch-errors.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(move |row| row["name"] == name)
    }

    /// Every refusal of the original, as it reaches the wire.
    #[test]
    fn refused_commands_match_the_envelopes_of_the_original() {
        for row in rows("refused-at-admission").chain(rows("no-usable-message")) {
            let reason = row["refusal"]["reason"].as_str().unwrap();
            let error = if reason.is_empty() { StoreError::InvalidCommand(String::new()) } else { StoreError::InvalidCommand(reason.into()) };
            assert_eq!(dispatch(&row["command"], &error), row["envelope"], "{row}");
            // The same refusal recorded on a receipt reads the same the first time.
            let receipt = Receipt {
                command_id: row["command"]["commandId"].as_str().unwrap().into(),
                aggregate_kind: "thread".into(),
                aggregate_id: "thread".into(),
                command_type: row["command"]["type"].as_str().unwrap().into(),
                accepted_at: String::new(),
                result_sequence: 0,
                status: "rejected".into(),
                error: Some(json!({"_tag":"OrchestratorDispatchError","commandId":row["command"]["commandId"],"commandType":row["command"]["type"],"cause":reason})),
            };
            assert_eq!(rejected(&receipt, true), row["envelope"], "{row}");
        }
    }

    #[test]
    fn a_repeated_rejected_command_reads_as_previously_rejected() {
        for row in rows("previously-rejected") {
            let receipt = Receipt {
                command_id: row["command"]["commandId"].as_str().unwrap().into(),
                aggregate_kind: "thread".into(),
                aggregate_id: "thread".into(),
                command_type: row["command"]["type"].as_str().unwrap().into(),
                accepted_at: String::new(),
                result_sequence: 0,
                status: "rejected".into(),
                error: Some(json!({"_tag":"OrchestratorDispatchError","cause":"Thread is deleted or archived."})),
            };
            assert_eq!(rejected(&receipt, false), row["envelope"], "{row}");
        }
    }

    /// The original keeps a launch's reason at the end of its cause chain; here it is the message.
    #[test]
    fn a_refused_launch_carries_the_reason_the_original_keeps_in_its_cause() {
        for row in rows("launch-refused") {
            let reason = row["reason"].as_str().unwrap();
            let mut deepest = &row["envelope"]["cause"];
            while deepest.is_object() {
                deepest = &deepest["cause"];
            }
            assert_eq!(deepest, reason);
            let wire = launch(&row["input"], &StoreError::InvalidCommand(reason.into()));
            assert_eq!(wire["_tag"], row["envelope"]["_tag"]);
            assert_eq!(wire["commandId"], row["envelope"]["commandId"]);
            assert_eq!(wire["projectId"], row["envelope"]["projectId"]);
            assert_eq!(wire["message"], reason);
            assert!(!wire["message"].as_str().unwrap().contains("Invalid command"));
        }
    }
}
