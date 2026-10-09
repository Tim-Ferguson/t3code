//! `subscribeWorktreeSetup` and `worktreeSetup.cancel`: what a client reads of a thread's
//! worktree setup while its first turn is prepared, and the way to stop it.
use crate::transport::ApiState;
use serde_json::{Value, json};
use t3_contracts::{WorktreeSetupCancelInput, WorktreeSetupSnapshot, WorktreeSetupSubscribeInput};
use tokio::sync::watch;

fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload)
        .map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}

/// A server without an execution service tracks nothing: its streams hold null.
fn tracker(state: &ApiState) -> crate::worktree_setup_tracker::WorktreeSetupTracker {
    state
        .execution
        .as_ref()
        .map(|execution| execution.workspace().tracker())
        .unwrap_or_default()
}

pub fn open_stream(
    state: &ApiState,
    payload: &Value,
) -> Result<watch::Receiver<Option<WorktreeSetupSnapshot>>, Value> {
    let input: WorktreeSetupSubscribeInput = decode(payload.clone())?;
    Ok(tracker(state).subscribe(input.thread_id.as_str()))
}

/// Stops the thread's running setup and waits for it to unwind. `cancelled` is false when
/// nothing is running or the setup is past cancellation.
pub async fn cancel(state: &ApiState, payload: Value) -> Result<Value, Value> {
    let input: WorktreeSetupCancelInput = decode(payload)?;
    let cancelled = tracker(state).cancel(input.thread_id.as_str()).await;
    Ok(json!({"cancelled":cancelled}))
}
