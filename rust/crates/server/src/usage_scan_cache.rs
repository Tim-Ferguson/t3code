//! The durable per-file scan cache (`usageScanCache.ts`).
//!
//! Transcripts are append-only and a file that has not changed can never yield different usage, so parsed records are keyed by
//! `(size, mtime)` and reused; they also outlive the transcript, so usage survives cleanup until the 90-day retention ends. The cache
//! is per file rather than per day (independent of the reporting zone, and cross-file de-duplication stays exact: entries are
//! de-duplicated within their own file only). The document is the original's, byte for byte, in the same file name, so a state
//! directory the original also reads keeps one cache.
use crate::{
    usage_transcript_reader::{GUARD_LENGTH, ParsePosition},
    usage_transcripts::{CodexScanState, Speed, Totals, UsageRecord},
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::{Arc, OnceLock}};

/// v2: Codex fork-copy suppression changed what a file parses to. v3: entries carry the parse position and reducer state. v4: records
/// carry Claude fast mode. v5: Codex records carry their service tier (v4 rows store speed the same way, so v4 entries still load).
const VERSION: u64 = 5;
const SPEED_COMPATIBLE_SINCE_VERSION: u64 = 4;

/// Each cache version writes its own file, so an older server sharing the directory neither reads nor replaces a newer cache. A v5
/// server reads the legacy (v4) file once, when its own is missing.
pub const SCAN_CACHE_FILE_NAME: &str = "usage-scan-cache-v5.json";
pub const LEGACY_SCAN_CACHE_FILE_NAME: &str = "usage-scan-cache.json";

/// Serialised as the index into this list.
const SPEEDS: [Speed; 3] = [Speed::Standard, Speed::Fast, Speed::Ultrafast];

pub struct CachedFile {
    /// `-1` for a legacy Codex entry: no file has that size, so a live rollout is re-parsed whole.
    pub size: i64,
    pub mtime_ms: f64,
    pub provider: &'static str,
    /// Records from newline-terminated lines, up to `position.resume_offset`.
    pub records: Vec<UsageRecord>,
    /// Records from a trailing segment the writer had not newline-terminated at parse time. Kept apart because an incremental parse
    /// re-reads that segment and would otherwise count it twice.
    pub tail_records: Vec<UsageRecord>,
    pub position: ParsePosition,
    /// The entry's JSON, made once by the writer that persists it (an entry is replaced, never changed, when its file changes).
    fragment: OnceLock<String>,
}

impl CachedFile {
    pub fn new(size: i64, mtime_ms: f64, provider: &'static str, records: Vec<UsageRecord>, tail_records: Vec<UsageRecord>, position: ParsePosition) -> Self {
        Self { size, mtime_ms, provider, records, tail_records, position, fragment: OnceLock::new() }
    }
    /// The entry's records, tail included.
    pub fn all_records(&self) -> Vec<UsageRecord> {
        let mut all = Vec::with_capacity(self.records.len() + self.tail_records.len());
        all.extend_from_slice(&self.records);
        all.extend_from_slice(&self.tail_records);
        all
    }
    /// Whether this read saw a later state of its file than `other`. Transcripts only grow.
    pub fn is_later_read_than(&self, other: &CachedFile) -> bool {
        self.mtime_ms > other.mtime_ms || (self.mtime_ms == other.mtime_ms && self.size > other.size)
    }
}

/// Path to entry, in the order entries were first cached: the aggregator keeps the first copy of a duplicate.
pub type ScanCache = IndexMap<String, Arc<CachedFile>>;

/// A whole number prints as an integer, as `JSON.stringify` prints it.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { json!(value as i64) } else { json!(value) }
}

fn speed_index(speed: Speed) -> usize {
    SPEEDS.iter().position(|candidate| *candidate == speed).unwrap_or(0)
}

#[derive(Default)]
struct Interned {
    values: Vec<String>,
    index: HashMap<String, usize>,
}
impl Interned {
    fn intern(&mut self, value: &str) -> usize {
        if let Some(existing) = self.index.get(value) {
            return *existing;
        }
        self.values.push(value.to_owned());
        self.index.insert(value.to_owned(), self.values.len() - 1);
        self.values.len() - 1
    }
}

pub(crate) fn codex_state_json(state: &Option<CodexScanState>) -> Value {
    match state {
        None => Value::Null,
        Some(state) => json!({"model":state.model,"speed":SPEEDS[speed_index(state.speed)].name(),"sessionId":state.session_id,"lastUsageSignature":state.last_usage_signature,"sawSessionMeta":state.saw_session_meta,"suppressingForkCopies":state.suppressing_fork_copies,"forkCopyAnchorMs":state.fork_copy_anchor_ms}),
    }
}

