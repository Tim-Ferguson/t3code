//! Best-effort provider event logging with one shared writer per thread (`provider/EventNdjsonLogger.ts`,
//! `provider/ProviderEventLoggers.ts`).
//!
//! The native and canonical views of a store share batching, rotation and retention state, so they cannot race while appending to the
//! same thread-scoped file `<logs>/provider/events.<thread>.log`. A line is `[<iso time>] NTIVE|CANON|ORCH: <json>`. Adapters write the
//! native view through [`native`]; a server whose log could not be opened writes nothing, as the original downgrades to no loggers.
use crate::{persistence::Store, trace_sink::RotatingFile};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

const MEBIBYTE: u64 = 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_RECORD_CHARACTERS: i64 = 64 * 1024;
const MAX_RECORD_FIELDS: i64 = 1024;
const MAX_RECORD_DEPTH: usize = 16;
const GLOBAL_THREAD_SEGMENT: &str = "_global";

const TRANSIENT_CANONICAL_TYPES: [&str; 7] = ["content.delta", "hook.progress", "item.updated", "task.progress", "thread.realtime.audio.delta", "tool.progress", "turn.proposed.delta"];
const TRANSIENT_NATIVE_METHODS: [&str; 9] = [
    "item/agentMessage/delta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "thread/realtime/outputAudio/delta",
    "thread/realtime/transcript/delta",
    "turn/diff/updated",
];
const TRANSIENT_ACP_UPDATES: [&str; 2] = ["agent_message_chunk", "agent_thought_chunk"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stream {
    Native,
    Canonical,
    Orchestration,
}
impl Stream {
    fn label(self) -> &'static str {
        match self {
            Stream::Native => "NTIVE",
            Stream::Orchestration => "ORCH",
            Stream::Canonical => "CANON",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Stream::Native => "native",
            Stream::Canonical => "canonical",
            Stream::Orchestration => "orchestration",
        }
    }
}

