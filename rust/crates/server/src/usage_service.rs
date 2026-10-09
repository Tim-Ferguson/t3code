//! Usage summaries (`UsageService.ts`): scans the Claude Code and Codex session transcripts the
//! provider CLIs keep on disk, including work driven outside T3 Code, prices the records and folds
//! them into `(day, hour?, provider, model, source)` buckets.
//!
//! Rates come from LiteLLM's published table. The first fetch is cached under the state directory
//! (`usage-model-rates.json`), a day-old table keeps the page working offline, and an explicit
//! refresh ignores the day but not a table fetched in the last minute. `T3CODE_USAGE_RATES_URL`
//! replaces the origin so tests can serve a loopback table.
//!
//! Divergence: the original keeps every parsed file in a persisted scan cache for 90 days so usage
//! survives its transcripts being deleted and a warm scan resumes appended files from a byte
//! position. This port re-reads transcripts newer than the window on every scan and remembers only
//! the last parse of each file in memory. Grok, Cursor, OpenCode and Antigravity are not read.
use crate::{
    usage_aggregation::{Aggregator, Options, resolve_model_aliases},
    usage_pricing::{RateTable, override_table, parse_rate_table},
    usage_transcripts::{CodexScanState, UsageRecord, dedupe_within_file, might_carry_usage, parse_claude_line, parse_codex_line},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::BufRead,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

pub const USAGE_CONTRACT_VERSION: u64 = 6;
pub const LITELLM_RATES_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
/// Rates move rarely; a day-old table keeps the page working offline.
const RATES_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// An explicit refresh ignores the TTL, but not a table fetched this recently.
const RATES_REFRESH_FLOOR_MS: i64 = 60 * 1000;
/// Files are filtered by mtime before opening. The slack covers a session whose last write lands
/// just before local midnight on the window's first day.
const MTIME_SLACK_MS: i64 = 36 * 60 * 60 * 1000;
const MAX_HOURLY_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// `UsageReadError`
pub struct ReadError {
    pub reason: &'static str,
    pub detail: String,
}
impl ReadError {
    pub fn wire(&self) -> Value {
        json!({"_tag":"UsageReadError","reason":self.reason,"detail":self.detail})
    }
}
fn invalid(detail: impl Into<String>) -> ReadError {
    ReadError { reason: "invalidWindow", detail: detail.into() }
}

#[derive(Default)]
struct RatesState {
    table: RateTable,
    fetched_at_ms: Option<i64>,
    /// `fresh`, `cached` or `unavailable`.
    status: Option<&'static str>,
}

struct CachedFile {
    size: u64,
    mtime_ms: i64,
    provider: &'static str,
    records: Vec<UsageRecord>,
}

pub struct UsageService {
    state_dir: PathBuf,
    rates: tokio::sync::Mutex<RatesState>,
    files: Mutex<HashMap<PathBuf, Arc<CachedFile>>>,
    http: reqwest::Client,
}

/// One service per state directory, so the rate table and parse cache outlive a request.
pub fn of(state_dir: &Path) -> Arc<UsageService> {
    static SERVICES: OnceLock<Mutex<HashMap<PathBuf, Arc<UsageService>>>> = OnceLock::new();
    SERVICES.get_or_init(Default::default).lock().unwrap().entry(state_dir.to_path_buf()).or_insert_with(|| Arc::new(UsageService { state_dir: state_dir.to_path_buf(), rates: Default::default(), files: Default::default(), http: reqwest::Client::new() })).clone()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}
fn rates_url() -> String {
    std::env::var("T3CODE_USAGE_RATES_URL").ok().filter(|url| !url.trim().is_empty()).unwrap_or_else(|| LITELLM_RATES_URL.into())
}

impl UsageService {
    fn pricing(&self, state: &RatesState) -> Value {
        json!({"status":state.status.unwrap_or("unavailable"),"source":rates_url(),"fetchedAt":state.fetched_at_ms.map(iso),"knownModels":state.table.len()})
    }

    /// `loadRates`: a fresh copy when the table is older than `max_age`, falling back to the on-disk
    /// snapshot. With neither, every model reports as unpriced rather than the page failing.
    async fn load_rates(&self, state: &mut RatesState, force: bool) {
        let now = now_ms();
        let max_age = if force { RATES_REFRESH_FLOOR_MS } else { RATES_TTL_MS };
        if state.fetched_at_ms.is_some_and(|fetched| now - fetched < max_age) {
            return;
        }
        let cache_path = self.state_dir.join("usage-model-rates.json");
        if state.fetched_at_ms.is_none() {
            if let Some(cached) = std::fs::read_to_string(&cache_path).ok().and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) {
                let parsed = parse_rate_table(&cached["document"]);
                if let (false, Some(fetched)) = (parsed.is_empty(), cached["fetchedAtMs"].as_f64()) {
                    state.table = parsed;
                    state.fetched_at_ms = Some(fetched as i64);
                    state.status = Some("cached");
                    if now - fetched as i64 <= max_age - 1 {
                        return;
                    }
                }
            }
        }
        let fetched = async { self.http.get(rates_url()).send().await?.error_for_status()?.json::<Value>().await };
        let Ok(Ok(document)) = tokio::time::timeout(Duration::from_secs(10), fetched).await else {
            // The refresh failed; whatever is being served is past its TTL and must not claim to be fresh.
            if !state.table.is_empty() {
                state.status = Some("cached");
            }
            return;
        };
        let parsed = parse_rate_table(&document);
        if parsed.is_empty() {
            return;
        }
        state.table = parsed;
        state.fetched_at_ms = Some(now);
        state.status = Some("fresh");
        let contents = json!({"fetchedAtMs":now,"document":document}).to_string();
        let temporary = cache_path.with_extension("json.tmp");
        if std::fs::create_dir_all(&self.state_dir).is_ok() && std::fs::write(&temporary, contents).is_ok() {
            let _ = std::fs::rename(&temporary, &cache_path);
        }
    }

    /// `refreshRates`: refetch ahead of the TTL so a model LiteLLM added since gets priced now.
    pub async fn refresh_rates(&self) -> Value {
        let mut state = self.rates.lock().await;
        self.load_rates(&mut state, true).await;
        self.pricing(&state)
    }

    /// `readSummary`
    pub async fn read_summary(self: &Arc<Self>, input: &Value, settings: &Value) -> Result<Value, ReadError> {
        let started = now_ms();
        let (since_day, until_day) = (input["sinceDay"].as_str().unwrap_or(""), input["untilDay"].as_str().unwrap_or(""));
        let time_zone = input["timeZone"].as_str().unwrap_or("UTC");
        if since_day > until_day {
            return Err(invalid(format!("sinceDay '{since_day}' is after untilDay '{until_day}'")));
        }
        let hourly = if input["resolution"] == "hour" {
            let parse = |name: &str| input[name].as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map(|time| time.timestamp_millis());
            let (Some(since), Some(until)) = (parse("sinceTime"), parse("untilTime")) else { return Err(invalid("Hourly usage requires valid sinceTime and untilTime instants")) };
            if until - since <= 0 || until - since > MAX_HOURLY_WINDOW_MS {
                return Err(invalid("Hourly usage window must be greater than zero and at most 24 hours"));
            }
            Some((since, until))
        } else {
            None
        };
        let Some(window_start) = chrono::NaiveDate::parse_from_str(since_day, "%Y-%m-%d").ok().and_then(|date| date.and_hms_opt(0, 0, 0)).map(|time| time.and_utc().timestamp_millis()) else {
            return Err(invalid(format!("sinceDay '{since_day}' is not a valid date")));
        };
        let window_start_ms = hourly.map_or(window_start, |(since, _)| since) - MTIME_SLACK_MS;
        // Pricing only matters once records are aggregated, so the table loads while transcripts stream.
        let rates = async {
            let mut state = self.rates.lock().await;
            self.load_rates(&mut state, false).await;
            (state.table.clone(), self.pricing(&state))
        };
        let scan = {
            let this = self.clone();
            let settings = settings.clone();
            tokio::task::spawn_blocking(move || this.scan(&settings, window_start_ms))
        };
        let ((table, pricing), scanned) = tokio::join!(rates, scan);
        let scanned = scanned.map_err(|error| ReadError { reason: "scanFailed", detail: format!("The transcript scan failed: {error}") })?;

        let overrides = override_table(settings["usagePriceOverrides"].as_object().into_iter().flatten().map(|(model, prices)| (model.clone(), prices.clone())));
        let aliases: Vec<(String, String)> = settings["usageModelAliases"].as_object().into_iter().flatten().filter_map(|(from, to)| Some((from.clone(), to.as_str()?.to_owned()))).collect();
        let mut aggregator = Aggregator::new(Options {
            time_zone: time_zone.to_owned(),
            since_day: since_day.to_owned(),
            until_day: until_day.to_owned(),
            rates: table,
            price_overrides: Some(overrides),
            model_aliases: resolve_model_aliases(&aliases),
            hourly,
        });
        let shared = shared_codex_sessions(scanned.iter().flat_map(|dir| dir.files.iter().flatten()));
        let host_id = hostname();
        let mut sources = vec![];
        for dir in &scanned {
            let (mut scanned_files, mut skipped_files) = (0u64, 0u64);
            let mut sessions = HashSet::new();
            for file in dir.files.iter().flatten() {
                if file.1.is_empty() {
                    skipped_files += 1;
                    continue;
                }
                scanned_files += 1;
                let mut occurrences: HashMap<String, u64> = HashMap::new();
                for record in file.1.iter() {
                    let mut record = record.clone();
                    if record.provider == "codex" && shared.contains(&record.session_id) {
                        // Match moved rollout copies without collapsing repeated equal events within one rollout.
                        let key = json!([record.provider, record.session_id, record.timestamp_ms, record.model, [record.totals.uncached_input, record.totals.cached_input, record.totals.cache_creation, record.totals.output, record.totals.reasoning]]).to_string();
                        let occurrence = occurrences.entry(key.clone()).and_modify(|count| *count += 1).or_insert(1);
                        record.dedupe_key = Some(format!("{key}:{occurrence}"));
                    }
                    // Only sessions contributing in-window count; the mtime slack admits boundary files.
                    if aggregator.add(&record, &dir.dir) && !record.session_id.is_empty() {
                        sessions.insert(record.session_id.clone());
                    }
                }
            }
            let missing = dir.files.is_none();
            sources.push(json!({
                "fingerprint":{"hostId":host_id,"provider":dir.provider,"resolvedHomePath":dir.dir,"volumeId":dir.volume_id},
                "status":if missing && scanned_files == 0 { "missing" } else { "ok" },
                "scannedFiles":scanned_files,"skippedFiles":skipped_files,"malformedRecords":0,"distinctSessions":sessions.len(),
                "message":if missing { json!("No transcript directory on this environment.") } else { Value::Null },
            }));
        }
        let aggregated = aggregator.finish();
        Ok(json!({
            "contractVersion":USAGE_CONTRACT_VERSION,"readAt":iso(now_ms()),"timeZone":time_zone,"sinceDay":since_day,"untilDay":until_day,
            "buckets":aggregated.buckets,"sources":sources,"pricing":pricing,"scanDurationMs":(now_ms() - started).max(0),
        }))
    }

    /// Walks each provider's transcript directory and parses the files newer than the window.
    fn scan(&self, settings: &Value, window_start_ms: i64) -> Vec<ScannedDir> {
        let mut scanned = vec![];
        for (provider, dir) in transcript_dirs(settings) {
            let volume_id = volume_id(&dir);
            if !dir.is_dir() {
                scanned.push(ScannedDir { provider, dir: dir.to_string_lossy().into_owned(), volume_id, files: None });
                continue;
            }
            let mut files = vec![];
            for (path, size, mtime_ms) in list_transcripts(&dir, window_start_ms) {
                let records = self.file_records(&path, size, mtime_ms, provider);
                files.push((path.to_string_lossy().into_owned(), records));
            }
            scanned.push(ScannedDir { provider, dir: dir.to_string_lossy().into_owned(), volume_id, files: Some(files) });
        }
        scanned
    }

    /// One transcript's records, reusing the last parse when the file is unchanged.
    fn file_records(&self, path: &Path, size: u64, mtime_ms: i64, provider: &'static str) -> Arc<Vec<UsageRecord>> {
        if let Some(cached) = self.files.lock().unwrap().get(path).filter(|cached| cached.size == size && cached.mtime_ms == mtime_ms && cached.provider == provider) {
            return Arc::new(cached.records.clone());
        }
        let Some(parsed) = read_transcript(path, provider) else {
            // A read failure is not an empty transcript: do not cache it.
            return Arc::new(vec![]);
        };
        self.files.lock().unwrap().insert(path.to_path_buf(), Arc::new(CachedFile { size, mtime_ms, provider, records: parsed.clone() }));
        Arc::new(parsed)
    }
}

