#![cfg(unix)]
//! A call the original's decoding refuses is refused over the real MCP HTTP transport with the
//! original's text, and does nothing: no event is persisted. The refused inputs are the ones
//! `tools/generate_mcp_tool_parameters.mjs` found the original refusing, spread over every tool.
mod mcp_support;
use mcp_support::*;
use serde_json::{Value, json};
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

async fn quiet_thread(h: &Harness) -> String {
    h.start_thread("parent", "fixture-model", "full-access", "finish this");
    h.until("parent", |view| view["runs"].as_array().unwrap().iter().all(|run| run["status"] == "completed").then_some(())).await;
    "parent".into()
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_arguments_get_the_originals_text_and_change_nothing() {
    let h = Harness::new().await;
    // A finished thread is quiet: nothing but the calls under test could persist an event.
    let parent = quiet_thread(&h).await;
    let caller = h.caller(&parent).await;
    // Refused object arguments by tool, kept to an even spread so the run stays short.
    let mut refused: BTreeMap<String, Vec<(Value, String)>> = BTreeMap::new();
    for line in include_str!("fixtures/mcp-tool-parameters.jsonl").lines().filter(|line| !line.is_empty()) {
        let row: Value = serde_json::from_str(line).unwrap();
        let input = expand(&row["input"]);
        // The device tools are the device service's, which decodes them on its own path.
        if row["accepted"] == false && input.is_object() && !row["name"].as_str().unwrap().starts_with("device_") {
            refused.entry(row["name"].as_str().unwrap().to_owned()).or_default().push((input, row["error"].as_str().unwrap().to_owned()));
        }
    }
    assert!(refused.len() >= 70, "{} tools", refused.len());
    let before = h.store.latest_sequence().unwrap();
    let mut calls = 0;
    for (name, rows) in &refused {
        let stride = (rows.len() / 40).max(1);
        for (input, error) in rows.iter().step_by(stride) {
            let response = h.rpc(&caller, "tools/call", json!({"name":name,"arguments":input})).await;
            assert_eq!(response["error"]["code"], -32602, "{name}: {response}");
            assert_eq!(response["error"]["message"], *error, "{name} {input}");
            calls += 1;
        }
    }
    assert!(calls > 1500, "{calls} calls");
    assert_eq!(h.store.latest_sequence().unwrap(), before, "a refused call must not persist anything");
    h.finish().await;
}

/// The messages the model reads to correct a call, spelled out for the classes the port once got wrong.
#[tokio::test(flavor = "multi_thread")]
async fn the_classes_the_port_once_accepted_are_refused_with_the_originals_wording() {
    let h = Harness::new().await;
    // A finished thread is quiet: nothing but the calls under test could persist an event.
    let parent = quiet_thread(&h).await;
    let caller = h.caller(&parent).await;
    let before = h.store.latest_sequence().unwrap();
    for (name, arguments, expected) in [
        // A blank id is refused, not trimmed into a lookup.
        ("t3_thread_send", json!({"threadId":"  ","message":"hi"}), "Invalid parameters for tool 't3_thread_send': Expected a non-blank string\n  at [\"threadId\"]"),
        ("t3_thread_send", json!({"threadId":"parent","message":"hi","clientRequestId":"  "}), "Invalid parameters for tool 't3_thread_send': Expected a non-blank string\n  at [\"clientRequestId\"]"),
        // Nested required fields of a model selection, and the type error the missing alias becomes.
        ("t3_thread_configure", json!({"modelSelection":{"model":"gpt"}}), "Invalid parameters for tool 't3_thread_configure': Expected string\n  at [\"modelSelection\"][\"instanceId\"]"),
        ("t3_thread_configure", json!({"modelSelection":{"instanceId":"codex"}}), "Invalid parameters for tool 't3_thread_configure': Missing key\n  at [\"modelSelection\"][\"model\"]"),
        // An optional property is `| undefined` in the original's text, never `| null`.
        ("request_secret", json!({"label":"a","reason":"b","placeholder":null}), "Invalid parameters for tool 'request_secret': Expected string | undefined\n  at [\"placeholder\"]"),
        // A union reports its members the way the original describes them.
        ("schedule_task", json!({"prompt":"p","schedule":{"type":"interval","everyMs":10}}), "Invalid parameters for tool 'schedule_task': Expected a value greater than or equal to 60000\n  at [\"schedule\"][\"everyMs\"]"),
    ] {
        let response = h.rpc(&caller, "tools/call", json!({"name":name,"arguments":arguments})).await;
        assert_eq!(response["error"]["message"], expected, "{name}: {response}");
    }
    let persisted = h.store.events(before, None, None, None, 100).unwrap();
    assert_eq!(h.store.latest_sequence().unwrap(), before, "{}", persisted.iter().map(|event| format!("{:?}", event.event).chars().take(300).collect::<String>()).collect::<Vec<_>>().join("\n"));
    h.finish().await;
}