/// `.length` of a JavaScript string.
fn utf16_len(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

fn object(value: &Value) -> Option<&Value> {
    (value.is_object() || value.is_array()).then_some(value)
}

/// `shouldPersistProviderEvent`: token deltas and repeated growing snapshots are not worth a line.
pub fn should_persist(stream: Stream, event: &Value) -> bool {
    if stream == Stream::Orchestration || object(event).is_none() {
        return true;
    }
    if event["type"].as_str().is_some_and(|kind| TRANSIENT_CANONICAL_TYPES.contains(&kind)) {
        return false;
    }
    if stream != Stream::Native {
        return true;
    }
    let envelope = object(&event["event"]).unwrap_or(event);
    // Decoded frames carry the same information as raw frames without another copy of every token delta.
    if envelope["stage"] == "raw" {
        return false;
    }
    let decoded = &envelope["payload"];
    let native_event = if envelope["stage"] == "decoded" && object(decoded).is_some() { decoded } else { envelope };
    let method = native_event["method"].as_str();
    if method.is_some_and(|method| TRANSIENT_NATIVE_METHODS.contains(&method) || method.starts_with("claude/stream_event/content_block_delta/")) {
        return false;
    }
    let native_type = &native_event["type"];
    if native_type == "message.part.delta" {
        return false;
    }
    if native_type == "stream_event" && object(&native_event["event"]).is_some_and(|inner| inner["type"] == "content_block_delta") {
        return false;
    }
    let payload = &native_event["payload"];
    if object(payload).is_none() {
        return true;
    }
    if method == Some("session/update") {
        let update = &payload["update"];
        if object(update).is_none() {
            return true;
        }
        return !update["sessionUpdate"].as_str().is_some_and(|kind| TRANSIENT_ACP_UPDATES.contains(&kind));
    }
    if native_type == "message.part.updated" {
        let part = &payload["properties"]["part"];
        if object(&payload["properties"]).is_none() || object(part).is_none() {
            return true;
        }
        match part["type"].as_str() {
            Some("text" | "reasoning") => return false,
            // Running snapshots repeat growing output. Pending and terminal states stay in the log.
            Some("tool") if object(&part["state"]).is_some() => return part["state"]["status"] != "running",
            _ => {}
        }
    }
    true
}

const SUMMARY_FIELDS: [&str; 33] = [
    "provider", "protocol", "kind", "providerSessionId", "direction", "stage", "type", "subtype", "method", "id", "threadId", "turnId", "requestId", "session_id", "status", "is_error", "api_error_status", "terminal_reason", "stop_reason", "operation", "code", "willRetry", "message", "event", "payload", "params", "result", "thread", "turn", "error", "turns", "items", "content",
];

/// `summarizeProviderEvent`: routing and error fields only, bounded.
fn summarize(event: &Value) -> Value {
    fn walk(value: &Value, depth: usize, fields: &mut i64, characters: &mut i64) -> Value {
        match value {
            Value::String(text) => {
                let length = utf16_len(text);
                if length > 1024.min(*characters) {
                    return json!({"omittedCharacters":length});
                }
                *characters -= length;
                value.clone()
            }
            Value::Null | Value::Number(_) | Value::Bool(_) => value.clone(),
            Value::Array(items) => json!({"itemCount":items.len()}),
            Value::Object(_) => {
                let mut summary = Map::new();
                summary.insert("truncated".into(), json!(true));
                if depth >= 6 {
                    return Value::Object(summary);
                }
                for key in SUMMARY_FIELDS {
                    if *fields <= 0 {
                        break;
                    }
                    let Some(nested) = value.get(key) else { continue };
                    *fields -= 1;
                    summary.insert(key.into(), walk(nested, depth + 1, fields, characters));
                }
                Value::Object(summary)
            }
        }
    }
    walk(event, 0, &mut 128, &mut (8 * 1024))
}

/// `boundProviderEventForLogging`: bounds traversal before an adapter copies a payload or the logger encodes it.
pub fn bound(event: &Value) -> Value {
    fn fits(value: &Value, depth: usize, characters: &mut i64, fields: &mut i64) -> bool {
        match value {
            Value::String(text) => {
                *characters -= utf16_len(text);
                *characters >= 0
            }
            Value::Array(items) => {
                if depth > MAX_RECORD_DEPTH || items.len() as i64 > *fields {
                    return false;
                }
                for (index, item) in items.iter().enumerate() {
                    *fields -= 1;
                    *characters -= index.to_string().len() as i64;
                    if *fields < 0 || *characters < 0 || !fits(item, depth + 1, characters, fields) {
                        return false;
                    }
                }
                true
            }
            Value::Object(map) => {
                if depth > MAX_RECORD_DEPTH {
                    return false;
                }
                for (key, item) in map {
                    *fields -= 1;
                    *characters -= utf16_len(key);
                    if *fields < 0 || *characters < 0 || !fits(item, depth + 1, characters, fields) {
                        return false;
                    }
                }
                true
            }
            _ => true,
        }
    }
    if fits(event, 0, &mut MAX_RECORD_CHARACTERS.clone(), &mut MAX_RECORD_FIELDS.clone()) { event.clone() } else { summarize(event) }
}

/// The text of the record, or `None` when it cannot be written. Escaping can expand strings beyond their input size, so an encoding
/// over the bound is replaced by the summary, which keeps routing and error fields.
pub fn encode(event: &Value) -> String {
    let payload = bound(event).to_string();
    if payload.len() as i64 > MAX_RECORD_CHARACTERS { summarize(event).to_string() } else { payload }
}

// ---- the store -------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub max_bytes: i64,
    pub max_files: i64,
    pub batch_window_ms: i64,
    pub max_total_bytes: i64,
    pub max_age_ms: i64,
    pub retention_check_interval_ms: i64,
    pub max_buffered_bytes: i64,
    pub max_buffered_records: i64,
}
impl Default for Options {
    fn default() -> Self {
        Self { max_bytes: 10 * MEBIBYTE as i64, max_files: 10, batch_window_ms: 1000, max_total_bytes: 512 * MEBIBYTE as i64, max_age_ms: 14 * DAY_MS, retention_check_interval_ms: 5 * 60 * 1000, max_buffered_bytes: MEBIBYTE as i64, max_buffered_records: 512 }
    }
}

