//! The local trace file and the route a browser exports its spans to (`packages/shared/src/observability.ts`: `makeTraceSink` and
//! `decodeOtlpTraceRecords`; `BrowserTraceCollector.ts`; the `/api/observability/v1/traces` route of `http.ts`).
//!
//! Records are appended to `<logs>/server.trace.ndjson` in batches, and the file rotates by size like the other logs. This is
//! what `server.getTraceDiagnostics` (`trace_diagnostics.rs`) folds into the diagnostics page.
//!
//! Divergence: the original also writes a record for every Effect span the server itself runs. This server's own spans are the
//! `ws.rpc.*` ones (`rpc_spans.rs`), which are written here and exported over OTLP when a traces endpoint is configured (`otlp.rs`); the
//! browsers' spans are forwarded to that endpoint as they arrive.
use crate::{persistence::Store, transport::{ApiState, authenticate, require}};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Map, Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::AuthEnvironmentScope;

pub const OTLP_TRACES_PROXY_PATH: &str = "/api/observability/v1/traces";
/// `traceMaxBytes`, `traceMaxFiles` and `traceBatchWindowMs` of the original's config.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_FILES: usize = crate::trace_diagnostics::TRACE_MAX_FILES;
pub const BATCH_WINDOW: Duration = Duration::from_millis(200);
/// `FLUSH_BUFFER_THRESHOLD`: a batch this long is written without waiting for the window.
const FLUSH_BUFFER_THRESHOLD: usize = 256;
/// A browser exports what it buffered in a second; far more than this is not a trace.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

// ---- the rotating file (`RotatingFileSink`) -----------------------------------------------------

pub(crate) struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    size: u64,
}
impl RotatingFile {
    pub(crate) fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> std::io::Result<Self> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        let mut file = Self { path, max_bytes, max_files, size: 0 };
        file.prune_overflow();
        file.size = file.read_size()?;
        Ok(file)
    }
    fn with_suffix(&self, index: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }
    /// `pruneOverflowBackups`: rotated files past the limit, left by a run that kept more.
    fn prune_overflow(&self) {
        let (Some(directory), Some(base)) = (self.path.parent(), self.path.file_name().and_then(|name| name.to_str())) else { return };
        let Ok(entries) = std::fs::read_dir(directory) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(base)).and_then(|rest| rest.strip_prefix('.')) else { continue };
            if suffix.parse::<usize>().is_ok_and(|index| index > self.max_files) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    fn read_size(&self) -> std::io::Result<u64> {
        match std::fs::metadata(&self.path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }
    fn rotate(&mut self) -> std::io::Result<()> {
        let _ = std::fs::remove_file(self.with_suffix(self.max_files));
        for index in (1..self.max_files).rev() {
            let source = self.with_suffix(index);
            if source.exists() {
                std::fs::rename(source, self.with_suffix(index + 1))?;
            }
        }
        if self.path.exists() {
            std::fs::rename(&self.path, self.with_suffix(1))?;
        }
        self.size = 0;
        Ok(())
    }
    /// `write`: a chunk that would pass the limit rotates first, unless the file is empty.
    pub(crate) fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.size > 0 && self.size + chunk.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?.write_all(chunk)?;
        self.size += chunk.len() as u64;
        Ok(())
    }
}

// ---- the sink (`makeTraceSink`) ------------------------------------------------------------------

struct Inner {
    file: RotatingFile,
    buffer: Vec<String>,
    /// Records dropped since writes started failing; a recovery logs once and clears it.
    dropped: usize,
    failing: bool,
    reported: bool,
    /// The OTLP traces exporter, which takes the server's own spans (`effect-span` records) as well as the file does.
    otlp: Option<crate::otlp::Exporter>,
}

/// Batches trace records and appends them to the rotating file.
#[derive(Clone)]
pub struct TraceSink(Arc<Mutex<Inner>>);