struct ScannedDir {
    provider: &'static str,
    dir: String,
    volume_id: String,
    /// Parsed records per file, `None` when the directory does not exist.
    files: Option<Vec<(String, Arc<Vec<UsageRecord>>)>>,
}

/// Codex sessions with records in more than one file, such as a rollout that moved after it was
/// read. Only these need cross-file dedupe keys.
fn shared_codex_sessions<'a>(files: impl Iterator<Item = &'a (String, Arc<Vec<UsageRecord>>)>) -> HashSet<String> {
    let mut first: HashMap<&str, usize> = HashMap::new();
    let mut shared = HashSet::new();
    for (index, (_, records)) in files.enumerate() {
        let mut previous = "";
        for record in records.iter() {
            if record.provider != "codex" || record.session_id == previous || record.session_id.is_empty() {
                continue;
            }
            previous = &record.session_id;
            match first.get(record.session_id.as_str()) {
                None => {
                    first.insert(&record.session_id, index);
                }
                Some(first) if *first != index => {
                    shared.insert(record.session_id.clone());
                }
                _ => {}
            }
        }
    }
    shared
}

fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its length and `gethostname` NUL-terminates within it.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return String::new();
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}
#[cfg(unix)]
fn volume_id(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|metadata| format!("{}:{}", metadata.dev(), metadata.ino())).unwrap_or_default()
}
#[cfg(not(unix))]
fn volume_id(_path: &Path) -> String {
    String::new()
}