/// `EventNdjsonLogConfigurationError` / `EventNdjsonLogDirectoryError`.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum OpenError {
    #[error("Provider event log option '{option}' must be an integer >= {minimum}; received {value} for '{path}'")]
    Option { path: String, option: &'static str, value: i64, minimum: i64 },
    #[error("Failed to create provider event log directory '{0}'")]
    Directory(String),
}

fn validate(path: &Path, options: &Options) -> Result<(), OpenError> {
    for (option, value, minimum) in [
        ("maxBytes", options.max_bytes, 1),
        ("maxFiles", options.max_files, 1),
        ("batchWindowMs", options.batch_window_ms, 0),
        ("maxTotalBytes", options.max_total_bytes, 1),
        ("maxAgeMs", options.max_age_ms, 1),
        ("retentionCheckIntervalMs", options.retention_check_interval_ms, 1),
        ("maxBufferedBytes", options.max_buffered_bytes, 1),
        ("maxBufferedRecords", options.max_buffered_records, 1),
    ] {
        if value < minimum {
            return Err(OpenError::Option { path: path.to_string_lossy().into_owned(), option, value, minimum });
        }
    }
    Ok(())
}

struct Pending {
    stream: Stream,
    segment: String,
    line: String,
}

/// What a flush wrote, for the resource accounting: per stream, the records and logical bytes.
pub type Attribution = Arc<dyn Fn(Stream, usize, u64, u64) + Send + Sync>;

struct State {
    pending: Vec<Pending>,
    pending_bytes: u64,
    sinks: HashMap<String, RotatingFile>,
    flush_scheduled: bool,
    closed: bool,
    last_retention_at: i64,
}

struct Shared {
    directory: PathBuf,
    prefix: String,
    options: Options,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    attribution: Option<Attribution>,
    state: Mutex<State>,
}

/// `<basename without extension>.`
fn log_prefix(path: &Path) -> String {
    let name = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{name}.")
}

fn iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}

fn is_provider_log(path: &Path, name: &str, prefix: &str) -> std::io::Result<bool> {
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    static HEADER: OnceLock<Regex> = OnceLock::new();
    if !SUFFIX.get_or_init(|| Regex::new(r"\.log(?:\.\d+)?$").unwrap()).is_match(name) {
        return Ok(false);
    }
    if name.starts_with(prefix) {
        return Ok(true);
    }
    let mut header = [0u8; 256];
    let read = std::fs::File::open(path)?.read(&mut header)?;
    Ok(HEADER.get_or_init(|| Regex::new(r"^\[[^\]\r\n]+\] (?:NTIVE|CANON|ORCH): ").unwrap()).is_match(&String::from_utf8_lossy(&header[..read])))
}

/// `enforceRetention`: files past the maximum age go, then the oldest until the total fits. A file a sink is writing is never removed.
/// Returns the paths that failed, for the log.
fn enforce_retention(directory: &Path, options: &Options, active: &HashSet<PathBuf>, prefix: &str, now: i64) -> Vec<PathBuf> {
    let mut failures = vec![];
    let Ok(entries) = std::fs::read_dir(directory) else { return vec![directory.to_path_buf()] };
    let mut files = vec![];
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        match is_provider_log(&path, &name, prefix).and_then(|is| if is { std::fs::metadata(&path).map(Some) } else { Ok(None) }) {
            Ok(Some(metadata)) => files.push((path, crate::usage_transcript_reader::mtime_ms(&metadata), metadata.len())),
            Ok(None) => {}
            Err(_) => failures.push(path),
        }
    }
    let total = std::cell::Cell::new(files.iter().map(|file| file.2).sum::<u64>());
    let remove = |file: &(PathBuf, f64, u64), failures: &mut Vec<PathBuf>| -> bool {
        if active.contains(&file.0) {
            return false;
        }
        match std::fs::remove_file(&file.0) {
            Ok(()) => {
                total.set(total.get().saturating_sub(file.2));
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => {
                failures.push(file.0.clone());
                false
            }
        }
    };
    let mut retained = vec![];
    for file in files {
        if now as f64 - file.1 <= options.max_age_ms as f64 || !remove(&file, &mut failures) {
            retained.push(file);
        }
    }
    retained.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
    for file in &retained {
        if total.get() <= options.max_total_bytes as u64 {
            break;
        }
        remove(file, &mut failures);
    }
    failures
}