/// Serialises the cache, interning the repeated model and session strings. Keep one writer for the life of the cache: the intern
/// tables only grow, so an entry's memoised JSON stays valid, and only entries that changed since the last call are encoded again
/// (all of them made a persist cost close to a second on a large cache). A pruned entry can leave an unused string behind until the
/// next process start.
#[derive(Default)]
pub struct ScanCacheWriter {
    models: Interned,
    sessions: Interned,
}

impl ScanCacheWriter {
    fn serialize_file(&mut self, entry: &CachedFile) -> String {
        let mut record_rows = |records: &[UsageRecord]| -> Vec<Value> {
            records
                .iter()
                .map(|record| {
                    json!([
                        record.timestamp_ms,
                        self.models.intern(&record.model),
                        self.sessions.intern(&record.session_id),
                        record.totals.uncached_input,
                        record.totals.cached_input,
                        record.totals.cache_creation,
                        record.totals.output,
                        record.totals.reasoning,
                        record.dedupe_key,
                        record.reported_cost_usd.map(number),
                        speed_index(record.speed),
                    ])
                })
                .collect()
        };
        let (r, t) = (record_rows(&entry.records), record_rows(&entry.tail_records));
        // Keys in the original's order: s, m, p, r, t, o, gl, gh, cs.
        let mut file = Map::new();
        file.insert("s".into(), json!(entry.size));
        file.insert("m".into(), number(entry.mtime_ms));
        file.insert("p".into(), json!(entry.provider));
        file.insert("r".into(), Value::Array(r));
        file.insert("t".into(), Value::Array(t));
        file.insert("o".into(), json!(entry.position.resume_offset));
        file.insert("gl".into(), json!(entry.position.guard_length));
        file.insert("gh".into(), json!(entry.position.guard_hash));
        file.insert("cs".into(), codex_state_json(&entry.position.codex_state));
        Value::Object(file).to_string()
    }

    /// The document as JSON text; `extra` is written ahead of the version, as the original spreads it.
    pub fn write(&mut self, cache: &ScanCache, extra: &Map<String, Value>) -> String {
        let mut files = Vec::with_capacity(cache.len());
        for (path, entry) in cache {
            let fragment = match entry.fragment.get() {
                Some(fragment) => fragment.clone(),
                None => {
                    let fragment = self.serialize_file(entry);
                    let _ = entry.fragment.set(fragment.clone());
                    fragment
                }
            };
            files.push(format!("{}:{fragment}", Value::String(path.clone())));
        }
        // Encoded after the files, which may have added to the intern tables.
        let mut head = extra.clone();
        head.insert("version".into(), json!(VERSION));
        head.insert("models".into(), json!(self.models.values));
        head.insert("sessions".into(), json!(self.sessions.values));
        let head = Value::Object(head).to_string();
        format!("{},\"files\":{{{}}}}}", &head[..head.len() - 1], files.join(","))
    }
}

impl Speed {
    fn name(self) -> &'static str {
        match self {
            Speed::Standard => "standard",
            Speed::Fast => "fast",
            Speed::Ultrafast => "ultrafast",
        }
    }
}

fn safe_integer(value: &Value) -> Option<u64> {
    value.as_u64().filter(|number| *number <= 9_007_199_254_740_991).or_else(|| value.as_f64().filter(|number| number.fract() == 0.0 && *number >= 0.0 && *number <= 9_007_199_254_740_991.0).map(|number| number as u64))
}

/// A persisted Codex reducer state; `Err` for a corrupt value, which disqualifies the entry (a bad state would attach appended usage
/// to the wrong model or replay fork-copied history).
pub(crate) fn decode_codex_state(value: &Value) -> Result<Option<CodexScanState>, ()> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or(())?;
    let speed = match object.get("speed").and_then(Value::as_str) {
        Some("standard") => Speed::Standard,
        Some("fast") => Speed::Fast,
        Some("ultrafast") => Speed::Ultrafast,
        _ => return Err(()),
    };
    let text = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_owned).ok_or(());
    let flag = |name: &str| object.get(name).and_then(Value::as_bool).ok_or(());
    let signature = match object.get("lastUsageSignature") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(()),
    };
    let anchor = object.get("forkCopyAnchorMs").and_then(Value::as_f64).filter(|number| number.is_finite()).ok_or(())?;
    Ok(Some(CodexScanState { model: text("model")?, speed, session_id: text("sessionId")?, last_usage_signature: signature, saw_session_meta: flag("sawSessionMeta")?, suppressing_fork_copies: flag("suppressingForkCopies")?, fork_copy_anchor_ms: anchor as i64 }))
}

