//! `server.getTraceDiagnostics` (port of `diagnostics/TraceDiagnostics.ts`): folds the server's
//! local trace file, with its rotated backups, into counts, the slowest spans, common failures and
//! the latest warnings. The files are streamed a line at a time, so memory stays bounded however
//! large the trace is. This server writes the spans browsers export (`trace_sink.rs`) and none of its own; a state
//! directory shared with the original server also reports that server's.
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, io::BufRead};

pub const METHODS: [&str; 1] = ["server.getTraceDiagnostics"];
const DEFAULT_SLOW_SPAN_THRESHOLD_MS: f64 = 1_000.0;
const TOP_LIMIT: usize = 10;
const RECENT_LIMIT: usize = 20;
/// The trace file's name under the logs directory, and how many rotated backups are scanned.
pub const TRACE_FILE_NAME: &str = "server.trace.ndjson";
pub const TRACE_MAX_FILES: usize = 10;

/// The trace file and its rotated backups, oldest first.
pub fn rotated_trace_paths(trace_file_path: &str, max_files: usize) -> Vec<String> {
    let mut paths: Vec<String> = (0..max_files).map(|index| format!("{trace_file_path}.{}", max_files - index)).collect();
    paths.push(trace_file_path.to_owned());
    paths
}

fn non_blank(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.trim().is_empty())
}
fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}
/// `unixNanoToDateTime`: a decimal string of nanoseconds, as an instant (milliseconds, truncated).
fn unix_nano(value: &Value) -> Option<i64> {
    let text = non_blank(value)?.trim();
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let millis = (text.parse::<i128>().ok()? / 1_000_000) as i64;
    DateTime::<Utc>::from_timestamp_millis(millis).map(|_| millis)
}
fn iso(millis: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(millis).map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)).unwrap_or_default()
}

/// `Option.some(value)` as Effect encodes it.
/// A JavaScript number: whole values print without a fraction.
fn num(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 1e15 { json!(value as i64) } else { json!(value) }
}

fn some(value: Value) -> Value {
    json!({"_tag":"Some","value":value})
}
fn none() -> Value {
    json!({"_tag":"None"})
}

struct SpanSummary {
    count: u64,
    failure_count: u64,
    total_duration_ms: f64,
    max_duration_ms: f64,
}
struct Occurrence {
    name: String,
    duration_ms: f64,
    ended_at: i64,
    trace_id: String,
    span_id: String,
}
struct Failure {
    occurrence: Occurrence,
    cause: String,
}
struct FailureSummary {
    name: String,
    cause: String,
    count: u64,
    last_seen_at: i64,
    trace_id: String,
    span_id: String,
}
struct LogEvent {
    span_name: String,
    level: String,
    message: String,
    seen_at: i64,
    trace_id: String,
    span_id: String,
}

/// Adds `item` to `items`, which stays sorted by `order` and holds at most `limit` entries. The
/// same result as a stable sort and slice over every item, with bounded memory.
fn insert_bounded<T>(items: &mut Vec<T>, item: T, limit: usize, order: impl Fn(&T, &T) -> std::cmp::Ordering) {
    if items.len() >= limit && order(&item, items.last().expect("full")) != std::cmp::Ordering::Less {
        return;
    }
    items.push(item);
    items.sort_by(&order);
    items.truncate(limit);
}

/// Folds trace NDJSON into diagnostics: `add_line` once per line as the files stream in, then `finish`.
pub struct Aggregator {
    slow_span_threshold_ms: f64,
    parse_error_count: u64,
    record_count: u64,
    failure_count: u64,
    interruption_count: u64,
    slow_span_count: u64,
    first_span_at: Option<i64>,
    last_span_at: Option<i64>,
    spans_by_name: Vec<(String, SpanSummary)>,
    span_index: HashMap<String, usize>,
    failures_by_key: Vec<FailureSummary>,
    failure_index: HashMap<(String, String), usize>,
    latest_failures: Vec<Failure>,
    slowest_spans: Vec<Occurrence>,
    latest_logs: Vec<LogEvent>,
    log_level_counts: Vec<(String, u64)>,
}

