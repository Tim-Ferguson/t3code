//! The scheduled-task wire contracts against the original: every payload the original's codecs were
//! given (valid, padded, nullable, malformed) must be accepted or refused here the same way and, when
//! accepted, mean the same value; the error envelope must encode the same.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use t3_contracts::*;
use t3_server::scheduled_tasks::ScheduledTaskError;

fn rows() -> Vec<Value> {
    include_str!("fixtures/scheduled-task-contracts.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn check<T: DeserializeOwned + Serialize>(row: &Value) {
    let payload = row["payload"].clone();
    let decoded = serde_json::from_value::<T>(payload);
    match row.get("ok") {
        None => assert!(decoded.is_err(), "the original refuses {row}, this accepted it as {:?}", decoded.map(|value| serde_json::to_value(value).unwrap())),
        Some(expected) => {
            let decoded = decoded.unwrap_or_else(|error| panic!("the original accepts {row}: {error}"));
            assert_eq!(&serde_json::to_value(decoded).unwrap(), expected, "{}", row["payload"]);
        }
    }
}

#[test]
fn every_payload_decodes_as_the_original_decodes_it() {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for row in rows().iter().filter(|row| row["type"] == "decode") {
        let kind = row["kind"].as_str().unwrap();
        *counts.entry(kind.to_owned()).or_default() += 1;
        match kind {
            "upsert" => check::<ScheduledTaskUpsertInput>(row),
            "setEnabled" => check::<ScheduledTaskSetEnabledInput>(row),
            "delete" => check::<ScheduledTaskDeleteInput>(row),
            "runNow" => check::<ScheduledTaskRunNowInput>(row),
            "rotate" => check::<ScheduledTaskRotateWebhookTokenInput>(row),
            "listDeliveries" => check::<ScheduledTaskListWebhookDeliveriesInput>(row),
            "getDelivery" => check::<ScheduledTaskGetWebhookDeliveryInput>(row),
            "task" => check::<ScheduledTask>(row),
            "schedule" => check::<ScheduledTaskSchedule>(row),
            "listResult" => check::<ScheduledTaskListResult>(row),
            // `Schema.Struct({})` takes anything but null, and what it returns is an empty struct.
            "list" => assert_eq!(serde_json::from_value::<ScheduledTaskListInput>(row["payload"].clone()).is_ok(), row.get("ok").is_some(), "{row}"),
            other => panic!("unknown kind {other}"),
        }
    }
    assert!(counts["upsert"] > 120 && counts["task"] > 25 && counts.len() == 11, "{counts:?}");
}

#[test]
fn the_error_envelope_encodes_as_the_original_encodes_it() {
    let wires: Vec<Value> = rows().iter().filter(|row| row["type"] == "error").map(|row| row["wire"].clone()).collect();
    assert_eq!(wires.len(), 7);
    let error = |message: &str, task: Option<&str>, cause: Option<Value>| ScheduledTaskError { message: message.into(), task_id: task.map(Into::into), cause };
    assert_eq!(error("Schedule task not found.", Some("t1"), None).wire(), wires[0]);
    assert_eq!(error("Schedule task not found.", None, None).wire(), wires[1]);
    let nested = json!({"name":"ScheduledTaskError","message":"Schedule task is already running."});
    assert_eq!(error("Could not run schedule task.", Some("t1"), Some(nested)).wire(), wires[2]);
    let plain = json!({"name":"Error","message":"SQLITE_BUSY: database is locked"});
    assert_eq!(error("Could not save schedule task.", Some("t1"), Some(plain)).wire(), wires[3]);
}