impl TraceSink {
    pub fn open(path: &Path, max_bytes: u64, max_files: usize) -> std::io::Result<Self> {
        let file = RotatingFile::open(path.to_path_buf(), max_bytes.max(1), max_files.max(1))?;
        Ok(Self(Arc::new(Mutex::new(Inner { file, buffer: Vec::new(), dropped: 0, failing: false, reported: false, otlp: None }))))
    }

    /// Writes batches on the window until the sink is dropped.
    pub fn start(&self, window: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(window).await;
                let Some(state) = weak.upgrade() else { return };
                Self(state).flush();
            }
        })
    }

    /// Exports the server's own spans from here on (`OtlpTracer` as the local tracer's delegate).
    pub fn set_otlp(&self, exporter: crate::otlp::Exporter) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).otlp = Some(exporter);
    }

    pub fn push(&self, record: &Value) {
        let Ok(line) = serde_json::to_string(record) else { return };
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let (Some(exporter), Some("effect-span")) = (&state.otlp, record["type"].as_str()) {
            exporter.push(crate::otlp::span_from_record(record));
        }
        state.buffer.push(format!("{line}\n"));
        if state.buffer.len() >= FLUSH_BUFFER_THRESHOLD {
            Self::flush_locked(&mut state);
        }
    }

    pub fn flush(&self) {
        let mut state = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::flush_locked(&mut state);
        // Logged once when writes start failing and once when they recover, so a disk that stays broken costs one line.
        if state.dropped > 0 && !state.reported {
            state.reported = true;
            tracing::warn!(file = %state.file.path.display(), "Trace writes are failing, dropping records until they recover");
        }
        if state.reported && !state.failing {
            tracing::info!(file = %state.file.path.display(), dropped = state.dropped, "Trace writes recovered");
            state.reported = false;
            state.dropped = 0;
        }
    }

    /// `flushUnsafe`: chunks no larger than the file's limit, a record larger than it skipped, and a failing disk dropping the rest of
    /// the batch (retrying it would grow the backlog).
    fn flush_locked(state: &mut Inner) {
        let records = std::mem::take(&mut state.buffer);
        let mut persisted = 0;
        while persisted < records.len() {
            let first = records[persisted].len() as u64;
            if first > state.file.max_bytes {
                persisted += 1;
                continue;
            }
            let (mut next, mut bytes) = (persisted + 1, first);
            while next < records.len() && bytes + records[next].len() as u64 <= state.file.max_bytes {
                bytes += records[next].len() as u64;
                next += 1;
            }
            if state.file.write(records[persisted..next].concat().as_bytes()).is_err() {
                state.failing = true;
                state.dropped += records.len() - persisted;
                return;
            }
            state.failing = false;
            persisted = next;
        }
    }
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<TraceSink>);

/// Installs the server's trace sink; a server that never installed one writes no trace.
pub fn install(store: &Store, sink: TraceSink) {
    let _ = store.extension(Slot::default).0.set(sink);
}
pub fn current(store: &Store) -> Option<TraceSink> {
    store.extension(Slot::default).0.get().cloned()
}

/// `observability.localTracingEnabled` of the server config: true once a sink writes the file.
pub fn overlay_config(store: &Store, config: &mut Value) {
    if current(store).is_some() {
        config["observability"]["localTracingEnabled"] = json!(true);
    }
    crate::otlp::overlay_config(store, config);
}

// ---- OTLP spans as trace records (`decodeOtlpTraceRecords`) --------------------------------------

const ATTRIBUTE_MAX_LENGTH: usize = 500;
const ATTRIBUTE_TRUNCATED_LENGTH: usize = 200;
const TRUNCATION_SUFFIX: &str = "…[truncated]";