/// `writeBatchedMessages`: records go to the sink in chunks of at most `max_bytes`.
fn write_batched(sink: &mut RotatingFile, records: &[&Pending], max_bytes: u64, mut written: impl FnMut(&[&Pending])) -> std::io::Result<()> {
    let (mut batch, mut bytes): (Vec<&Pending>, u64) = (vec![], 0);
    let mut flush = |batch: &mut Vec<&Pending>, bytes: &mut u64, sink: &mut RotatingFile| -> std::io::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        sink.write(batch.iter().map(|record| record.line.as_str()).collect::<String>().as_bytes())?;
        written(batch);
        batch.clear();
        *bytes = 0;
        Ok(())
    };
    for record in records {
        let size = record.line.len() as u64;
        if *&bytes > 0 && bytes + size > max_bytes {
            flush(&mut batch, &mut bytes, sink)?;
        }
        batch.push(record);
        bytes += size;
        if bytes >= max_bytes {
            flush(&mut batch, &mut bytes, sink)?;
        }
    }
    flush(&mut batch, &mut bytes, sink)
}

impl Shared {
    /// `drainPending`: writes what is buffered, one file per thread, and runs retention when it is due.
    fn drain(&self, timer_fired: bool, close: bool) {
        let started = (self.clock)();
        let mut summary: BTreeMap<&'static str, (Stream, usize, u64)> = BTreeMap::new();
        let mut failures: Vec<PathBuf> = vec![];
        {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.closed {
                return;
            }
            let pending = std::mem::take(&mut state.pending);
            state.pending_bytes = 0;
            let mut by_segment: Vec<(String, Vec<&Pending>)> = vec![];
            for record in &pending {
                match by_segment.iter_mut().find(|(segment, _)| *segment == record.segment) {
                    Some((_, records)) => records.push(record),
                    None => by_segment.push((record.segment.clone(), vec![record])),
                }
            }
            for (segment, records) in by_segment {
                let path = self.directory.join(format!("{}{segment}.log", self.prefix));
                if !state.sinks.contains_key(&segment) {
                    match RotatingFile::open(path.clone(), self.options.max_bytes as u64, self.options.max_files as usize) {
                        Ok(sink) => {
                            state.sinks.insert(segment.clone(), sink);
                        }
                        Err(_) => {
                            failures.push(path);
                            continue;
                        }
                    }
                }
                let sink = state.sinks.get_mut(&segment).expect("the sink was just opened");
                let outcome = write_batched(sink, &records, self.options.max_bytes as u64, |written| {
                    for record in written {
                        let entry = summary.entry(record.stream.name()).or_insert((record.stream, 0, 0));
                        entry.1 += 1;
                        entry.2 += record.line.len() as u64;
                    }
                });
                if outcome.is_err() {
                    state.sinks.remove(&segment);
                    failures.push(path);
                }
            }
            if timer_fired {
                state.flush_scheduled = false;
            }
            if started - state.last_retention_at >= self.options.retention_check_interval_ms {
                let active: HashSet<PathBuf> = state.sinks.keys().map(|segment| self.directory.join(format!("{}{segment}.log", self.prefix))).collect();
                failures.extend(enforce_retention(&self.directory, &self.options, &active, &self.prefix, started));
                state.last_retention_at = started;
            }
            state.closed = close;
        }
        for path in failures {
            tracing::warn!(path = %path.display(), scope = "provider-observability", "provider event log write or retention failed");
        }
        if let Some(attribution) = &self.attribution {
            let finished = (self.clock)();
            let duration = (finished - started).max(0) as u64;
            let total: u64 = summary.values().map(|entry| entry.2).sum();
            for (stream, count, bytes) in summary.into_values() {
                let share = if total == 0 { 0 } else { (duration as f64 * (bytes as f64 / total as f64)).round() as u64 };
                attribution(stream, count, bytes, share);
            }
        }
    }
}

