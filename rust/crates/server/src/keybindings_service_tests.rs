//! Replays the file-backed rows of `tests/fixtures/keybindings.jsonl` against the owner: how a file
//! loads, what the startup sync writes, what upsert and remove persist and report, and which
//! payloads decode. Real files in temporary directories.
use super::*;
use crate::keybindings::tests::{rows, rows_of};
use crate::keybindings::{encode_shortcut, encode_when};

struct Fixture {
    _directory: tempfile::TempDir,
    owner: Owner,
}

fn fixture(file: Option<&str>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap().join("userdata").join("keybindings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    match file {
        Some("DIRECTORY") => std::fs::create_dir(&path).unwrap(),
        Some(contents) => std::fs::write(&path, contents).unwrap(),
        None => {}
    }
    Fixture { _directory: directory, owner: Owner { path, cached: None, listeners: Vec::new(), watcher: None } }
}

impl Fixture {
    fn file(&self) -> Option<String> {
        std::fs::read_to_string(&self.owner.path).ok()
    }
    fn migrations(&self) -> Option<String> {
        std::fs::read_to_string(self.owner.migrations_path()).ok()
    }
}

/// `[command, key, when]` per rule, as the generator records them.
fn compact(rules: &[Resolved]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| {
                json!([
                    rule.command,
                    encode_shortcut(&rule.shortcut).unwrap(),
                    rule.when_ast.as_ref().map(encode_when)
                ])
            })
            .collect(),
    )
}

#[test]
fn files_load_like_the_original() {
    let rows = rows_of("state");
    assert!(rows.len() > 60);
    for row in rows {
        let content = row["content"].as_str().unwrap();
        let mut fixture = fixture(Some(content));
        let state = fixture.owner.load_state().unwrap();
        assert_eq!(Value::Array(state.issues.clone()), row["issues"], "issues of {content:?}");
        assert_eq!(compact(&state.keybindings), row["keybindings"], "bindings of {content:?}");
        if !row["full"].is_null() {
            assert_eq!(state.keybindings_json(), row["full"], "{content:?}");
        }
        // Loading never rewrites the file.
        assert_eq!(fixture.file().as_deref(), Some(content));
    }
}