/// `slice(0, length)` of a JavaScript string, which counts UTF-16 units; a cut through a surrogate pair keeps the replacement
/// character (JavaScript would keep a lone surrogate, which JSON text writes as an escape).
fn utf16_prefix(text: &str, length: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().take(length).collect();
    String::from_utf16_lossy(&units)
}
fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `truncateNestedValue`: strings inside an attribute are clamped, wherever they nest.
fn truncate_nested(value: &Value) -> Value {
    match value {
        Value::String(text) if utf16_length(text) > ATTRIBUTE_MAX_LENGTH => json!(format!("{}{TRUNCATION_SUFFIX}", utf16_prefix(text, ATTRIBUTE_MAX_LENGTH))),
        Value::Array(items) => Value::Array(items.iter().map(truncate_nested).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(key, value)| (key.clone(), truncate_nested(value))).collect()),
        other => other.clone(),
    }
}
/// `truncateTraceAttributes`: `db.query.text` is always cut short, the rest only when oversized.
fn truncate_attributes(attributes: Map<String, Value>) -> Map<String, Value> {
    attributes
        .into_iter()
        .map(|(key, value)| match &value {
            Value::String(text) if key == "db.query.text" => {
                let value = if utf16_length(text) <= ATTRIBUTE_TRUNCATED_LENGTH { value } else { json!(format!("{}{TRUNCATION_SUFFIX}", utf16_prefix(text, ATTRIBUTE_TRUNCATED_LENGTH))) };
                (key, value)
            }
            _ => {
                let truncated = truncate_nested(&value);
                (key, truncated)
            }
        })
        .collect()
}

/// A malformed payload, which the original's decoder meets as a thrown error and logs.
struct Malformed;
type Decoded<T> = Result<T, Malformed>;

fn array<'a>(value: &'a Value, key: &str) -> Decoded<&'a Vec<Value>> {
    value.get(key).and_then(Value::as_array).ok_or(Malformed)
}

/// `decodeValue`
fn decode_value(input: &Value) -> Value {
    let Some(map) = input.as_object() else { return Value::Null };
    for key in ["stringValue", "boolValue", "intValue", "doubleValue", "bytesValue"] {
        if let Some(value) = map.get(key) {
            return value.clone();
        }
    }
    if let Some(values) = map.get("arrayValue").and_then(|array| array.get("values")).and_then(Value::as_array) {
        return Value::Array(values.iter().map(decode_value).collect());
    }
    if let Some(values) = map.get("kvlistValue").and_then(|list| list.get("values")).and_then(Value::as_array) {
        return Value::Object(decode_attributes(values));
    }
    Value::Null
}
/// `decodeAttributes`: key/value pairs as an object, later keys replacing earlier ones.
fn decode_attributes(input: &[Value]) -> Map<String, Value> {
    let mut entries = Map::new();
    for attribute in input {
        if let Some(key) = attribute.get("key").and_then(Value::as_str) {
            entries.insert(key.to_owned(), decode_value(&attribute["value"]));
        }
    }
    truncate_attributes(entries)
}
fn attributes_of(value: &Value) -> Decoded<Map<String, Value>> {
    Ok(decode_attributes(array(value, "attributes")?))
}

/// `BigInt(input)`: a decimal string or a whole number; anything else is 0 (`parseBigInt`).
fn parse_nanos(value: &Value) -> i128 {
    match value {
        Value::String(text) => text.trim().parse().unwrap_or(0),
        Value::Number(number) => number.as_i64().map_or(0, i128::from),
        _ => 0,
    }
}
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { json!(value as i64) } else { json!(value) }
}

/// The named keys of `source` that it has, and the decoded `attributes`: a key JavaScript reads as `undefined` is left out of the text.
fn keyed(source: &Value, keys: [&str; 2], attributes: Map<String, Value>) -> Value {
    let mut record = Map::new();
    for key in keys {
        if let Some(value) = source.get(key) {
            record.insert(key.into(), value.clone());
        }
    }
    record.insert("attributes".into(), Value::Object(attributes));
    Value::Object(record)
}

