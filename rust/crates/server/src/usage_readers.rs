//! The usage of OpenCode and Antigravity, read from the databases and message files they keep
//! (`usage/opencodeUsageReader.ts`, `usage/antigravityUsageReader.ts`). Neither writes transcripts: OpenCode keeps one row (or, before
//! it migrated, one JSON file) per message, and Antigravity keeps its generation and step metadata as protobuf blobs in a SQLite
//! database per conversation. Both are opened read-only, so a running provider is never disturbed, and a busy writer fails a source
//! at once (a 100 ms busy timeout) rather than stalling the scan.
//!
//! A number the originals read as a JavaScript double (a timestamp with a fraction, a file's modification time) is cut to whole
//! milliseconds here; the records carry whole milliseconds.
use crate::usage_transcripts::{Speed, Totals};
pub use crate::usage_transcripts::UsageRecord as Record;
use crate::usage_transcripts::UsageRecord;
use regex::Regex;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};

// ---------------------------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------------------------

fn object(value: &Value) -> &serde_json::Map<String, Value> {
    static EMPTY: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(Default::default);
    value.as_object().unwrap_or(&EMPTY)
}

fn tokens(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_f64).filter(|value| value.is_finite() && *value > 0.0).map_or(0, |value| value.trunc() as u64)
}

fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

#[derive(Default)]
struct Fallback<'a> {
    id: &'a str,
    session_id: &'a str,
    timestamp_ms: Option<f64>,
}

/// `parseOpenCodeMessage`: OpenCode stores uncached input and reasoning separately from input and output.
fn parse_opencode_message(source: &str, fallback: &Fallback<'_>) -> Option<UsageRecord> {
    let parsed: Value = serde_json::from_str(source).ok()?;
    let message = object(&parsed);
    if message.get("role").is_some_and(|role| role != "assistant") {
        return None;
    }
    let usage = object(message.get("tokens").unwrap_or(&Value::Null));
    let cache = object(usage.get("cache").unwrap_or(&Value::Null));
    let model_reference = object(message.get("model").unwrap_or(&Value::Null));
    let model = [text(model_reference.get("id")), text(model_reference.get("modelID")), text(message.get("modelID"))].into_iter().find(|name| !name.is_empty()).unwrap_or("");
    // `created ?? fallback`: a value that is there but not a number is not replaced.
    let created = object(message.get("time").unwrap_or(&Value::Null)).get("created").filter(|value| !value.is_null());
    let timestamp_ms = match created {
        None => fallback.timestamp_ms,
        Some(value) => Some(value.as_f64()?),
    };
    let timestamp_ms = timestamp_ms.filter(|value| value.is_finite())?;
    if model.is_empty() {
        return None;
    }
    let reasoning = tokens(usage.get("reasoning"));
    let totals = Totals { uncached_input: tokens(usage.get("input")), cached_input: tokens(cache.get("read")), cache_creation: tokens(cache.get("write")), output: tokens(usage.get("output")) + reasoning, reasoning };
    if totals.total() == 0 {
        return None;
    }
    let id = if fallback.id.is_empty() { text(message.get("id")) } else { fallback.id };
    let session_id = if fallback.session_id.is_empty() { text(message.get("sessionID")) } else { fallback.session_id };
    // OpenCode writes zero for models without a known rate, including paid subscription models. The shared price table estimates those.
    let reported_cost_usd = message.get("cost").and_then(Value::as_f64).filter(|cost| cost.is_finite() && *cost > 0.0);
    Some(UsageRecord { provider: "opencode", timestamp_ms: timestamp_ms as i64, model: model.to_owned(), session_id: session_id.to_owned(), totals, reported_cost_usd, speed: Speed::Standard, dedupe_key: (!id.is_empty()).then(|| format!("opencode:{id}")) })
}

pub struct OpenCodeRead {
    pub files: Vec<(PathBuf, Vec<UsageRecord>)>,
    pub missing: bool,
    pub error: bool,
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    // A busy live provider fails this source promptly instead of stalling the server while SQLite waits for its writer.
    connection.busy_timeout(std::time::Duration::from_millis(100))?;
    Ok(connection)
}