impl Aggregator {
    pub fn new(slow_span_threshold_ms: f64) -> Self {
        Self {
            slow_span_threshold_ms,
            parse_error_count: 0,
            record_count: 0,
            failure_count: 0,
            interruption_count: 0,
            slow_span_count: 0,
            first_span_at: None,
            last_span_at: None,
            spans_by_name: Vec::new(),
            span_index: HashMap::new(),
            failures_by_key: Vec::new(),
            failure_index: HashMap::new(),
            latest_failures: Vec::new(),
            slowest_spans: Vec::new(),
            latest_logs: Vec::new(),
            log_level_counts: Vec::new(),
        }
    }

    pub fn add_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else { return self.parse_error_count += 1 };
        // JSON `null` and every scalar are not records; an array is an object in JavaScript's eyes.
        if !(parsed.is_object() || parsed.is_array()) {
            return self.parse_error_count += 1;
        }
        let (name, trace_id, span_id) = (non_blank(&parsed["name"]), non_blank(&parsed["traceId"]), non_blank(&parsed["spanId"]));
        let (duration_ms, ended_at, started_at) = (finite(&parsed["durationMs"]), unix_nano(&parsed["endTimeUnixNano"]), unix_nano(&parsed["startTimeUnixNano"]));
        let (Some(name), Some(trace_id), Some(span_id), Some(duration_ms), Some(ended_at)) = (name, trace_id, span_id, duration_ms, ended_at) else {
            return self.parse_error_count += 1;
        };
        self.record_count += 1;
        if let Some(started) = started_at {
            if self.first_span_at.is_none_or(|first| started < first) {
                self.first_span_at = Some(started);
            }
        }
        if self.last_span_at.is_none_or(|last| ended_at > last) {
            self.last_span_at = Some(ended_at);
        }
        let exit_tag = parsed["exit"].get("_tag").and_then(non_blank);
        let (is_failure, is_interrupted) = (exit_tag == Some("Failure"), exit_tag == Some("Interrupted"));
        self.failure_count += u64::from(is_failure);
        self.interruption_count += u64::from(is_interrupted);
        let index = *self.span_index.entry(name.to_owned()).or_insert_with(|| {
            self.spans_by_name.push((name.to_owned(), SpanSummary { count: 0, failure_count: 0, total_duration_ms: 0.0, max_duration_ms: 0.0 }));
            self.spans_by_name.len() - 1
        });
        let summary = &mut self.spans_by_name[index].1;
        summary.count += 1;
        summary.total_duration_ms += duration_ms;
        summary.max_duration_ms = summary.max_duration_ms.max(duration_ms);
        summary.failure_count += u64::from(is_failure);
        let occurrence = || Occurrence { name: name.to_owned(), duration_ms, ended_at, trace_id: trace_id.to_owned(), span_id: span_id.to_owned() };
        if duration_ms >= self.slow_span_threshold_ms {
            self.slow_span_count += 1;
        }
        insert_bounded(&mut self.slowest_spans, occurrence(), TOP_LIMIT, |left, right| right.duration_ms.total_cmp(&left.duration_ms));
        if is_failure {
            let cause = parsed["exit"].get("cause").and_then(non_blank).map(|cause| cause.trim().to_owned()).unwrap_or_else(|| "Failure".into());
            insert_bounded(&mut self.latest_failures, Failure { occurrence: occurrence(), cause: cause.clone() }, RECENT_LIMIT, |left, right| right.occurrence.ended_at.cmp(&left.occurrence.ended_at));
            let key = (name.to_owned(), cause.clone());
            match self.failure_index.get(&key) {
                Some(&at) => {
                    let existing = &mut self.failures_by_key[at];
                    existing.count += 1;
                    if ended_at > existing.last_seen_at {
                        existing.last_seen_at = ended_at;
                        existing.trace_id = trace_id.to_owned();
                        existing.span_id = span_id.to_owned();
                    }
                }
                None => {
                    self.failure_index.insert(key, self.failures_by_key.len());
                    self.failures_by_key.push(FailureSummary { name: name.to_owned(), cause, count: 1, last_seen_at: ended_at, trace_id: trace_id.to_owned(), span_id: span_id.to_owned() });
                }
            }
        }
        let Some(events) = parsed["events"].as_array() else { return };
        for event in events.iter().filter(|event| event.is_object() || event.is_array()) {
            let Some(level) = event["attributes"].as_object().and_then(|attributes| attributes.get("effect.logLevel")).and_then(non_blank) else { continue };
            match self.log_level_counts.iter_mut().find(|(known, _)| known == level) {
                Some((_, count)) => *count += 1,
                None => self.log_level_counts.push((level.to_owned(), 1)),
            }
            if !matches!(level.to_lowercase().as_str(), "warning" | "warn" | "error" | "fatal") {
                continue;
            }
            let seen_at = unix_nano(&event["timeUnixNano"]).unwrap_or(ended_at);
            let message = non_blank(&event["name"]).map(|message| message.trim().to_owned()).unwrap_or_else(|| "Log event".into());
            insert_bounded(
                &mut self.latest_logs,
                LogEvent { span_name: name.to_owned(), level: level.to_owned(), message, seen_at, trace_id: trace_id.to_owned(), span_id: span_id.to_owned() },
                RECENT_LIMIT,
                |left, right| right.seen_at.cmp(&left.seen_at),
            );
        }
    }

    pub fn finish(self, trace_file_path: &str, scanned: &[String], read_at: i64, error: Option<(&str, String)>, partial_failure: bool) -> Value {
        let mut top: Vec<&(String, SpanSummary)> = self.spans_by_name.iter().collect();
        top.sort_by(|(_, left), (_, right)| right.count.cmp(&left.count).then(right.max_duration_ms.total_cmp(&left.max_duration_ms)));
        let top_spans: Vec<Value> = top
            .into_iter()
            .take(TOP_LIMIT)
            .map(|(name, span)| json!({"name":name,"count":span.count,"failureCount":span.failure_count,"totalDurationMs":num(span.total_duration_ms),"averageDurationMs":num(if span.count > 0 { span.total_duration_ms / span.count as f64 } else { 0.0 }),"maxDurationMs":num(span.max_duration_ms)}))
            .collect();
        let occurrence = |item: &Occurrence| json!({"name":item.name,"durationMs":num(item.duration_ms),"endedAt":iso(item.ended_at),"traceId":item.trace_id,"spanId":item.span_id});
        let mut common: Vec<&FailureSummary> = self.failures_by_key.iter().collect();
        common.sort_by(|left, right| right.count.cmp(&left.count).then(right.last_seen_at.cmp(&left.last_seen_at)));
        let mut counts = Map::new();
        for (level, count) in &self.log_level_counts {
            counts.insert(level.clone(), json!(count));
        }
        json!({
            "traceFilePath": trace_file_path,
            "scannedFilePaths": scanned,
            "readAt": iso(read_at),
            "recordCount": self.record_count,
            "parseErrorCount": self.parse_error_count,
            "firstSpanAt": self.first_span_at.map_or_else(none, |at| some(json!(iso(at)))),
            "lastSpanAt": self.last_span_at.map_or_else(none, |at| some(json!(iso(at)))),
            "failureCount": self.failure_count,
            "interruptionCount": self.interruption_count,
            "slowSpanThresholdMs": num(self.slow_span_threshold_ms),
            "slowSpanCount": self.slow_span_count,
            "logLevelCounts": counts,
            "topSpansByCount": top_spans,
            "slowestSpans": self.slowest_spans.iter().map(occurrence).collect::<Vec<_>>(),
            "commonFailures": common.into_iter().take(TOP_LIMIT).map(|failure| json!({"name":failure.name,"cause":failure.cause,"count":failure.count,"lastSeenAt":iso(failure.last_seen_at),"traceId":failure.trace_id,"spanId":failure.span_id})).collect::<Vec<_>>(),
            "latestFailures": self.latest_failures.iter().map(|failure| { let mut value = occurrence(&failure.occurrence); value["cause"] = json!(failure.cause); value }).collect::<Vec<_>>(),
            "latestWarningAndErrorLogs": self.latest_logs.iter().map(|log| json!({"spanName":log.span_name,"level":log.level,"message":log.message,"seenAt":iso(log.seen_at),"traceId":log.trace_id,"spanId":log.span_id})).collect::<Vec<_>>(),
            "partialFailure": if partial_failure { some(json!(true)) } else { none() },
            "error": error.map_or_else(none, |(kind, message)| some(json!({"kind":kind,"message":message}))),
        })
    }
}

