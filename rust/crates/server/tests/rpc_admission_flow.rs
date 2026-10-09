//! Every WebSocket RPC method over a real socket: a payload the original's decoding refuses is refused before the handler
//! runs, with the defect text the original's RPC server sends (the default formatter's text of the schema issue), and
//! nothing is persisted. The payloads are the ones `tools/generate_rpc_decode_cases.mjs` found the original refusing,
//! spread over each method; the same cases are checked without the socket in `rpc_decode_tests.rs`. Only refused payloads
//! are sent (a refused one reaches no handler), so no accepted request here has a side effect outside the temp directory.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The generator writes a long run of one string as `{"$repeat": [unit, count]}` (with `"$pad": [before, after]` for a
/// blank on each side), and a long array of one element as `{"$items": [element, count]}`.
fn expand(value: &Value) -> Value {
    match value {
        Value::Object(object) if object.contains_key("$repeat") => {
            let body = object["$repeat"][0].as_str().unwrap().repeat(object["$repeat"][1].as_u64().unwrap() as usize);
            let pad = |at: usize| object.get("$pad").and_then(|pad| pad[at].as_str()).unwrap_or_default();
            Value::String(format!("{}{body}{}", pad(0), pad(1)))
        }
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
    // Every scope there is, so only the payload can stop a call.
    let auth = t3_server::auth::AuthService::new(h.store.clone(), [7; 32], "t3_session_rpc".into(), "loopback-browser".into()).unwrap();
    let scopes = vec![OrchestrationRead, OrchestrationOperate, SettingsWrite, ProvidersManage, EnvironmentMaintain, PreviewOperate, DiagnosticsRead, TerminalRead, TerminalOperate, SourceControlWrite, FilesystemRead, FilesystemWrite, ReviewWrite, AccessRead, AccessWrite, RelayRead, RelayWrite];
    let token = auth.issue_session("admission", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let mut socket = h.connect(&token).await;
    let mut refused: BTreeMap<String, Vec<(Value, String)>> = BTreeMap::new();
    for line in include_str!("fixtures/rpc-decode-cases.jsonl").lines().filter(|line| !line.is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["accepted"] == false {
            refused.entry(row["method"].as_str().unwrap().to_owned()).or_default().push((expand(&row["input"]), row["message"].as_str().unwrap().to_owned()));
        }
    }
    assert!(refused.len() >= 170, "{} methods", refused.len());
    let before = h.store.latest_sequence().unwrap();
    let mut id = 0;
    for (method, payloads) in &refused {
        let stride = (payloads.len() / if method == "orchestration.dispatchCommand" { 200 } else { 12 }).max(1);
        for (payload, message) in payloads.iter().step_by(stride) {
            id += 1;
            let exit = rpc(&mut socket, id, method, payload.clone()).await;
            // A defect carrying the library's text, as the original's server sends it; not a typed error, not a serde message.
            assert_eq!(defect(&exit), &json!(message), "{method} {payload}");
        }
    }
    assert!(id > 1500, "{id} calls");

    // A method the original does not have is a defect too, whatever the scopes.
    let exit = rpc(&mut socket, 100_000, "nope.method", json!({})).await;
    assert_eq!(defect(&exit), &json!("Unknown request tag: nope.method"));
    // An accepted request with no side effect still works, and is answered.
    let exit = rpc(&mut socket, 100_001, "server.probe", json!({})).await;
    assert_eq!(success(&exit), &json!({}));

    // The scope is checked before the payload: a caller without it learns nothing of the schema.
    let mut reader = h.connect(&h.reader).await;
    let exit = rpc(&mut reader, 1, "terminal.write", json!({"data":1})).await;
    assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{exit}");
    assert_eq!(h.store.latest_sequence().unwrap(), before);
    h.finish().await;
}