#[test]
fn an_unreadable_file_fails_the_read_and_is_retried() {
    let row = rows_of("unreadable").remove(0);
    let mut fixture = fixture(Some("DIRECTORY"));
    let error = fixture.owner.load_state().unwrap_err();
    assert_eq!(error.detail, row["loadDetail"]);
    assert_eq!(error.wire()["cause"]["name"], row["loadCause"]);
    // The failure is not cached: once a good file replaces the directory, the next read succeeds.
    std::fs::remove_dir(&fixture.owner.path).unwrap();
    std::fs::write(&fixture.owner.path, r#"[{"key":"mod+j","command":"terminal.toggle"}]"#).unwrap();
    let state = fixture.owner.load_state().unwrap();
    assert!(state.issues.is_empty());
    assert!(state.keybindings.iter().any(|rule| rule.command == "terminal.toggle"));
}

#[test]
fn the_startup_sync_persists_what_the_original_persists() {
    let rows = rows_of("sync");
    assert!(rows.len() >= 15);
    for row in rows {
        let name = row["name"].as_str().unwrap();
        let mut fixture = fixture(row["file"].as_str());
        if let Some(migrations) = row["migrations"].as_str() {
            std::fs::write(fixture.owner.migrations_path(), migrations).unwrap();
        }
        for (index, step) in row["steps"].as_array().unwrap().iter().enumerate() {
            if index > 0 {
                if let Some(rewrite) = row["rewriteBetween"].as_str() {
                    std::fs::write(&fixture.owner.path, rewrite).unwrap();
                }
            }
            assert_eq!(fixture.owner.sync_defaults().is_ok(), step["ok"], "{name} step {index}");
            assert_eq!(fixture.file().map(Value::from).unwrap_or(Value::Null), step["file"], "{name} step {index}: file");
            assert_eq!(fixture.migrations().map(Value::from).unwrap_or(Value::Null), step["migrations"], "{name} step {index}: migrations");
        }
    }
}

fn rule_of(value: &Value) -> Rule {
    Rule {
        key: value["key"].as_str().unwrap().into(),
        command: value["command"].as_str().unwrap().into(),
        when: value["when"].as_str().map(str::to_owned),
    }
}

/// The original's error with the config path scrubbed. Platform error causes carry the host's own
/// operating-system text, so they are compared down to the platform error's name and message.
fn assert_error_matches(error: &KeybindingsError, expected: &Value, context: &str) {
    let path = error.config_path.to_string_lossy().into_owned();
    let expected: Value = serde_json::from_str(&expected.to_string().replace("<CONFIG>", &path.replace('\\', "\\\\"))).unwrap();
    let mut actual = error.wire();
    for value in [&mut actual, &mut expected.clone()] {
        let _ = value;
    }
    assert_eq!(actual["_tag"], expected["_tag"], "{context}");
    assert_eq!(actual["configPath"], expected["configPath"], "{context}");
    assert_eq!(actual["detail"], expected["detail"], "{context}");
    let strip = |cause: &Value| -> Value {
        // Down to (name, message) of each level, dropping the OS error at the bottom.
        let mut levels = Vec::new();
        let mut current = cause;
        while current.is_object() {
            levels.push(json!([current["name"], current["message"]]));
            if current["name"] == "Error" {
                levels.pop();
            }
            current = &current["cause"];
        }
        Value::Array(levels)
    };
    assert_eq!(strip(&actual["cause"]), strip(&expected["cause"]), "{context}");
}

#[test]
fn upsert_and_remove_persist_and_report_like_the_original() {
    let rows = rows_of("mutation");
    assert!(rows.len() >= 25);
    for row in rows {
        let name = row["name"].as_str().unwrap();
        if row["readonlyDirectory"] == true && unsafe { libc::geteuid() } == 0 {
            continue; // root writes into a read-only directory
        }
        let mut fixture = fixture(row["file"].as_str());
        if row["readonlyDirectory"] == true {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(fixture.owner.path.parent().unwrap(), std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        for (index, (op, expected)) in row["ops"].as_array().unwrap().iter().zip(row["results"].as_array().unwrap()).enumerate() {
            let context = format!("{name} op {index}");
            let input = &op[1];
            let result = match op[0].as_str().unwrap() {
                "upsert" => {
                    let replace = input.get("replace").map(rule_of);
                    fixture.owner.upsert(rule_of(input), replace)
                }
                _ => fixture.owner.remove(rule_of(input)),
            };
            match result {
                Ok(resolved) => {
                    assert!(expected.get("ok").is_some(), "{context}: unexpected success");
                    assert_eq!(compact(&resolved), expected["ok"], "{context}");
                    assert_eq!(resolved.len() as u64, expected["count"].as_u64().unwrap(), "{context}");
                    // A write also makes its result the cached state, with no issues.
                    assert_eq!(fixture.owner.cached.as_ref().unwrap().issues, Vec::<Value>::new());
                }
                Err(error) => {
                    assert!(expected.get("error").is_some(), "{context}: unexpected failure {error:?}");
                    assert_error_matches(&error, &expected["error"], &context);
                }
            }
            assert_eq!(fixture.file().map(Value::from).unwrap_or(Value::Null), expected["file"], "{context}: file");
        }
        if row["readonlyDirectory"] == true {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(fixture.owner.path.parent().unwrap(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

#[test]
fn payloads_decode_like_the_original() {
    use t3_contracts::{ServerKeybindingTarget, ServerUpsertKeybindingInput};
    let rows = rows_of("payload");
    assert!(rows.len() > 30);
    for row in rows {
        let payload = row["payload"].clone();
        let upsert = serde_json::from_value::<ServerUpsertKeybindingInput>(payload.clone()).map(|v| serde_json::to_value(v).unwrap());
        match (&upsert, row["upsert"].get("ok")) {
            (Ok(actual), Some(expected)) => assert_eq!(actual, expected, "upsert {payload}"),
            (Err(_), None) => {}
            other => panic!("upsert {payload}: {other:?}"),
        }
        let remove = serde_json::from_value::<ServerKeybindingTarget>(payload.clone()).map(|v| serde_json::to_value(v).unwrap());
        match (&remove, row["remove"].get("ok")) {
            (Ok(actual), Some(expected)) => assert_eq!(actual, expected, "remove {payload}"),
            (Err(_), None) => {}
            other => panic!("remove {payload}: {other:?}"),
        }
    }
    let _ = rows;
}

#[test]
fn rows_are_all_replayed() {
    // Guards the fixture against a new row type that no test replays.
    let known = ["constants", "shortcut", "when", "compile", "defaults", "state", "unreadable", "sync", "mutation", "payload"];
    for row in rows() {
        assert!(known.contains(&row["type"].as_str().unwrap()), "{}", row["type"]);
    }
}