fn span_record(resource_attributes: &Map<String, Value>, scope: &Value, scope_attributes: &Map<String, Value>, span: &Value) -> Decoded<Value> {
    let mut record = Map::new();
    record.insert("type".into(), json!("otlp-span"));
    for key in ["name", "traceId", "spanId"] {
        if let Some(value) = span.get(key) {
            record.insert(key.into(), value.clone());
        }
    }
    if let Some(parent) = span.get("parentSpanId").and_then(Value::as_str).filter(|parent| !parent.is_empty()) {
        record.insert("parentSpanId".into(), json!(parent));
    }
    record.insert("sampled".into(), json!(true));
    let kind = match span["kind"].as_u64() {
        Some(1) => "internal",
        Some(2) => "server",
        Some(3) => "client",
        Some(4) => "producer",
        Some(5) => "consumer",
        _ => "internal",
    };
    record.insert("kind".into(), json!(kind));
    for key in ["startTimeUnixNano", "endTimeUnixNano"] {
        if let Some(value) = span.get(key) {
            record.insert(key.into(), value.clone());
        }
    }
    record.insert("durationMs".into(), number((parse_nanos(&span["endTimeUnixNano"]) - parse_nanos(&span["startTimeUnixNano"])) as f64 / 1_000_000.0));
    record.insert("attributes".into(), Value::Object(attributes_of(span)?));
    record.insert("resourceAttributes".into(), Value::Object(resource_attributes.clone()));
    let mut scope_record = Map::new();
    if let Some(name) = scope.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()) {
        scope_record.insert("name".into(), json!(name));
    }
    if let Some(version) = scope.get("version").and_then(Value::as_str).filter(|version| !version.is_empty()) {
        scope_record.insert("version".into(), json!(version));
    }
    scope_record.insert("attributes".into(), Value::Object(scope_attributes.clone()));
    record.insert("scope".into(), Value::Object(scope_record));
    let events = array(span, "events")?
        .iter()
        .map(|event| Ok(keyed(event, ["name", "timeUnixNano"], attributes_of(event)?)))
        .collect::<Decoded<Vec<_>>>()?;
    record.insert("events".into(), Value::Array(events));
    let links = array(span, "links")?
        .iter()
        .map(|link| Ok(keyed(link, ["traceId", "spanId"], attributes_of(link)?)))
        .collect::<Decoded<Vec<_>>>()?;
    record.insert("links".into(), Value::Array(links));
    let status = span.get("status").filter(|status| status.is_object()).ok_or(Malformed)?;
    let mut decoded = Map::new();
    decoded.insert("code".into(), json!(match &status["code"] {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".to_owned(),
        other => other.to_string(),
    }));
    if let Some(message) = status.get("message").and_then(Value::as_str).filter(|message| !message.is_empty()) {
        decoded.insert("message".into(), json!(message));
    }
    record.insert("status".into(), Value::Object(decoded));
    Ok(Value::Object(record))
}

/// `decodeOtlpTraceRecords`: one record per span of a trace export. All or nothing: a payload the original's decoder throws on yields
/// no records.
pub fn decode_otlp_trace_records(payload: &Value) -> Result<Vec<Value>, String> {
    let mut records = vec![];
    let mut decode = || -> Decoded<()> {
        for resource_span in array(payload, "resourceSpans")? {
            let resource_attributes = match resource_span["resource"]["attributes"].as_array() {
                Some(attributes) => decode_attributes(attributes),
                None => Map::new(),
            };
            for scope_span in array(resource_span, "scopeSpans")? {
                let scope = &scope_span["scope"];
                let scope_attributes = scope["attributes"].as_array().map(|attributes| decode_attributes(attributes)).unwrap_or_default();
                for span in array(scope_span, "spans")? {
                    records.push(span_record(&resource_attributes, scope, &scope_attributes, span)?);
                }
            }
        }
        Ok(())
    };
    decode().map_err(|Malformed| "The export is not an OTLP trace payload.".to_owned())?;
    Ok(records)
}

// ---- the route -----------------------------------------------------------------------------------

pub fn routes() -> Router<ApiState> {
    Router::new().route("/api/observability/v1/traces", post(handle))
}

