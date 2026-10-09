//! `server.upsertKeybinding`, `server.removeKeybinding`, and the keybindings in `server.getConfig`
//! and `subscribeServerConfig`, over a real socket and a real file in a temporary directory.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn config_path(h: &Harness) -> PathBuf {
    h.base.join("userdata").join("keybindings.json")
}
fn commands(config: &Value) -> Vec<&str> {
    config["keybindings"].as_array().unwrap().iter().map(|rule| rule["command"].as_str().unwrap()).collect()
}
fn rule<'a>(keybindings: &'a Value, command: &str) -> Vec<&'a Value> {
    keybindings.as_array().unwrap().iter().filter(|rule| rule["command"] == command).collect()
}
/// An editor-style save: the new content lands in one rename.
fn save(path: &PathBuf, contents: &str) {
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, contents).unwrap();
    std::fs::rename(staging, path).unwrap();
}

/// Reads stream messages until one `Chunk` value satisfies `wanted`, acknowledging each chunk.
async fn event_where(socket: &mut Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = next(socket).await;
        assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
        if message["_tag"] != "Chunk" {
            continue;
        }
        send(socket, json!({"_tag":"Ack","requestId":id})).await;
        for value in message["values"].as_array().unwrap() {
            if wanted(value) {
                return value.clone();
            }
        }
    }
}

#[tokio::test]
async fn get_config_returns_the_real_resolved_bindings_and_the_synced_file() {
    let h = harness_with_config().await;
    let mut socket = h.connect(&h.reader).await;
    let config = success(&rpc(&mut socket, 1, "server.getConfig", json!({})).await).clone();
    assert_eq!(config["keybindingsConfigPath"], config_path(&h).to_string_lossy().as_ref());
    assert_eq!(config["issues"], json!([]));
    assert_eq!(config["keybindings"].as_array().unwrap().len(), 80);
    // The first start wrote every default into the file, as the original's startup sync does.
    let persisted: Value = serde_json::from_str(&std::fs::read_to_string(config_path(&h)).unwrap()).unwrap();
    assert_eq!(persisted.as_array().unwrap().len(), 80);
    assert_eq!(persisted[0], json!({"key":"mod+b","command":"sidebar.toggle"}));
    h.finish().await;
}

#[tokio::test]
async fn upsert_replace_and_remove_persist_and_return_the_resolved_bindings() {
    let h = harness_with_config().await;
    let mut socket = h.connect(&h.operator).await;
    // The first request starts the service and its default sync; then the user's own file replaces
    // what the sync wrote: one customised command, with the defaults filling in beside it.
    success(&rpc(&mut socket, 100, "server.getConfig", json!({})).await);
    save(&config_path(&h), &json!([{"key":"mod+shift+y","command":"terminal.toggle"}]).to_string());
    let first = success(&rpc(&mut socket, 1, "server.upsertKeybinding", json!({"key":" mod+shift+r ","command":"script.run-tests.run","when":" terminalFocus "})).await).clone();
    assert_eq!(first["issues"], json!([]));
    let tests = rule(&first["keybindings"], "script.run-tests.run");
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["shortcut"], json!({"key":"r","metaKey":false,"ctrlKey":false,"shiftKey":true,"altKey":false,"modKey":true}));
    assert_eq!(tests[0]["whenAst"], json!({"type":"identifier","name":"terminalFocus"}));
    // The customised command replaced its default.
    assert_eq!(rule(&first["keybindings"], "terminal.toggle").len(), 1);
    let persisted: Value = serde_json::from_str(&std::fs::read_to_string(config_path(&h)).unwrap()).unwrap();
    assert_eq!(persisted, json!([
        {"key":"mod+shift+y","command":"terminal.toggle"},
        {"key":"mod+shift+r","command":"script.run-tests.run","when":"terminalFocus"},
    ]));

    // Replace moves the rule to the end with its new key.
    let second = success(&rpc(&mut socket, 2, "server.upsertKeybinding", json!({"key":"mod+alt+r","command":"script.run-tests.run","when":"terminalFocus","replace":{"key":"mod+shift+r","command":"script.run-tests.run","when":"terminalFocus"}})).await).clone();
    assert_eq!(rule(&second["keybindings"], "script.run-tests.run").len(), 1);
    assert_eq!(rule(&second["keybindings"], "script.run-tests.run")[0]["shortcut"]["altKey"], true);

    // getConfig sees the same bindings without waiting for the watcher.
    let config = success(&rpc(&mut socket, 3, "server.getConfig", json!({})).await).clone();
    assert_eq!(config["keybindings"], second["keybindings"]);

    // Remove restores the default for a command whose custom rule goes away.
    let third = success(&rpc(&mut socket, 4, "server.removeKeybinding", json!({"key":"mod+shift+y","command":"terminal.toggle"})).await).clone();
    assert_eq!(rule(&third["keybindings"], "terminal.toggle")[0]["shortcut"]["key"], "j");
    // Removing something absent is not an error.
    let again = success(&rpc(&mut socket, 5, "server.removeKeybinding", json!({"key":"mod+q","command":"chat.new"})).await).clone();
    assert_eq!(again["keybindings"], third["keybindings"]);
    let persisted: Value = serde_json::from_str(&std::fs::read_to_string(config_path(&h)).unwrap()).unwrap();
    assert_eq!(persisted, json!([{"key":"mod+alt+r","command":"script.run-tests.run","when":"terminalFocus"}]));
    h.finish().await;
}

