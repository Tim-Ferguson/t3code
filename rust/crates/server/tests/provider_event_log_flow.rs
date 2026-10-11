//! The provider event log against the unchanged logger (`tools/generate_provider_event_log_fixtures.mjs`): which events are kept, how they
//! are bounded and written, how files rotate, batch and age out, and what a bad option says. Timestamps are masked.
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, sync::Arc};
use t3_server::provider_event_log::{EventLogStore, Options, Stream, bound, should_persist};

fn rows() -> Vec<Value> {
    include_str!("fixtures/provider-event-log.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn stream(name: &str) -> Stream {
    match name {
        "native" => Stream::Native,
        "canonical" => Stream::Canonical,
        _ => Stream::Orchestration,
    }
}

fn clock() -> Arc<dyn Fn() -> i64 + Send + Sync> {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// The files of a directory by name, with each line's time masked.
fn tree(directory: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let masked: String = text.split_inclusive('\n').map(|line| match line.strip_prefix('[').and_then(|rest| rest.split_once(']')) { Some((_, rest)) => format!("[T]{rest}"), None => line.to_owned() }).collect();
            (entry.file_name().to_string_lossy().into_owned(), masked)
        })
        .collect()
}

fn expected(files: &Value) -> BTreeMap<String, String> {
    files.as_object().unwrap().iter().map(|(name, text)| (name.clone(), text.as_str().unwrap().to_owned())).collect()
}

fn options(overrides: &Value) -> Options {
    let mut options = Options::default();
    let number = |name: &str| overrides[name].as_i64();
    options.max_bytes = number("maxBytes").unwrap_or(options.max_bytes);
    options.max_files = number("maxFiles").unwrap_or(options.max_files);
    options.batch_window_ms = number("batchWindowMs").unwrap_or(options.batch_window_ms);
    options.max_total_bytes = number("maxTotalBytes").unwrap_or(options.max_total_bytes);
    options.max_age_ms = number("maxAgeMs").unwrap_or(options.max_age_ms);
    options.max_buffered_records = number("maxBufferedRecords").unwrap_or(options.max_buffered_records);
    options
}

fn event_of(events: &[Value], reference: &Value) -> Value {
    match reference.as_u64() {
        Some(index) => events[index as usize][1].clone(),
        None => reference.clone(),
    }
}

fn write_all(store: &EventLogStore, events: &[Value], writes: &Value) {
    for write in writes.as_array().unwrap() {
        store.logger(stream(write["stream"].as_str().unwrap())).write(&event_of(events, &write["event"]), write["threadId"].as_str());
    }
}

#[tokio::test]
async fn the_log_filters_bounds_writes_rotates_batches_and_ages_out_as_the_original_does() {
    let rows = rows();
    let events: Vec<Value> = rows[0]["events"].as_array().unwrap().clone();
    let mut kinds = BTreeMap::<String, usize>::new();
    for row in &rows[1..] {
        let kind = row["kind"].as_str().unwrap().to_owned();
        *kinds.entry(kind.clone()).or_default() += 1;
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("provider").join("events.log");
        match kind.as_str() {
            "filter" => {
                let (stream, event) = (stream(row["stream"].as_str().unwrap()), &events[row["index"].as_u64().unwrap() as usize][1]);
                let name = row["name"].as_str().unwrap();
                assert_eq!(should_persist(stream, event), row["persist"].as_bool().unwrap(), "{stream:?}: {name}");
                let bounded = bound(event);
                match &row["bounded"] {
                    Value::String(same) if same == "same" => assert_eq!(&bounded, event, "{name}"),
                    summary => assert_eq!(&bounded, summary, "{name}"),
                }
            }
            "write" | "rotate" => {
                let store = EventLogStore::open(&log, if kind == "rotate" { options(&row["options"]) } else { Options { batch_window_ms: 0, ..Options::default() } }, clock(), None).unwrap();
                write_all(&store, &events, &row["writes"]);
                store.close();
                assert_eq!(tree(log.parent().unwrap()), expected(&row["files"]), "{}", row["name"]);
            }
            "batch" => {
                let store = EventLogStore::open(&log, options(&row["options"]), clock(), None).unwrap();
                let logger = store.logger(Stream::Native);
                let rows_of = |name: &str| expected(&row[name]);
                let events3 = row["events"].as_array().unwrap();
                for event in &events3[..2] {
                    logger.write(event, Some("t"));
                }
                assert_eq!(tree(log.parent().unwrap()), rows_of("before"), "two records wait for the window");
                logger.write(&events3[2], Some("t"));
                assert_eq!(tree(log.parent().unwrap()), rows_of("after"), "the third reaches the count");
                logger.write(&events3[3], Some("t"));
                assert_eq!(tree(log.parent().unwrap()), rows_of("pending"));
                store.close();
                assert_eq!(tree(log.parent().unwrap()), rows_of("closed"), "close writes what is buffered");
                logger.write(&events3[0], Some("t"));
                assert_eq!(tree(log.parent().unwrap()), rows_of("closed"), "a closed store writes nothing");
            }
            "retention" => {
                let directory = log.parent().unwrap();
                std::fs::create_dir_all(directory).unwrap();
                let now = std::time::SystemTime::now();
                for file in row["files"].as_array().unwrap() {
                    let path = directory.join(file["name"].as_str().unwrap());
                    std::fs::write(&path, file["text"].as_str().map_or_else(|| "x".repeat(file["size"].as_u64().unwrap() as usize), str::to_owned)).unwrap();
                    let age = std::time::Duration::from_millis((file["ageDays"].as_f64().unwrap() * 86_400_000.0) as u64);
                    std::fs::File::options().write(true).open(&path).unwrap().set_modified(now - age).unwrap();
                }
                EventLogStore::open(&log, options(&row["options"]), clock(), None).unwrap().close();
                let mut kept: Vec<String> = std::fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
                kept.sort();
                assert_eq!(serde_json::json!(kept), row["kept"], "{}", row["name"]);
            }
            "option" => {
                // A fraction is not an integer; the Rust options are integers to begin with.
                let Some(value) = row["value"].as_i64() else { continue };
                let mut bad = Options::default();
                match row["option"].as_str().unwrap() {
                    "maxBytes" => bad.max_bytes = value,
                    "maxFiles" => bad.max_files = value,
                    "batchWindowMs" => bad.batch_window_ms = value,
                    "maxTotalBytes" => bad.max_total_bytes = value,
                    "maxAgeMs" => bad.max_age_ms = value,
                    "retentionCheckIntervalMs" => bad.retention_check_interval_ms = value,
                    "maxBufferedBytes" => bad.max_buffered_bytes = value,
                    "maxBufferedRecords" => bad.max_buffered_records = value,
                    other => panic!("{other}"),
                }
                let path = scratch.path().join("options").join("events.log");
                let error = EventLogStore::open(&path, bad, clock(), None).err().unwrap();
                assert_eq!(error.to_string().replace(&scratch.path().to_string_lossy().into_owned(), "<scratch>"), row["message"].as_str().unwrap());
            }
            other => panic!("{other}"),
        }
    }
    assert_eq!(kinds.keys().map(String::as_str).collect::<Vec<_>>(), ["batch", "filter", "option", "retention", "rotate", "write"]);
}

