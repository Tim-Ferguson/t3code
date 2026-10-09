//! The `scheduledTasks.*` socket methods: decode, one `ScheduledTasks` call, and the original's error
//! envelopes. A payload that does not decode is a defect, as in the original; every service failure is
//! a `ScheduledTaskError`.
use crate::{
    scheduled_tasks::{ScheduledTaskError, ScheduledTasks, TaskListSubscription},
    transport::ApiState,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use t3_contracts::{ScheduledTaskListInput, ScheduledTaskListResult};

/// The unary methods. `scheduledTasks.subscribe` is a stream (see `open_stream`).
pub const METHODS: [&str; 8] = [
    "scheduledTasks.list",
    "scheduledTasks.upsert",
    "scheduledTasks.setEnabled",
    "scheduledTasks.delete",
    "scheduledTasks.runNow",
    "scheduledTasks.rotateWebhookToken",
    "scheduledTasks.listWebhookDeliveries",
    "scheduledTasks.getWebhookDelivery",
];
pub const SUBSCRIBE: &str = "scheduledTasks.subscribe";

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}
fn encode<T: Serialize>(result: Result<T, ScheduledTaskError>) -> Result<Value, Value> {
    let value = result.map_err(|error| error.wire())?;
    serde_json::to_value(value).map_err(|error| ScheduledTaskError::new("Could not encode the scheduled task result.").wire_with(&error))
}
impl ScheduledTaskError {
    fn wire_with(self, cause: &dyn std::fmt::Display) -> Value {
        let mut wire = self.wire();
        wire["cause"] = json!({"name":"Error","message":cause.to_string()});
        wire
    }
}

/// A webhook URL starts agent runs, so only sessions that may operate see it; read-only sessions still
/// see the task itself.
fn visible(result: ScheduledTaskListResult, can_operate: bool) -> ScheduledTaskListResult {
    if can_operate {
        return result;
    }
    ScheduledTaskListResult {
        tasks: t3_contracts::ForwardCompatibleArray(
            result.tasks.0.into_iter().map(|mut task| {
                task.webhook = None;
                task
            }).collect(),
        ),
    }
}

fn service(state: &ApiState) -> Result<ScheduledTasks, Value> {
    let service = ScheduledTasks::of(&state.store).map_err(|error| error.wire())?;
    service.ensure_launcher(state.execution.as_ref());
    Ok(service)
}

/// `can_operate` is whether the session holds `orchestration:operate`.
pub async fn command(state: &ApiState, tag: &str, payload: Value, can_operate: bool) -> Result<Value, Value> {
    let service = service(state)?;
    match tag {
        "scheduledTasks.list" => {
            decode::<ScheduledTaskListInput>(payload)?;
            encode(service.list().await.map(|result| visible(result, can_operate)))
        }
        "scheduledTasks.upsert" => encode(service.upsert(decode(payload)?).await),
        "scheduledTasks.setEnabled" => encode(service.set_enabled(decode(payload)?).await),
        "scheduledTasks.delete" => encode(service.delete(decode(payload)?).await),
        "scheduledTasks.runNow" => encode(service.run_now(decode(payload)?).await),
        "scheduledTasks.rotateWebhookToken" => encode(service.rotate_webhook_token(decode(payload)?).await),
        "scheduledTasks.listWebhookDeliveries" => encode(service.list_webhook_deliveries(decode(payload)?).await),
        "scheduledTasks.getWebhookDelivery" => encode(service.get_webhook_delivery(decode(payload)?).await),
        other => Err(json!({"_tag":"NativeMethodUnsupportedError","method":other})),
    }
}

/// `scheduledTasks.subscribe`: the full list once, then again after every change, until the client leaves.
pub struct ListStream {
    subscription: TaskListSubscription,
    can_operate: bool,
}
pub fn open_stream(state: &ApiState, payload: Value, can_operate: bool) -> Result<ListStream, Value> {
    decode::<ScheduledTaskListInput>(payload)?;
    Ok(ListStream { subscription: service(state)?.subscribe(), can_operate })
}
impl ListStream {
    /// The next list event; an error ends the stream with that error.
    pub async fn next(&mut self) -> Option<Result<Value, Value>> {
        let next = self.subscription.next().await?;
        Some(encode(next.map(|result| visible(result, self.can_operate))))
    }
}