#[tokio::test]
async fn bad_payloads_malformed_files_and_missing_scopes_are_refused_as_the_original_refuses_them() {
    let h = harness_with_config().await;
    let mut operator = h.connect(&h.operator).await;
    let mut reader = h.connect(&h.reader).await;
    // Prime the service so the startup sync has run before the file is replaced.
    success(&rpc(&mut operator, 1, "server.getConfig", json!({})).await);

    for (id, payload) in [
        (10, json!({"key":"","command":"terminal.toggle"})),
        (11, json!({"key":"mod+j","command":"not.a.command"})),
        (12, json!({"key":"mod+j","command":"script.A.run"})),
        (13, json!({"key":"mod+j"})),
        (14, json!({"key":"mod+j","command":"terminal.toggle","when":5})),
        (15, Value::Null),
    ] {
        defect(&rpc(&mut operator, id, "server.upsertKeybinding", payload.clone()).await);
        defect(&rpc(&mut operator, id + 10, "server.removeKeybinding", payload).await);
    }

    // A file that is not an array is never overwritten.
    std::fs::write(config_path(&h), r#"{"key":"mod+j","command":"terminal.toggle"}"#).unwrap();
    for (id, tag, payload) in [
        (30, "server.upsertKeybinding", json!({"key":"mod+shift+r","command":"script.run-tests.run"})),
        (31, "server.removeKeybinding", json!({"key":"mod+shift+r","command":"script.run-tests.run"})),
    ] {
        let exit = rpc(&mut operator, id, tag, payload).await;
        let error = failure(&exit);
        assert_eq!(error["_tag"], "KeybindingsConfigParseError");
        assert_eq!(error["configPath"], config_path(&h).to_string_lossy().as_ref());
        assert_eq!(error["detail"], "expected JSON array");
        assert_eq!(error["cause"], json!({"name":"SchemaError","message":"Expected array"}));
    }
    assert_eq!(std::fs::read_to_string(config_path(&h)).unwrap(), r#"{"key":"mod+j","command":"terminal.toggle"}"#);

    // Without settings:write nothing is read or written.
    for (id, tag) in [(40, "server.upsertKeybinding"), (41, "server.removeKeybinding")] {
        let exit = rpc(&mut reader, id, tag, json!({"key":"mod+shift+r","command":"script.run-tests.run"})).await;
        let error = failure(&exit);
        assert_eq!(error["_tag"], "EnvironmentAuthorizationError", "{exit}");
        assert_eq!(error["operation"], tag);
        assert_eq!(error["requiredPermission"], "settings:write");
        assert_eq!(error["requiredScope"], "orchestration:operate");
    }
    assert_eq!(std::fs::read_to_string(config_path(&h)).unwrap(), r#"{"key":"mod+j","command":"terminal.toggle"}"#);
    h.finish().await;
}

#[tokio::test]
async fn external_edits_are_published_to_subscribers_and_a_malformed_file_keeps_the_file_and_reports_the_issue() {
    let h = harness_with_config().await;
    let mut stream = h.connect(&h.reader).await;
    send(&mut stream, json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
    let snapshot = event_where(&mut stream, 1, |event| event["type"] == "snapshot").await;
    assert_eq!(snapshot["config"]["keybindings"].as_array().unwrap().len(), 80);
    assert_eq!(snapshot["config"]["issues"], json!([]));

    // Another process saves a custom binding.
    let custom = json!([{"key":"mod+shift+y","command":"terminal.toggle"}]).to_string();
    save(&config_path(&h), &custom);
    let updated = event_where(&mut stream, 1, |event| {
        event["type"] == "keybindingsUpdated" && event["payload"]["keybindings"].as_array().is_some_and(|rules| rules.iter().any(|rule| rule["command"] == "terminal.toggle" && rule["shortcut"]["key"] == "y"))
    })
    .await;
    assert_eq!(updated["payload"]["issues"], json!([]));

    // A broken edit: defaults apply and the issue is reported; the file is left exactly as saved.
    save(&config_path(&h), "[{ not json");
    let malformed = event_where(&mut stream, 1, |event| event["type"] == "keybindingsUpdated" && !event["payload"]["issues"].as_array().unwrap().is_empty()).await;
    assert_eq!(malformed["payload"]["issues"], json!([{"kind":"keybindings.malformed-config","message":"expected JSON array (SchemaError: Expected a valid JSON string)"}]));
    assert_eq!(rule(&malformed["payload"]["keybindings"], "terminal.toggle")[0]["shortcut"]["key"], "j");
    assert_eq!(std::fs::read_to_string(config_path(&h)).unwrap(), "[{ not json");

    // One bad entry among good ones: the good ones apply, the bad one is reported by index.
    save(&config_path(&h), &json!([{"key":"mod+shift+y","command":"terminal.toggle"},{"key":"mod+shift+d+o","command":"terminal.new"}]).to_string());
    let partial = event_where(&mut stream, 1, |event| {
        event["type"] == "keybindingsUpdated" && event["payload"]["issues"].as_array().is_some_and(|issues| issues.len() == 1 && issues[0]["kind"] == "keybindings.invalid-entry")
    })
    .await;
    assert_eq!(partial["payload"]["issues"][0]["index"], 1);
    assert_eq!(partial["payload"]["issues"][0]["message"], "SchemaError: Invalid keybinding rule");
    assert_eq!(rule(&partial["payload"]["keybindings"], "terminal.toggle")[0]["shortcut"]["key"], "y");

    // A change made through the socket by another connection reaches this subscriber too.
    save(&config_path(&h), &custom);
    let mut writer = h.connect(&h.operator).await;
    success(&rpc(&mut writer, 2, "server.upsertKeybinding", json!({"key":"mod+shift+r","command":"script.run-tests.run"})).await);
    let pushed = event_where(&mut stream, 1, |event| {
        event["type"] == "keybindingsUpdated" && !rule(&event["payload"]["keybindings"], "script.run-tests.run").is_empty() && event["payload"]["issues"] == json!([])
    })
    .await;
    assert!(commands(&pushed["payload"]).contains(&"script.run-tests.run"));
    h.finish().await;
}