/// The store the loggers of one server share.
#[derive(Clone)]
pub struct EventLogStore(Arc<Shared>);

impl EventLogStore {
    /// `makeEventNdjsonLogStore`: `path` is `<logs>/provider/events.log`, whose directory holds the per-thread files. Retention runs once
    /// now, against every file there.
    pub fn open(path: &Path, options: Options, clock: Arc<dyn Fn() -> i64 + Send + Sync>, attribution: Option<Attribution>) -> Result<Self, OpenError> {
        validate(path, &options)?;
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
        std::fs::create_dir_all(&directory).map_err(|_| OpenError::Directory(directory.to_string_lossy().into_owned()))?;
        let prefix = log_prefix(path);
        let now = clock();
        for failure in enforce_retention(&directory, &options, &HashSet::new(), &prefix, now) {
            tracing::warn!(path = %failure.display(), scope = "provider-observability", "provider event log retention failed");
        }
        Ok(Self(Arc::new(Shared { directory, prefix, options, clock, attribution, state: Mutex::new(State { pending: vec![], pending_bytes: 0, sinks: HashMap::new(), flush_scheduled: false, closed: false, last_retention_at: now }) })))
    }

    pub fn logger(&self, stream: Stream) -> EventLogger {
        EventLogger { store: self.0.clone(), stream }
    }

    /// Writes what is buffered and stops: later writes are dropped.
    pub fn close(&self) {
        self.0.drain(false, true);
    }
}

/// One stream's view of a store.
#[derive(Clone)]
pub struct EventLogger {
    store: Arc<Shared>,
    stream: Stream,
}

impl EventLogger {
    /// `write`: a record for `thread_id` (the shared `_global` file when there is none). Never fails and never blocks on the disk longer
    /// than a flush: the batch window collects records, and a window of 0 writes each at once.
    pub fn write(&self, event: &Value, thread_id: Option<&str>) {
        if !should_persist(self.stream, event) {
            return;
        }
        let payload = encode(event);
        let line = format!("[{}] {}: {payload}\n", iso((self.store.clock)()), self.stream.label());
        let segment = thread_id.and_then(crate::attachment_store::safe_thread_segment).unwrap_or_else(|| GLOBAL_THREAD_SEGMENT.to_owned());
        let bytes = line.len() as u64;
        let (flush_now, schedule) = {
            let mut state = self.store.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.closed {
                return;
            }
            state.pending.push(Pending { stream: self.stream, segment, line });
            state.pending_bytes += bytes;
            let flush = self.store.options.batch_window_ms == 0 || state.pending.len() as i64 >= self.store.options.max_buffered_records || state.pending_bytes as i64 >= self.store.options.max_buffered_bytes;
            let schedule = !flush && !state.flush_scheduled;
            state.flush_scheduled |= schedule;
            (flush, schedule)
        };
        if schedule {
            let store = self.store.clone();
            let window = Duration::from_millis(store.options.batch_window_ms as u64);
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        tokio::time::sleep(window).await;
                        store.drain(true, false);
                    });
                }
                // No runtime to time the window: write now rather than hold the record.
                Err(_) => self.store.drain(true, false),
            }
        }
        if flush_now {
            self.store.drain(false, false);
        }
    }
}

// ---- the server's loggers --------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Slot(OnceLock<EventLogStore>);

/// Installs the server's store; a server that never installed one writes no provider event log.
pub fn install(store: &Store, log: EventLogStore) {
    let _ = store.extension(Slot::default).0.set(log);
}

/// The native view (`ProviderEventLoggers.native`), or `None` when the log could not be opened.
pub fn native(store: &Store) -> Option<EventLogger> {
    store.extension(Slot::default).0.get().map(|log| log.logger(Stream::Native))
}

/// The canonical view (`ProviderEventLoggers.canonical`).
pub fn canonical(store: &Store) -> Option<EventLogger> {
    store.extension(Slot::default).0.get().map(|log| log.logger(Stream::Canonical))
}