/// The transcript directory of every Claude Code and Codex instance. Disabled accounts still have
/// history; the default slots apply when settings do not name that driver's own instance.
fn transcript_dirs(settings: &Value) -> Vec<(&'static str, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let instances = settings["providerInstances"].as_object().cloned().unwrap_or_default();
    let mut dirs: Vec<(&'static str, PathBuf)> = vec![];
    for driver in ["claudeAgent", "codex"] {
        let mut entries: Vec<(Value, Value)> = instances.iter().filter(|(_, instance)| instance["driver"] == driver).map(|(_, instance)| (instance["config"].clone(), instance["environment"].clone())).collect();
        if !instances.contains_key(driver) {
            entries.push((Value::Null, Value::Null));
        }
        for (config, environment) in entries {
            if !(config.is_null() || config.is_object()) {
                continue;
            }
            let environment_value = |name: &str| {
                environment.as_array().into_iter().flatten().find(|variable| variable["name"] == name).and_then(|variable| variable["value"].as_str().map(str::to_owned)).or_else(|| std::env::var(name).ok()).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
            };
            let text = |name: &str| config[name].as_str().map(str::trim).unwrap_or("").to_owned();
            let expand = |value: &str| crate::claude_skills::expand_home(value, &home);
            let (provider, resolved) = if driver == "codex" {
                let configured = text("homePath");
                let shadow = text("shadowHomePath");
                let managed = config["setupMode"] == "managed";
                let configured = if !managed && configured.is_empty() && shadow.is_empty() { environment_value("CODEX_HOME").unwrap_or(configured) } else { configured };
                ("codex", if configured.is_empty() { home.join(".codex") } else { expand(&configured) }.join("sessions"))
            } else {
                let configured = text("homePath");
                let base = if configured.is_empty() { environment_value("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude")) } else { expand(&configured) };
                ("claude", base.join("projects"))
            };
            let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
            // Canonical, so aliases of one directory read once.
            let dir = std::fs::canonicalize(&resolved).unwrap_or(resolved);
            if !dirs.iter().any(|(existing, known)| *existing == provider && *known == dir) {
                dirs.push((provider, dir));
            }
        }
    }
    dirs
}