fn table_names(connection: &Connection) -> rusqlite::Result<HashSet<String>> {
    connection.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?.query_map([], |row| row.get::<_, String>(0))?.collect()
}

fn column_text(row: &rusqlite::Row<'_>, index: usize) -> String {
    match row.get_ref(index) {
        Ok(ValueRef::Text(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

/// What ICU's `localeCompare` makes of two names.
fn locale_order(left: &str, right: &str) -> std::cmp::Ordering {
    crate::usage_aggregation::locale_compare(left, right)
}

/// Reads the current SQLite and the pre-migration JSON stores without modifying either (`readOpenCodeUsage`).
pub fn read_opencode_usage(root: &Path, since_ms: i64) -> OpenCodeRead {
    let mut files: Vec<(PathBuf, Vec<UsageRecord>)> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    let (mut found, mut error) = (false, false);
    let append = |records: &mut Vec<UsageRecord>, seen: &mut HashSet<String>, record: Option<UsageRecord>| {
        let Some(record) = record.filter(|record| record.timestamp_ms >= since_ms) else { return };
        if let Some(key) = &record.dedupe_key {
            if !seen.insert(key.clone()) {
                return;
            }
        }
        records.push(record);
    };

    static DATABASE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^opencode(?:-[a-zA-Z0-9_-]+)?\.db$").expect("a valid pattern"));
    let mut databases: Vec<String> = vec![];
    match std::fs::read_dir(root) {
        Ok(entries) => {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|kind| kind.is_file()) && DATABASE_NAME.is_match(&name) {
                    databases.push(name);
                }
            }
        }
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => error = true,
    }
    databases.sort_by(|a, b| match (a == "opencode.db", b == "opencode.db") {
        (true, _) => std::cmp::Ordering::Less,
        (_, true) => std::cmp::Ordering::Greater,
        _ => locale_order(a, b),
    });
    for name in databases {
        found = true;
        let path = root.join(&name);
        let mut records = vec![];
        let read = (|| -> rusqlite::Result<bool> {
            let connection = open_read_only(&path)?;
            let tables = table_names(&connection)?;
            let mut broken = !tables.contains("message") && !tables.contains("session_message");
            for table in ["message", "session_message"] {
                if !tables.contains(table) {
                    continue;
                }
                let columns: HashSet<String> = connection.prepare(&format!("PRAGMA table_info({table})"))?.query_map([], |row| row.get::<_, String>(1))?.collect::<Result<_, _>>()?;
                let has_time = columns.contains("time_created");
                let timestamp = if has_time { "time_created" } else { "NULL" };
                let mut predicates: Vec<&str> = if table == "session_message" { vec!["type = 'assistant'"] } else { vec![] };
                if has_time {
                    predicates.push("time_created >= ?");
                }
                let filter = if predicates.is_empty() { String::new() } else { format!(" WHERE {}", predicates.join(" AND ")) };
                let mut statement = connection.prepare(&format!("SELECT id, session_id, data, {timestamp} AS created FROM {table}{filter}"))?;
                let mut rows = if has_time { statement.query([since_ms])? } else { statement.query([])? };
                while let Some(row) = rows.next()? {
                    let (id, session_id, data) = (column_text(row, 0), column_text(row, 1), column_text(row, 2));
                    let created = match row.get_ref(3)? {
                        ValueRef::Integer(value) => Some(value as f64),
                        ValueRef::Real(value) => Some(value),
                        _ => None,
                    };
                    append(&mut records, &mut seen, parse_opencode_message(&data, &Fallback { id: &id, session_id: &session_id, timestamp_ms: created }));
                }
            }
            if broken {
                broken = true;
            }
            Ok(broken)
        })();
        match read {
            Ok(true) | Err(_) => error = true,
            Ok(false) => {}
        }
        files.push((path, records));
    }

    // Do not follow symlinks, including cycles. Database records win over their old JSON copies when OpenCode has migrated a store in place.
    let mut directories = vec![root.join("storage").join("message")];
    while let Some(directory) = directories.pop() {
        match std::fs::read_dir(&directory) {
            Ok(entries) => {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    let Ok(kind) = entry.file_type() else { continue };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if kind.is_dir() {
                        directories.push(path);
                    } else if kind.is_file() && name.ends_with(".json") {
                        found = true;
                        let id = &name[..name.len() - 5];
                        if seen.contains(&format!("opencode:{id}")) {
                            continue;
                        }
                        let mut records = vec![];
                        match std::fs::read(&path) {
                            Ok(bytes) => append(&mut records, &mut seen, parse_opencode_message(&String::from_utf8_lossy(&bytes), &Fallback { id, ..Default::default() })),
                            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
                            Err(_) => error = true,
                        }
                        files.push((path, records));
                    }
                }
            }
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => error = true,
        }
    }
    OpenCodeRead { files, missing: !found && !error, error }
}

// ---------------------------------------------------------------------------------------------
// Antigravity
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Invalid;

/// A decoded protobuf field. A varint above 2^53 - 1 is a `bigint` in the original and reads as zero wherever a number is wanted.
#[derive(Clone)]
enum Field {
    Number(u64),
    Big,
    Bytes(Vec<u8>),
}

type Fields = HashMap<u64, Vec<Field>>;

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Antigravity stores usage metadata as protobuf, independently of conversation text.
fn fields(bytes: &[u8]) -> Result<Fields, Invalid> {
    let mut offset = 0usize;
    let mut result: Fields = HashMap::new();
    let varint = |offset: &mut usize| -> Result<Field, Invalid> {
        let mut value = 0u128;
        let mut shift = 0u32;
        while shift < 70 {
            let byte = *bytes.get(*offset).ok_or(Invalid)?;
            *offset += 1;
            if shift == 63 && byte > 1 {
                return Err(Invalid);
            }
            value |= u128::from(byte & 127) << shift;
            if byte < 128 {
                return Ok(if value > u128::from(MAX_SAFE_INTEGER) { Field::Big } else { Field::Number(value as u64) });
            }
            shift += 7;
        }
        Err(Invalid)
    };
    while offset < bytes.len() {
        let Field::Number(tag) = varint(&mut offset)? else { return Err(Invalid) };
        let (number, wire) = (tag / 8, tag % 8);
        if number == 0 {
            return Err(Invalid);
        }
        let value = match wire {
            0 => varint(&mut offset)?,
            1 | 2 | 5 => {
                let length = match wire {
                    2 => match varint(&mut offset)? {
                        Field::Number(length) => length as usize,
                        _ => return Err(Invalid),
                    },
                    1 => 8,
                    _ => 4,
                };
                if length > bytes.len() - offset {
                    return Err(Invalid);
                }
                let value = &bytes[offset..offset + length];
                offset += length;
                if wire != 2 {
                    continue;
                }
                Field::Bytes(value.to_vec())
            }
            _ => return Err(Invalid),
        };
        result.entry(number).or_default().push(value);
    }
    Ok(result)
}

fn number_at(value: &Fields, key: u64) -> u64 {
    match value.get(&key).and_then(|entries| entries.first()) {
        Some(Field::Number(number)) => *number,
        _ => 0,
    }
}

fn bytes_at(value: &Fields, key: u64) -> Option<&[u8]> {
    match value.get(&key).and_then(|entries| entries.first()) {
        Some(Field::Bytes(bytes)) => Some(bytes),
        _ => None,
    }
}

fn nested(value: &Fields, key: u64) -> Result<Fields, Invalid> {
    bytes_at(value, key).map_or_else(|| Ok(HashMap::new()), fields)
}

/// `TextDecoder("utf-8", { fatal: true })` and a trim: bytes that are not UTF-8 are an error, not replacement characters.
fn text_at(value: &Fields, key: u64) -> Result<String, Invalid> {
    match bytes_at(value, key) {
        None => Ok(String::new()),
        Some(bytes) => Ok(crate::text_generation::js_trim(std::str::from_utf8(bytes).map_err(|_| Invalid)?).to_owned()),
    }
}

fn timestamp(value: &Fields) -> Option<i64> {
    let seconds = number_at(value, 1);
    (seconds > 0).then(|| (seconds * 1000 + number_at(value, 2) / 1_000_000) as i64)
}

fn model_id(id: u64) -> Option<&'static str> {
    Some(match id {
        246 => "gemini-2.5-pro",
        312 => "gemini-2.5-flash",
        313 | 329 => "gemini-2.5-flash-thinking",
        330 => "gemini-2.5-flash-lite",
        281 | 282 => "claude-sonnet-4",
        290 | 291 => "claude-opus-4",
        333 | 334 => "claude-sonnet-4-5",
        340 | 341 => "claude-haiku-4-5",
        1026 => "claude-opus-4-6",
        1035 => "claude-sonnet-4-6",
        1016 | 1036 | 1037 => "gemini-3.1-pro",
        1018 | 1084 | 1047 => "gemini-3-flash-preview",
        _ => return None,
    })
}

