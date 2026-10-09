//! `server.getTraceDiagnostics`: the original reads the same rotated trace files
//! (`generate_trace_diagnostics_fixtures.mjs`) and the Rust reader must produce the same result,
//! then the method over a real socket with its scope.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use t3_server::trace_diagnostics::read;

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
}
/// Effect's `Option` also prints an `_id`; numbers are compared as numbers, not as integers or floats.
fn comparable(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.iter().filter(|(key, _)| key.as_str() != "_id").map(|(key, value)| (key.clone(), comparable(value))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(comparable).collect()),
        Value::Number(number) => json!(number.as_f64().unwrap()),
        other => other.clone(),
    }
}
const READ_AT: i64 = 1_791_504_000_000; // 2026-10-09T00:00:00Z

#[test]
fn the_reader_agrees_with_the_original_on_rotated_trace_files() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/trace-diagnostics.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    for file in fixture["files"].as_array().unwrap() {
        std::fs::write(base.join(file["name"].as_str().unwrap()), unhex(file["hex"].as_str().unwrap())).unwrap();
    }
    for case in fixture["cases"].as_array().unwrap() {
        let path = base.join(case["file"].as_str().unwrap());
        let actual = read(path.to_str().unwrap(), case["maxFiles"].as_u64().unwrap() as usize, READ_AT);
        let actual: Value = serde_json::from_str(&actual.to_string().replace(base.to_str().unwrap(), "{base}")).unwrap();
        assert_eq!(comparable(&actual), comparable(&case["result"]), "{}", case["name"]);
    }
}

#[test]
fn an_unreadable_trace_file_is_reported_without_losing_the_rest() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let main = base.join("server.trace.ndjson");
    std::fs::write(&main, "").unwrap();
    // A directory where a backup should be: reading it fails, which is not "not found".
    std::fs::create_dir(base.join("server.trace.ndjson.1")).unwrap();
    let result = read(main.to_str().unwrap(), 1, READ_AT);
    assert_eq!(result["partialFailure"], json!({"_tag":"Some","value":true}));
    assert_eq!(result["error"]["value"]["kind"], "trace-file-read-failed");
    assert!(result["error"]["value"]["message"].as_str().unwrap().starts_with("Failed to read local trace file '"));
}

#[test]
fn the_method_reads_the_logs_directory_and_needs_diagnostics_access() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let mut operator = h.connect(&h.operator).await;
        let mut reader = h.connect(&h.reader).await;
        let denied = rpc(&mut reader, 1, "server.getTraceDiagnostics", json!({})).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
        assert_eq!(failure(&denied)["requiredPermission"], "diagnostics:read");
        let none = success(&rpc(&mut operator, 2, "server.getTraceDiagnostics", json!({})).await).clone();
        assert_eq!(none["error"]["value"]["kind"], "trace-file-not-found");
        assert_eq!(none["traceFilePath"], h.base.join("userdata/logs/server.trace.ndjson").to_str().unwrap());
        let line = json!({"name":"ws.request","traceId":"t","spanId":"s","startTimeUnixNano":"1000000000","endTimeUnixNano":"2000000000","durationMs":1000,"exit":{"_tag":"Success"}});
        std::fs::create_dir_all(h.base.join("userdata/logs")).unwrap();
        std::fs::write(h.base.join("userdata/logs/server.trace.ndjson"), format!("{line}\n")).unwrap();
        let some = success(&rpc(&mut operator, 3, "server.getTraceDiagnostics", json!({})).await).clone();
        assert_eq!((some["recordCount"].as_u64(), some["slowSpanCount"].as_u64(), some["error"]["_tag"].as_str()), (Some(1), Some(1), Some("None")));
        assert_eq!(some["topSpansByCount"][0]["name"], "ws.request");
        assert!(!defect(&rpc(&mut operator, 4, "server.getTraceDiagnostics", Value::Null).await).is_null());
        h.finish().await;
    }));
}
