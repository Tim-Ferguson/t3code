//! A delegate call is dropped from the conversation once the child it started has a card of its own
//! (`withoutSubagentDelegationRows`), and stays when it failed or names another child.
use serde_json::{Value, json};
use t3_client::timeline_rows::entries;

fn row(item: Value) -> Value {
    json!({"sourceThreadId":"thread","sourceItemId":item["id"],"visibility":"local","item":item})
}
fn delegate(output: Value, status: &str) -> Value {
    row(json!({"id":"call","type":"dynamic_tool","status":status,"runId":"run-1","toolName":"mcp__t3-code__delegate_task","output":output,"updatedAt":"2026-01-01T00:00:00Z"}))
}
fn child(origin: &str, run: &str) -> Value {
    row(json!({"id":"child","type":"subagent","status":"running","runId":run,"origin":origin,"subagentId":"task-1","updatedAt":"2026-01-01T00:00:00Z"}))
}
fn kept(rows: Vec<Value>) -> usize {
    entries(&rows, &[], &[]).len()
}

#[test]
fn the_call_goes_when_the_child_it_started_has_a_card() {
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1"}), "completed"), child("app_owned", "run-1")]), 1);
    // The envelopes the adapters keep read the same.
    assert_eq!(kept(vec![delegate(json!("{\"taskId\":\"task-1\"}"), "running"), child("app_owned", "run-1")]), 1);
    assert_eq!(kept(vec![delegate(json!({"structuredContent":{"taskId":"task-1"}}), "completed"), child("app_owned", "run-1")]), 1);
}

#[test]
fn the_call_stays_when_nothing_ties_it_to_a_child_or_it_failed() {
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1"}), "completed")]), 1, "no child yet");
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-2"}), "completed"), child("app_owned", "run-1")]), 2, "another child");
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1"}), "completed"), child("provider_native", "run-1")]), 2, "a provider's own subagent");
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1"}), "completed"), child("app_owned", "run-2")]), 2, "another run's child");
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1","isError":true}), "completed"), child("app_owned", "run-1")]), 2, "a failed call");
    assert_eq!(kept(vec![delegate(json!({"taskId":"task-1"}), "failed"), child("app_owned", "run-1")]), 2, "a call that did not complete");
}