fn model_name(name: &str, id: u64) -> String {
    if !name.is_empty() {
        static SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\([^)]*\)\s*$").expect("a valid pattern"));
        static CLAUDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^claude-(4(?:\.\d+)?)-(sonnet|opus|haiku)").expect("a valid pattern"));
        let lowered = name.to_lowercase();
        let normalized = SUFFIX.replace(&lowered, "").replace(' ', "-");
        if normalized.starts_with("claude-") {
            return CLAUDE.replace(&normalized, "claude-$2-$1").replace('.', "-");
        }
        return normalized;
    }
    match model_id(id) {
        Some(name) => name.to_owned(),
        None if id > 0 => format!("antigravity-model-{id}"),
        None => String::new(),
    }
}

struct Metadata {
    model: String,
    timestamp_ms: Option<i64>,
    usages: Vec<Fields>,
}

fn metadata(bytes: &[u8], step: bool) -> Result<Metadata, Invalid> {
    let root = fields(bytes)?;
    if !step && bytes_at(&root, 1).is_none() {
        return Err(Invalid);
    }
    let data = if step { root.clone() } else { nested(&root, 1)? };
    let model = if step { nested(&data, 24)? } else { data.clone() };
    let mut usages = match bytes_at(&data, if step { 9 } else { 4 }) {
        Some(usage) => vec![fields(usage)?],
        None => vec![],
    };
    for retry in data.get(&(if step { 28 } else { 17 })).into_iter().flatten() {
        let Field::Bytes(retry) = retry else { return Err(Invalid) };
        if let Some(usage) = bytes_at(&fields(retry)?, 2) {
            usages.push(fields(usage)?);
        }
    }
    let (name_key, fallback_key, id_key) = if step { (12, 8, 1) } else { (19, 21, 3) };
    let name = match text_at(&model, name_key)? {
        name if !name.is_empty() => name,
        _ => text_at(&model, fallback_key)?,
    };
    let timestamp_ms = if step {
        match timestamp(&nested(&data, 8)?) {
            Some(value) => Some(value),
            None => timestamp(&nested(&data, 1)?),
        }
    } else {
        timestamp(&nested(&nested(&data, 9)?, 4)?)
    };
    Ok(Metadata { model: model_name(&name, number_at(&model, id_key)), timestamp_ms, usages })
}