/// Feeds each line of one trace file to `on_line`. `Ok(false)` when the file does not exist.
fn stream_lines(path: &str, mut on_line: impl FnMut(&str)) -> Result<bool, std::io::Error> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // Lines end with \n, \r\n or a lone \r; invalid UTF-8 becomes replacement characters.
    let mut reader = std::io::BufReader::new(file);
    let mut pending: Vec<u8> = Vec::new();
    let mut after_carriage_return = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let length = chunk.len();
        for &byte in chunk {
            match byte {
                b'\n' if after_carriage_return => after_carriage_return = false,
                b'\n' | b'\r' => {
                    after_carriage_return = byte == b'\r';
                    on_line(&String::from_utf8_lossy(&pending));
                    pending.clear();
                }
                other => {
                    after_carriage_return = false;
                    pending.push(other);
                }
            }
        }
        reader.consume(length);
    }
    if !pending.is_empty() {
        on_line(&String::from_utf8_lossy(&pending));
    }
    Ok(true)
}

/// `TraceDiagnostics.read`.
pub fn read(trace_file_path: &str, max_files: usize, read_at: i64) -> Value {
    let paths = rotated_trace_paths(trace_file_path, max_files);
    let mut aggregator = Aggregator::new(DEFAULT_SLOW_SPAN_THRESHOLD_MS);
    let mut found = false;
    let mut first_failure: Option<String> = None;
    // Every file feeds one aggregator, so they are read one at a time, oldest first.
    for path in &paths {
        match stream_lines(path, |line| aggregator.add_line(line)) {
            Ok(exists) => found |= exists,
            Err(error) => {
                tracing::warn!(trace_file_path = %path, error = %error, "Failed to read local trace file.");
                first_failure.get_or_insert_with(|| format!("Failed to read local trace file '{path}'."));
            }
        }
    }
    let failure = first_failure.map(|message| ("trace-file-read-failed", message));
    if !found {
        let error = failure.unwrap_or(("trace-file-not-found", "No local trace files were found.".into()));
        return Aggregator::new(DEFAULT_SLOW_SPAN_THRESHOLD_MS).finish(trace_file_path, &paths, read_at, Some(error), false);
    }
    let partial = failure.is_some();
    aggregator.finish(trace_file_path, &paths, read_at, failure, partial)
}

/// The request: the payload is any non-null value (an empty struct), the answer the diagnostics of
/// the logs directory's trace.
pub async fn command(store: &crate::persistence::Store, tag: &str, payload: Value) -> Result<Value, Value> {
    if tag != "server.getTraceDiagnostics" {
        return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag}));
    }
    if payload.is_null() {
        return Err(json!({"_tag":"SchemaDecodeError","message":"Expected a non-null value."}));
    }
    let logs = store.state_dir().map(|state| state.join("logs")).ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"Trace diagnostics need a state directory."}))?;
    let path = logs.join(TRACE_FILE_NAME).to_string_lossy().into_owned();
    let now = Utc::now().timestamp_millis();
    tokio::task::spawn_blocking(move || read(&path, TRACE_MAX_FILES, now)).await.map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
}
