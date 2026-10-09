//! What a thread's queue can do right now: the original's `deriveThreadQueueWorkflowState` and the
//! provider-session lookup it reads (`threadWorkflows.ts`), over the same projection. Replayed
//! against the original by `tests/queue_workflow_parity.rs`.
use serde_json::Value;

fn list<'a>(projection: &'a Value, key: &str) -> &'a [Value] {
    projection[key].as_array().map(Vec::as_slice).unwrap_or_default()
}

/// `resolveActiveThreadRun`: the newest run that is preparing, starting, running or waiting.
pub fn active_run(projection: &Value) -> Option<&Value> {
    list(projection, "runs").iter().rev().find(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")))
}

/// `resolveThreadProviderSession`: the session the thread's provider conversation runs in.
pub fn provider_session(projection: &Value) -> Option<&Value> {
    let thread = &projection["thread"];
    let provider_thread_id = active_run(projection).map(|run| &run["providerThreadId"]).filter(|id| !id.is_null()).unwrap_or(&thread["activeProviderThreadId"]);
    let threads = list(projection, "providerThreads");
    let active = (!provider_thread_id.is_null()).then(|| threads.iter().find(|candidate| candidate["id"] == *provider_thread_id)).flatten();
    let attached = active.or_else(|| threads.iter().find(|candidate| candidate["appThreadId"] == thread["id"] && !candidate["providerSessionId"].is_null()));
    let session_id = attached.map(|attached| &attached["providerSessionId"]).filter(|id| !id.is_null());
    let sessions = list(projection, "providerSessions");
    match session_id {
        Some(id) => sessions.iter().find(|session| session["id"] == *id),
        None => sessions.iter().rev().find(|session| !matches!(session["status"].as_str(), Some("stopped" | "error"))),
    }
}

/// Whether a queued message may be sent into the running turn instead of waiting
/// (`canPromoteToSteer`): the turn is running on the provider and the provider can be steered.
pub fn can_promote_to_steer(projection: &Value) -> bool {
    let Some(run) = active_run(projection) else { return false };
    let steerable_turn = run["status"] == "running" && !run["activeAttemptId"].is_null() && list(projection, "providerTurns").iter().any(|turn| turn["runAttemptId"] == run["activeAttemptId"] && turn["status"] == "running");
    let turns = provider_session(projection).map(|session| &session["capabilities"]["turns"]);
    steerable_turn && turns.is_some_and(|turns| turns["supportsActiveSteering"] == true || turns["supportsSteeringByInterruptRestart"] == true)
}

/// `isHeld`: a queued message waits for the reader to resume the queue.
pub fn is_held(projection: &Value) -> bool {
    list(projection, "runs").iter().any(|run| run["status"] == "queued" && run["queueHeld"] == true)
}

/// `queued-message.promote-to-steer`: sends `queued` into `target`, the running turn.
pub fn promote_command(thread_id: &str, queued_run_id: &str, target_run_id: &str) -> Value {
    serde_json::json!({"type":"queued-message.promote-to-steer","threadId":thread_id,"queuedRunId":queued_run_id,"targetRunId":target_run_id})
}