struct Candidate {
    record: UsageRecord,
    keys: Vec<String>,
    timestamp_quality: u8,
}

struct Group {
    candidate: Candidate,
    parent: usize,
    size: usize,
    owner: usize,
    file_index: usize,
}

fn blob(value: ValueRef<'_>) -> Result<&[u8], Invalid> {
    match value {
        ValueRef::Blob(bytes) => Ok(bytes),
        _ => Err(Invalid),
    }
}

/// One conversation database's usage records, in the order the original produces them (steps, then generations).
fn read_database(path: &Path, fallback_timestamp: i64) -> Result<Vec<Candidate>, Invalid> {
    let connection = open_read_only(path).map_err(|_| Invalid)?;
    connection.execute_batch("BEGIN").map_err(|_| Invalid)?;
    let tables = table_names(&connection).map_err(|_| Invalid)?;
    if !tables.contains("gen_metadata") && !tables.contains("steps") {
        return Err(Invalid);
    }
    let read_metadata = |query: &str, step: bool| -> Result<Vec<(i64, Metadata)>, Invalid> {
        let mut statement = connection.prepare(query).map_err(|_| Invalid)?;
        let mut rows = statement.query([]).map_err(|_| Invalid)?;
        let mut entries = vec![];
        while let Some(row) = rows.next().map_err(|_| Invalid)? {
            let index = match row.get_ref(0).map_err(|_| Invalid)? {
                ValueRef::Integer(value) => value,
                ValueRef::Real(value) if value.fract() == 0.0 => value as i64,
                _ => return Err(Invalid),
            };
            entries.push((index, metadata(blob(row.get_ref(1).map_err(|_| Invalid)?)?, step)?));
        }
        Ok(entries)
    };
    let generations = if tables.contains("gen_metadata") { read_metadata("SELECT idx, data FROM gen_metadata ORDER BY idx", false)? } else { vec![] };
    let mut trajectory_timestamp: Option<i64> = None;
    if tables.contains("trajectory_metadata_blob") {
        let mut statement = connection.prepare("SELECT data FROM trajectory_metadata_blob").map_err(|_| Invalid)?;
        let mut rows = statement.query([]).map_err(|_| Invalid)?;
        while let Some(row) = rows.next().map_err(|_| Invalid)? {
            let found = timestamp(&nested(&fields(blob(row.get_ref(0).map_err(|_| Invalid)?)?)?, 2)?);
            trajectory_timestamp = trajectory_timestamp.or(found);
        }
    }
    let steps = if tables.contains("steps") { read_metadata("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx", true)? } else { vec![] };
    let session_id = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let generation_models: HashMap<i64, &str> = generations.iter().map(|(index, entry)| (*index, entry.model.as_str())).collect();
    let mut records = vec![];
    for (source, entries) in [("step", &steps), ("generation", &generations)] {
        for (index, (idx, entry)) in entries.iter().enumerate() {
            for (usage_index, usage) in entry.usages.iter().enumerate() {
                let output = number_at(usage, 3).max(number_at(usage, 9) + number_at(usage, 10));
                let totals = Totals { uncached_input: number_at(usage, 2), cached_input: number_at(usage, 5), cache_creation: number_at(usage, 4), output, reasoning: output.min(number_at(usage, 9)) };
                if totals.total() == 0 {
                    continue;
                }
                let mut keys = vec![];
                for key in [11u64, 12, 7] {
                    let id = text_at(usage, key)?;
                    if !id.is_empty() {
                        keys.push(format!("antigravity:{key}:{id}"));
                    }
                }
                let usage_model = number_at(usage, 1);
                let model = [
                    model_id(usage_model).unwrap_or("").to_owned(),
                    entry.model.clone(),
                    if source == "step" { generation_models.get(idx).map_or(String::new(), |model| (*model).to_owned()) } else { String::new() },
                    model_name("", usage_model),
                ]
                .into_iter()
                .find(|model| !model.is_empty())
                .unwrap_or_else(|| "antigravity-unknown".to_owned());
                let record = UsageRecord {
                    provider: "antigravity",
                    timestamp_ms: entry.timestamp_ms.or(trajectory_timestamp).unwrap_or(fallback_timestamp),
                    model,
                    session_id: session_id.clone(),
                    totals,
                    reported_cost_usd: None,
                    speed: Speed::Standard,
                    dedupe_key: Some(keys.first().cloned().unwrap_or_else(|| format!("antigravity:{session_id}:{source}:{index}:{usage_index}"))),
                };
                records.push(Candidate { record, timestamp_quality: if entry.timestamp_ms.is_some() { 2 } else if trajectory_timestamp.is_some() { 1 } else { 0 }, keys });
            }
        }
    }
    Ok(records)
}

