#![cfg(unix)]
//! The original's `delegated_task_status` recording, driven the way `OrchestratorMcpToolkit.integration.test.ts`
//! drives it: a parent holds a turn open, delegates a task over MCP, reads its result, sends the child a
//! follow-up that stays running and a queued one, interrupts the first, and reads the task's status at
//! each step. The parent and the child run on two instances of the fake Codex, each serving its half of the
//! recording (see `tools/generate_codex_delegated_replay_fixture.mjs`). The fake Codex (`fixtures/codex-provider.py`) serves the recording frame by frame and ends
//! the run under test on any frame the Rust client sends that the recording does not expect.
mod mcp_support;
use mcp_support::*;
use serde_json::{Value, json};

const PARENT_PROMPT: &str = "Keep this parent turn active while orchestration tools are tested.";
const DELEGATED_PROMPT: &str = "Inspect the delegated API boundary and return the result.";
const DELEGATED_RESULT: &str = "Delegated API boundary inspected.";
const RUNNING_PROMPT: &str = "Remain active until the parent cancels this delegated task.";
const QUEUED_PROMPT: &str = "Complete the queued follow-up and return the final result.";
const QUEUED_RESULT: &str = "Queued delegated follow-up completed.";

fn run_of<'a>(view: &'a Value, id: &Value) -> &'a Value {
    view["runs"].as_array().unwrap().iter().find(|run| run["id"] == *id).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_tasks_status_follows_its_child_through_a_running_and_a_queued_follow_up() {
    let scratch = tempfile::tempdir().unwrap();
    let (parent_transcript, child_transcript) = (scratch.path().join("parent.ndjson"), scratch.path().join("child.ndjson"));
    std::fs::write(&parent_transcript, include_str!("fixtures/codex-delegated-task-status.parent.ndjson")).unwrap();
    std::fs::write(&child_transcript, include_str!("fixtures/codex-delegated-task-status.child.ndjson")).unwrap();
    let h = Harness::with_instances(vec![("CODEX_FIXTURE_TRANSCRIPT", parent_transcript.to_string_lossy().into_owned())], vec![("CODEX_FIXTURE_TRANSCRIPT", child_transcript.to_string_lossy().into_owned())]).await;
    h.start_thread("parent", "fixture-model", "full-access", PARENT_PROMPT);
    h.until("parent", |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
    let caller = h.caller("parent").await;

    let started = h.ok(&caller, "delegate_task", json!({"task":DELEGATED_PROMPT,"target":{"providerInstanceId":"codexChild","model":"fixture-model"},"mode":"async","clientRequestId":"delegate-codex-replay-1"})).await;
    assert!(started["childRunId"].is_string());
    let child = started["childThreadId"].as_str().unwrap().to_owned();
    // The child's result reaches the parent as a context transfer from its original run.
    h.until("parent", |view| view["contextTransfers"].as_array().unwrap().iter().any(|transfer| transfer["type"] == "subagent_result" && transfer["sourceThreadId"] == child.as_str() && transfer["sourcePoint"]["runId"] == started["childRunId"]).then_some(())).await;
    let delegated = h.ok(&caller, "task_status", json!({"taskId":started["taskId"]})).await;
    assert_eq!(h.mismatch(), None, "the recording refused a frame the client sent");
    assert_eq!((delegated["status"].as_str(), delegated["summary"].as_str(), delegated["providerInstanceId"].as_str(), delegated["hasPendingChildRuns"].as_bool()), (Some("completed"), Some(DELEGATED_RESULT), Some("codexChild"), Some(false)));
    assert_eq!((delegated["latestTerminalRunId"].as_str(), delegated["latestTerminalStatus"].as_str(), delegated["latestTerminalSummary"].as_str()), (started["childRunId"].as_str(), Some("completed"), Some(DELEGATED_RESULT)));
    assert!(delegated["resultContextTransferId"].is_string());
    assert_eq!(delegated["latestTerminalResultContextTransferId"], delegated["resultContextTransferId"]);

    // A delegated child is T3's own, so it keeps taking follow-ups.
    let view = h.view(&child);
    assert_eq!(view["thread"]["lineage"]["relationshipToParent"], "subagent");
    assert_eq!(view["thread"]["creationSource"], "mcp");
    let running = h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":RUNNING_PROMPT,"clientRequestId":"delegated-child-replay-running-1"})).await;
    h.until(&child, |view| (view["runs"].as_array().unwrap().len() == 2 && view["runs"][1]["status"] == "running").then_some(())).await;
    let queued = h.ok(&caller, "t3_thread_send", json!({"threadId":child,"message":QUEUED_PROMPT,"mode":"queue","clientRequestId":"delegated-child-replay-queued-1"})).await;
    assert_eq!((queued["status"].as_str(), queued["delivery"].as_str()), (Some("queued"), Some("queued")));
    let pending = h.view(&child);
    assert_eq!((run_of(&pending, &started["childRunId"])["status"].as_str(), run_of(&pending, &running["runId"])["status"].as_str(), run_of(&pending, &queued["runId"])["status"].as_str()), (Some("completed"), Some("running"), Some("queued")));
    assert!(h.view("parent")["runs"].as_array().unwrap().iter().any(|run| run["status"] == "running"));
    let during = h.ok(&caller, "task_status", json!({"taskId":started["taskId"]})).await;
    assert_eq!((during["status"].as_str(), during["summary"].as_str(), during["hasPendingChildRuns"].as_bool(), during["latestTerminalRunId"].as_str()), (Some("completed"), Some(DELEGATED_RESULT), Some(true), started["childRunId"].as_str()));
    assert_eq!(during["resultContextTransferId"], delegated["resultContextTransferId"]);

    // Interrupting the running follow-up lets the queued one run and complete.
    let interrupt = h.ok(&caller, "t3_thread_interrupt", json!({"threadId":child,"runId":running["runId"],"reason":"Allow the queued replay follow-up to run.","clientRequestId":"interrupt-delegated-child-replay-1"})).await;
    assert_eq!((interrupt["runId"].as_str(), interrupt["status"].as_str()), (running["runId"].as_str(), Some("interrupt_requested")));
    h.until(&child, |view| (run_of(view, &queued["runId"])["status"] == "completed").then_some(())).await;
    let finished = h.view(&child);
    assert_eq!(run_of(&finished, &running["runId"])["status"], "interrupted");
    let last = h.ok(&caller, "task_status", json!({"taskId":started["taskId"]})).await;
    assert_eq!((last["status"].as_str(), last["summary"].as_str(), last["hasPendingChildRuns"].as_bool(), last["latestTerminalRunId"].as_str(), last["latestTerminalSummary"].as_str()), (Some("completed"), Some(DELEGATED_RESULT), Some(false), queued["runId"].as_str(), Some(QUEUED_RESULT)));
    assert_eq!(last["resultContextTransferId"], delegated["resultContextTransferId"]);
    assert_eq!(last["latestTerminalResultContextTransferId"], Value::Null);
    h.finish().await;
}
