//! Ten WebSocket RPC methods over a real socket: a payload the original's decoding refuses is
//! refused as a schema failure before the handler runs, and nothing is persisted. The payloads are
//! the ones `tools/generate_rpc_decode_cases.mjs` found the original refusing, spread over each
//! method; the same cases are checked without the socket in `rpc_decode_tests.rs`.
mod rpc_support;
use rpc_support::*;
use serde_json::Value;
use std::collections::BTreeMap;

/// The generator writes a long run of one string as `{"$repeat": [unit, count]}`, and a long array
/// of one element as `{"$items": [element, count]}`.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$repeat") => Value::String(object["$repeat"][0].as_str().unwrap().repeat(object["$repeat"][1].as_u64().unwrap() as usize)),
        Value::Object(object) if object.len() == 1 && object.contains_key("$items") => Value::Array(vec![expand(&object["$items"][0]); object["$items"][1].as_u64().unwrap() as usize]),
        Value::Object(object) => Value::Object(object.iter().map(|(key, found)| (key.clone(), expand(found))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        other => other.clone(),
    }
}

#[tokio::test]
async fn payloads_the_original_refuses_never_reach_the_handlers() {
    use t3_contracts::AuthEnvironmentScope::*;
    let h = harness().await;
    // Every scope the ten methods ask for, so only the payload can stop a call.
    let auth = t3_server::auth::AuthService::new(h.store.clone(), [7; 32], "t3_session_rpc".into(), "loopback-browser".into()).unwrap();
    let scopes = vec![OrchestrationRead, OrchestrationOperate, SettingsWrite, ProvidersManage, EnvironmentMaintain, SourceControlWrite, FilesystemRead, FilesystemWrite];
    let token = auth.issue_session("admission", "bearer-access-token", scopes, serde_json::json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let mut socket = h.connect(&token).await;
    let mut refused: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for line in include_str!("fixtures/rpc-decode-cases.jsonl").lines().filter(|line| !line.is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["accepted"] == false {
            refused.entry(row["method"].as_str().unwrap().to_owned()).or_default().push(expand(&row["input"]));
        }
    }
    assert_eq!(refused.len(), 10);
    let before = h.store.latest_sequence().unwrap();
    let mut id = 0;
    for (method, payloads) in &refused {
        let stride = (payloads.len() / 25).max(1);
        for payload in payloads.iter().step_by(stride) {
            id += 1;
            let exit = rpc(&mut socket, id, method, payload.clone()).await;
            let text = defect(&exit).to_string();
            // The library's wording, not a serde error from the handler's own decode.
            assert!(text.contains("Expected") || text.contains("Missing key"), "{method} {payload}: {exit}");
        }
    }
    assert!(id > 200, "{id} calls");
    assert_eq!(h.store.latest_sequence().unwrap(), before);
    h.finish().await;
}