pub struct AntigravityFile {
    pub root: PathBuf,
    pub path: PathBuf,
    pub records: Vec<UsageRecord>,
}

pub struct AntigravityRead {
    pub files: Vec<AntigravityFile>,
    pub errors: Vec<PathBuf>,
}

fn find(groups: &mut [Group], mut index: usize) -> usize {
    let mut root = index;
    while groups[root].parent != root {
        root = groups[root].parent;
    }
    while index != root {
        let parent = groups[index].parent;
        groups[index].parent = root;
        index = parent;
    }
    root
}

/// Joins two records of one response, taking what the better-known side has and the larger of every count.
fn merge(groups: &mut [Group], left: usize, right: usize) -> usize {
    let (mut a, mut b) = (find(groups, left), find(groups, right));
    if a == b {
        return a;
    }
    if groups[a].size < groups[b].size {
        std::mem::swap(&mut a, &mut b);
    }
    let (target, source) = (&groups[a], &groups[b]);
    let first_is_target = target.owner < source.owner;
    let first = if first_is_target { target } else { source };
    let best_time = if source.candidate.timestamp_quality > target.candidate.timestamp_quality
        || (source.candidate.timestamp_quality == target.candidate.timestamp_quality && source.candidate.record.timestamp_ms < target.candidate.record.timestamp_ms)
    {
        source
    } else {
        target
    };
    let (x, y) = (&target.candidate.record.totals, &source.candidate.record.totals);
    let mut record = first.candidate.record.clone();
    if first.candidate.record.model == "antigravity-unknown" {
        record.model = if first_is_target { source.candidate.record.model.clone() } else { target.candidate.record.model.clone() };
    }
    record.timestamp_ms = best_time.candidate.record.timestamp_ms;
    record.totals = Totals {
        uncached_input: x.uncached_input.max(y.uncached_input),
        cached_input: x.cached_input.max(y.cached_input),
        cache_creation: x.cache_creation.max(y.cache_creation),
        output: x.output.max(y.output),
        reasoning: x.reasoning.max(y.reasoning),
    };
    let (quality, owner, file_index, size) = (best_time.candidate.timestamp_quality, first.owner, first.file_index, target.size + source.size);
    let target = &mut groups[a];
    target.candidate.record = record;
    target.candidate.timestamp_quality = quality;
    target.owner = owner;
    target.file_index = file_index;
    target.size = size;
    groups[b].parent = a;
    a
}

