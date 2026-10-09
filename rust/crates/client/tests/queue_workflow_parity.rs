//! The queue's workflow state replayed against the original
//! (`rust/tools/generate_client_queue_workflow_fixtures.mjs`).
use serde_json::Value;
use t3_client::queue_workflow as workflow;

#[test]
fn the_active_run_the_steer_and_the_hold_are_the_originals() {
    let mut count = 0;
    for line in include_str!("fixtures/queue-workflow.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let projection = &row["projection"];
        assert_eq!(workflow::active_run(projection).and_then(|run| run["id"].as_str()), row["activeRun"].as_str(), "{row}");
        assert_eq!(workflow::can_promote_to_steer(projection), row["canPromoteToSteer"].as_bool().unwrap(), "{row}");
        assert_eq!(workflow::is_held(projection), row["isHeld"].as_bool().unwrap(), "{row}");
        count += 1;
    }
    assert!(count > 1000);
}

#[test]
fn the_promote_command_names_the_queued_run_and_the_one_it_joins() {
    assert_eq!(workflow::promote_command("t", "q", "r"), serde_json::json!({"type":"queued-message.promote-to-steer","threadId":"t","queuedRunId":"q","targetRunId":"r"}));
}