fn decode_records(rows: &[Value], provider: &'static str, models: &[String], sessions: &[String]) -> Option<Vec<UsageRecord>> {
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let row = row.as_array().filter(|row| row.len() >= 11)?;
        let timestamp = row[0].as_f64().filter(|number| number.is_finite())?;
        let model = models.get(usize::try_from(row[1].as_u64()?).ok()?)?;
        let count = |index: usize| row[index].as_u64();
        let speed = *SPEEDS.get(usize::try_from(row[10].as_u64()?).ok()?)?;
        // Any corrupt row disqualifies the entry: keeping the survivors under the original (size, mtime) would read as a valid warm hit,
        // and the file would never be parsed again, silently losing the dropped rows' usage.
        records.push(UsageRecord {
            provider,
            timestamp_ms: timestamp as i64,
            model: model.clone(),
            session_id: row[2].as_u64().and_then(|index| sessions.get(usize::try_from(index).ok()?)).cloned().unwrap_or_default(),
            totals: Totals { uncached_input: count(3)?, cached_input: count(4)?, cache_creation: count(5)?, output: count(6)?, reasoning: count(7)? },
            reported_cost_usd: row[9].as_f64(),
            speed,
            dedupe_key: row[8].as_str().map(str::to_owned),
        });
    }
    Some(records)
}

/// Rebuilds the cache from a parsed document. Anything malformed yields an empty cache, or loses the damaged entry: a corrupt cache
/// costs one cold scan, never a broken page.
pub fn decode_scan_cache(document: &Value) -> ScanCache {
    let mut cache = ScanCache::new();
    let Some(root) = document.as_object() else { return cache };
    let Some(version) = root.get("version").and_then(Value::as_f64) else { return cache };
    if version < SPEED_COMPATIBLE_SINCE_VERSION as f64 || version > VERSION as f64 {
        return cache;
    }
    let (Some(models), Some(sessions), Some(files)) = (root.get("models").and_then(Value::as_array), root.get("sessions").and_then(Value::as_array), root.get("files").and_then(Value::as_object)) else { return cache };
    // A numeric entry would land in a record's model and crash the aggregate at its rate lookup: a corrupt table rejects the cache.
    let strings = |values: &[Value]| values.iter().map(|value| value.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>();
    let (Some(models), Some(sessions)) = (strings(models), strings(sessions)) else { return cache };
    for (path, raw) in files {
        let Some(entry) = raw.as_object() else { continue };
        let (Some(size), Some(mtime)) = (entry.get("s").and_then(Value::as_f64), entry.get("m").and_then(Value::as_f64)) else { continue };
        let provider: &'static str = match entry.get("p").and_then(Value::as_str) {
            Some("claude") => "claude",
            Some("codex") => "codex",
            Some("grok") => "grok",
            _ => continue,
        };
        let (Some(rows), Some(tail_rows)) = (entry.get("r").and_then(Value::as_array), entry.get("t").and_then(Value::as_array)) else { continue };
        // Position fields feed byte offsets and a buffer allocation in the reader, so anything outside their real ranges rejects the
        // entry: a bogus guard length would otherwise fail every parse of the file, silently dropping its usage.
        let (Some(offset), Some(guard_length)) = (entry.get("o").and_then(safe_integer), entry.get("gl").and_then(safe_integer)) else { continue };
        let Some(guard_hash) = entry.get("gh").and_then(Value::as_f64).filter(|number| number.is_finite()) else { continue };
        if guard_length > GUARD_LENGTH || guard_length > offset {
            continue;
        }
        // v4 Codex records predate service tiers, so they all priced as standard. They are kept, because the rollout may be gone, but a
        // live rollout is re-parsed whole: no file has size -1, and a zero position cannot resume.
        let legacy_codex = provider == "codex" && version < VERSION as f64;
        let codex_state = if legacy_codex { None } else { match decode_codex_state(entry.get("cs").unwrap_or(&Value::Null)) { Ok(state) => state, Err(()) => continue } };
        let (Some(records), Some(tail_records)) = (decode_records(rows, provider, &models, &sessions), decode_records(tail_rows, provider, &models, &sessions)) else { continue };
        cache.insert(
            path.clone(),
            Arc::new(CachedFile::new(
                if legacy_codex { -1 } else { size as i64 },
                mtime,
                provider,
                records,
                tail_records,
                if legacy_codex { ParsePosition { resume_offset: 0, guard_length: 0, guard_hash: 0, codex_state: None } } else { ParsePosition { resume_offset: offset, guard_length, guard_hash: guard_hash as u32, codex_state } },
            )),
        );
    }
    cache
}

/// `pruneScanCache`: keeps saved usage after transcript cleanup until the reporting retention expires. Returns how many went.
pub fn prune_scan_cache(cache: &mut ScanCache, retention_cutoff_ms: f64) -> usize {
    let before = cache.len();
    cache.retain(|_, entry| entry.mtime_ms >= retention_cutoff_ms);
    before - cache.len()
}