/// The tap a protocol connection is handed writes decoded frames to the thread's file, the summary writer a bounded structural line, and
/// a server with no log hands out nothing.
#[tokio::test]
async fn adapters_write_through_the_servers_log() {
    use t3_server::{persistence::Store, provider_event_log as log};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    assert!(log::protocol_tap(&store, "claudeAgent", "claude-agent-sdk.query", "thread-1", "session-1").is_none());
    let logs = directory.path().join("logs").join("provider");
    log::install(&store, EventLogStore::open(&logs.join("events.log"), Options { batch_window_ms: 0, ..Options::default() }, clock(), None).unwrap());

    let tap = log::protocol_tap(&store, "claudeAgent", "claude-agent-sdk.query", "Thread-1", "session-1").unwrap();
    tap("incoming", &serde_json::json!({"type":"assistant","message":{"id":"m"}}));
    tap("incoming", &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta"}}));
    tap("outgoing", &serde_json::json!({"type":"user","message":"hi"}));
    let logger = log::native(&store).unwrap();
    log::write_protocol_summary(&logger, "opencode", Some("opencode-sdk.sse"), "opencode", "session-2", "thread-1", "incoming", "notification", "message.updated", &serde_json::json!({"secret":"prompt text","_tag":"Odd Tag!","tag":"has space"}));
    log::current(&store).unwrap().close();

    let files = tree(&logs);
    let lines: Vec<Value> = files["events.thread-1.log"].lines().map(|line| serde_json::from_str(line.split_once("NTIVE: ").unwrap().1).unwrap()).collect();
    assert_eq!(lines.len(), 3, "the content delta is not kept: {files:?}");
    assert_eq!(lines[0], serde_json::json!({"provider":"claudeAgent","protocol":"claude-agent-sdk.query","kind":"protocol","providerSessionId":"session-1","event":{"direction":"incoming","stage":"decoded","payload":{"type":"assistant","message":{"id":"m"}}}}));
    assert_eq!(lines[1]["event"]["direction"], "outgoing");
    let summary = &lines[2]["event"];
    assert_eq!((summary["kind"].as_str(), summary["protocol"].as_str(), summary["providerSessionId"].as_str(), summary["threadId"].as_str()), (Some("protocol"), Some("opencode-sdk.sse"), Some("session-2"), Some("thread-1")));
    assert_eq!(summary["payload"], serde_json::json!({"direction":"incoming","messageKind":"notification","method":"message.updated","payload":{"valueType":"object","fieldCount":3,"messageTag":"TaggedError","method":"unknown"}}));
    assert!(!files["events.thread-1.log"].contains("prompt text"));
}