/// A browser's span export: recorded to the local trace file, forwarded to the OTLP traces endpoint when one is configured, and
/// answered 204 (502 when the collector refused it). A payload that does not decode is logged and dropped, as the original does.
async fn handle(State(state): State<ApiState>, headers: HeaderMap, body: Body) -> Response {
    let session = match authenticate(&state, &headers) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = require(&session, AuthEnvironmentScope::OrchestrationOperate) {
        return error.into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "The request body is not JSON.").into_response();
    };
    match decode_otlp_trace_records(&payload) {
        Ok(records) => {
            if let Some(sink) = current(&state.store) {
                for record in &records {
                    sink.push(record);
                }
            }
        }
        Err(detail) => tracing::warn!(%detail, "Failed to decode browser OTLP traces"),
    }
    let Some(endpoint) = crate::otlp::current(&state.store).and_then(|otlp| otlp.traces_endpoint) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match crate::otlp::forward(&endpoint, &payload).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(detail) => {
            tracing::warn!(%detail, otlp_traces_url = %endpoint.url, "Failed to export browser OTLP traces");
            (StatusCode::BAD_GATEWAY, "Trace export failed.").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the unchanged decoder made of exports of every shape (`tools/generate_trace_record_fixtures.mjs`).
    #[test]
    fn exports_decode_to_the_originals_records_and_the_malformed_ones_to_none() {
        let rows: Vec<Value> = include_str!("../tests/fixtures/trace-otlp-records.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(rows.len() >= 15);
        for row in rows {
            let decoded = decode_otlp_trace_records(&row["payload"]);
            match row.get("error") {
                Some(_) => assert!(decoded.is_err(), "{}", row["name"]),
                None => assert_eq!(json!(decoded.unwrap()), row["records"], "{}", row["name"]),
            }
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn records_are_appended_in_batches_and_the_file_rotates_by_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("logs").join("server.trace.ndjson");
        // Room for two of these records (each 18 bytes with its newline), not three.
        let sink = TraceSink::open(&path, 40, 2).unwrap();
        for index in 0..5 {
            sink.push(&json!({"n": index, "pad": "x"}));
        }
        // Nothing is written until the batch is flushed.
        assert!(!path.exists());
        sink.flush();
        // Five records, two to a file, two backups kept: the oldest fell off.
        let backup = |index: usize| lines(&PathBuf::from(format!("{}.{index}", path.display())));
        assert_eq!(lines(&path), vec![r#"{"n":4,"pad":"x"}"#]);
        assert_eq!(backup(1), vec![r#"{"n":2,"pad":"x"}"#, r#"{"n":3,"pad":"x"}"#]);
        assert_eq!(backup(2), vec![r#"{"n":0,"pad":"x"}"#, r#"{"n":1,"pad":"x"}"#]);
        assert!(!PathBuf::from(format!("{}.3", path.display())).exists());
        // A record larger than the file is skipped, not written across a rotation.
        sink.push(&json!({"pad": "y".repeat(100)}));
        sink.push(&json!({"n": 5, "pad": "x"}));
        sink.flush();
        assert!(lines(&path).iter().all(|line| !line.contains("yyy")));
        // A reopened sink carries on from the file's size and prunes backups past its limit.
        std::fs::write(format!("{}.7", path.display()), "stale\n").unwrap();
        let again = TraceSink::open(&path, 40, 2).unwrap();
        assert!(!PathBuf::from(format!("{}.7", path.display())).exists());
        again.push(&json!({"n": 6, "pad": "x"}));
        again.flush();
        assert_eq!(lines(&path).last().unwrap(), r#"{"n":6,"pad":"x"}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_writes_what_has_been_pushed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.trace.ndjson");
        let sink = TraceSink::open(&path, MAX_BYTES, MAX_FILES).unwrap();
        let writer = sink.start(BATCH_WINDOW);
        sink.push(&json!({"n": 1}));
        assert!(!path.exists());
        // The writer's timer is the earlier of the two, so the clock reaches it first and it has written when this wakes.
        tokio::time::sleep(BATCH_WINDOW + Duration::from_millis(1)).await;
        assert_eq!(lines(&path), vec![r#"{"n":1}"#]);
        writer.abort();
    }
}
