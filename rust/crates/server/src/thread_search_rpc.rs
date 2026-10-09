//! The `orchestration.searchThreads` socket method: decode, one `thread_search::search` call on a
//! blocking worker (the store is one synchronous connection), and the original's error envelope.
use crate::{persistence::Store, thread_search};
use serde_json::{Value, json};
use t3_contracts::{
    OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult,
};

pub const METHODS: [&str; 1] = ["orchestration.searchThreads"];

pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    if tag != METHODS[0] {
        return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag}));
    }
    let input: OrchestrationSearchThreadsInput = serde_json::from_value(payload)
        .map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))?;
    let (store, query) = (store.clone(), input.query.as_str().to_owned());
    let limit = input.limit.map(|limit| limit.0 as u32);
    let matches = tokio::task::spawn_blocking(move || thread_search::search(&store, &query, limit))
        .await
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?
        .map_err(|error| error.wire())?;
    let result = thread_search::result_json(&matches);
    // The result must decode as the original's success type before it leaves.
    serde_json::from_value::<OrchestrationSearchThreadsResult>(result)
        .and_then(|typed| serde_json::to_value(typed))
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
}