/// Reads and merges aliases across every configured Antigravity store before date filtering (`readAntigravityUsage`).
pub fn read_antigravity_usage(roots: &[PathBuf], since_ms: i64) -> AntigravityRead {
    let mut files: Vec<AntigravityFile> = vec![];
    let mut errors: Vec<PathBuf> = vec![];
    let mut identities: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Group> = vec![];
    let mut visited: HashSet<PathBuf> = HashSet::new();

    fn walk(directory: &Path, root: &Path, files: &mut Vec<AntigravityFile>, errors: &mut Vec<PathBuf>, groups: &mut Vec<Group>, identities: &mut HashMap<String, usize>, visited: &mut HashSet<PathBuf>) {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(cause) => {
                if cause.kind() != std::io::ErrorKind::NotFound {
                    errors.push(directory.to_owned());
                }
                return;
            }
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by(|a, b| locale_order(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                walk(&path, root, files, errors, groups, identities, visited);
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".db") {
                let outcome = (|| -> Result<(), ()> {
                    let canonical = std::fs::canonicalize(&path).map_err(|_| ())?;
                    if !visited.insert(canonical) {
                        return Ok(());
                    }
                    let modified = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).map_err(|_| ())?;
                    let fallback = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as i64);
                    let candidates = read_database(&path, fallback).map_err(|_| ())?;
                    let file_index = files.len();
                    files.push(AntigravityFile { root: root.to_owned(), path: path.clone(), records: vec![] });
                    for candidate in candidates {
                        let index = groups.len();
                        let keys = candidate.keys.clone();
                        groups.push(Group { candidate, parent: index, size: 1, owner: index, file_index });
                        for key in keys {
                            if let Some(existing) = identities.get(&key).copied() {
                                merge(groups, index, existing);
                            }
                            identities.insert(key, index);
                        }
                    }
                    Ok(())
                })();
                if outcome.is_err() {
                    errors.push(path);
                }
            }
        }
    }
    for root in roots {
        walk(root, root, &mut files, &mut errors, &mut groups, &mut identities, &mut visited);
    }
    for index in 0..groups.len() {
        if groups[index].parent == index && groups[index].candidate.record.timestamp_ms >= since_ms {
            let file_index = groups[index].file_index;
            files[file_index].records.push(groups[index].candidate.record.clone());
        }
    }
    AntigravityRead { files, errors }
}