/// The store, to close it at shutdown.
pub fn current(store: &Store) -> Option<EventLogStore> {
    store.extension(Slot::default).0.get().cloned()
}

/// The tap an adapter hands its protocol connection (`makeClaudeAgentSdkProtocolLogger` and its kin): each decoded frame becomes
/// `{provider, protocol, kind: "protocol", providerSessionId, event: {direction, stage: "decoded", payload}}` in the thread's file.
/// `None` when the server keeps no log.
pub fn protocol_tap(store: &Store, provider: &'static str, protocol: &'static str, thread_id: &str, session_id: &str) -> Option<Arc<dyn Fn(&'static str, &Value) + Send + Sync>> {
    let logger = native(store)?;
    let (thread_id, session_id) = (thread_id.to_owned(), session_id.to_owned());
    Some(Arc::new(move |direction, payload| {
        let event = json!({"direction":direction,"stage":"decoded","payload":payload});
        if !should_persist(Stream::Native, &event) {
            return;
        }
        logger.write(&json!({"provider":provider,"protocol":protocol,"kind":"protocol","providerSessionId":session_id,"event":bound(&event)}), Some(&thread_id));
    }))
}

// ---- native protocol summaries (`provider/NativeProtocolLogging.ts`) --------------------------------------------------------------

/// `structuralProtocolMethod`: a method name that is plain enough to log, else `unknown`.
pub fn structural_protocol_method(value: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    if utf16_len(value) <= 128 && PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9._:/-]*$").unwrap()).is_match(value) { value.to_owned() } else { "unknown".to_owned() }
}

/// `summarizeNativeProtocolPayload`: bounded structural metadata only. Provider payload values may contain prompts, credentials or
/// arbitrarily large tool output and are never copied into the log.
pub fn summarize_native_protocol_payload(payload: &Value) -> Value {
    match payload {
        Value::Null => json!({"valueType":"null"}),
        Value::String(text) => json!({"valueType":"string","byteLength":text.len()}),
        Value::Array(items) => json!({"valueType":"array","itemCount":items.len()}),
        Value::Bool(_) => json!({"valueType":"boolean"}),
        Value::Number(_) => json!({"valueType":"number"}),
        Value::Object(record) => {
            let mut summary = Map::new();
            summary.insert("valueType".into(), json!("object"));
            summary.insert("fieldCount".into(), json!(record.len()));
            if let Some(tag) = record.get("_tag").and_then(Value::as_str) {
                summary.insert("messageTag".into(), json!(structural_tag(tag)));
            }
            if let Some(method) = record.get("tag").and_then(Value::as_str) {
                summary.insert("method".into(), json!(structural_protocol_method(method)));
            }
            Value::Object(summary)
        }
    }
}

/// `isStructuralTag`: a tag short and plain enough to keep, else `TaggedError`.
fn structural_tag(tag: &str) -> &str {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    if utf16_len(tag) <= 128 && PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9._:/-]*$").unwrap()).is_match(tag) { tag } else { "TaggedError" }
}

/// What an adapter that logs a summary of each message writes (`makeOpenCodeProtocolLogger`, `makeAcpNativeLoggerFactory`): one record
/// with an id, the provider and session it came from, and the summary of the payload.
#[allow(clippy::too_many_arguments)]
pub fn write_protocol_summary(logger: &EventLogger, provider: &str, protocol: Option<&str>, instance_id: &str, session_id: &str, thread_id: &str, direction: &str, message_kind: &str, method: &str, payload: &Value) {
    let observed_at = iso((logger.store.clock)());
    let mut event = Map::new();
    event.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
    event.insert("kind".into(), json!("protocol"));
    if let Some(protocol) = protocol {
        event.insert("protocol".into(), json!(protocol));
    }
    event.insert("provider".into(), json!(provider));
    event.insert("providerInstanceId".into(), json!(instance_id));
    event.insert("providerSessionId".into(), json!(session_id));
    event.insert("createdAt".into(), json!(observed_at));
    event.insert("threadId".into(), json!(thread_id));
    event.insert("payload".into(), json!({"direction":direction,"messageKind":message_kind,"method":structural_protocol_method(method),"payload":summarize_native_protocol_payload(payload)}));
    logger.write(&json!({"observedAt":observed_at,"event":Value::Object(event)}), Some(thread_id));
}