/// Every `.jsonl` under `root` whose mtime is at or after `since_ms`, in directory order.
fn list_transcripts(root: &Path, since_ms: i64) -> Vec<(PathBuf, u64, i64)> {
    let mut found = vec![];
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        // Depth-first in name order: subdirectories are visited where they sort.
        let mut batch = vec![];
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                batch.push((true, path));
            } else if path.extension().is_some_and(|extension| extension == "jsonl") {
                batch.push((false, path));
            }
        }
        for (is_dir, path) in batch.into_iter().rev() {
            if is_dir {
                pending.push(path);
            } else if let Ok(metadata) = std::fs::metadata(&path) {
                let mtime_ms = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |elapsed| elapsed.as_millis() as i64);
                if mtime_ms >= since_ms {
                    found.push((path, metadata.len(), mtime_ms));
                }
            }
        }
    }
    found
}

/// Parses one transcript, de-duplicated within the file (99% of all duplicates).
fn read_transcript(path: &Path, provider: &'static str) -> Option<Vec<UsageRecord>> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut state = CodexScanState::default();
    let mut records = vec![];
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        // Cheap substring gates before `JSON.parse`; the Codex reducer also needs the lines that
        // carry its session, model and service tier.
        let relevant = if provider == "claude" {
            might_carry_usage(text, provider)
        } else {
            might_carry_usage(text, provider) || text.contains("\"turn_context\"") || text.contains("\"thread_settings_applied\"") || text.contains("\"session_meta\"")
        };
        if text.is_empty() || !relevant {
            continue;
        }
        let record = if provider == "claude" { parse_claude_line(text) } else { parse_codex_line(text, &mut state) };
        records.extend(record);
    }
    Some(dedupe_within_file(records, &mut HashSet::new()))
}