/// What the ACP family writes (`writeNativeAcpLog`): the record `kind` is `request` or `protocol`, and the payload is already a bounded
/// summary. Unlike [`write_protocol_summary`] it names neither a session nor a protocol.
pub fn write_acp_record(logger: &EventLogger, provider: &str, thread_id: &str, kind: &str, payload: Value) {
    let observed_at = iso((logger.store.clock)());
    let event = json!({"id":uuid::Uuid::new_v4().to_string(),"kind":kind,"provider":provider,"createdAt":observed_at,"threadId":thread_id,"payload":payload});
    logger.write(&json!({"observedAt":observed_at,"event":event}), Some(thread_id));
}

// ---- Codex ------------------------------------------------------------------------------------------------------------------------

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    ["authorization", "apikey", "token", "password", "secret"].iter().any(|suffix| normalized.ends_with(suffix))
}

/// `redactCodexProtocolValue`: values under a key that names a credential, and bearer tokens, are replaced; a string that is itself a JSON
/// object or array is bounded and redacted inside.
pub fn redact_codex_protocol_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact_codex_protocol_value).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(key, nested)| (key.clone(), if is_sensitive_key(key) { json!("[REDACTED]") } else { redact_codex_protocol_value(nested) })).collect()),
        Value::String(text) => {
            static BEARER: OnceLock<Regex> = OnceLock::new();
            if BEARER.get_or_init(|| Regex::new(r"(?i)^Bearer\s+").unwrap()).is_match(text) {
                return json!("[REDACTED]");
            }
            let trimmed = text.trim();
            if !((trimmed.starts_with('{') && trimmed.ends_with('}')) || (trimmed.starts_with('[') && trimmed.ends_with(']'))) {
                return value.clone();
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) => json!(redact_codex_protocol_value(&bound(&parsed)).to_string()),
                Err(_) => value.clone(),
            }
        }
        other => other.clone(),
    }
}

/// `makeCodexAppServerProtocolLogger`'s writer, for the Codex runtime to hand its app-server connection: as [`protocol_tap`], with the
/// credentials redacted. The runtime (`codex_runtime.rs`) calls it for each frame it reads and writes.
pub fn codex_protocol_tap(store: &Store, thread_id: &str, session_id: &str) -> Option<Arc<dyn Fn(&'static str, &Value) + Send + Sync>> {
    let logger = native(store)?;
    let (thread_id, session_id) = (thread_id.to_owned(), session_id.to_owned());
    Some(Arc::new(move |direction, payload| {
        let event = json!({"direction":direction,"stage":"decoded","payload":payload});
        if !should_persist(Stream::Native, &event) {
            return;
        }
        logger.write(&json!({"provider":"codex","protocol":"codex.app-server","kind":"protocol","providerSessionId":session_id,"event":redact_codex_protocol_value(&bound(&event))}), Some(&thread_id));
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_frames_lose_credentials_wherever_they_nest() {
        let frame = json!({"method":"account/login","params":{"apiKey":"sk-1","auth_token":"t","Authorization":"Bearer abc","nested":[{"client-secret":"s","ok":1}],"header":"bearer   xyz","body":"{\"password\":\"p\",\"name\":\"n\"}","list":"[{\"token\":\"t\"}]","brace":"{not json}","plain":"text"}});
        assert_eq!(
            redact_codex_protocol_value(&frame),
            json!({"method":"account/login","params":{"apiKey":"[REDACTED]","auth_token":"[REDACTED]","Authorization":"[REDACTED]","nested":[{"client-secret":"[REDACTED]","ok":1}],"header":"[REDACTED]","body":"{\"password\":\"[REDACTED]\",\"name\":\"n\"}","list":"[{\"token\":\"[REDACTED]\"}]","brace":"{not json}","plain":"text"}})
        );
    }
}
