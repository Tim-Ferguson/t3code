//! Usage summaries (`UsageService.ts`): scans the Claude Code and Codex session transcripts the
//! provider CLIs keep on disk, including work driven outside T3 Code, prices the records and folds
//! them into `(day, hour?, provider, model, source)` buckets.
//!
//! Rates come from LiteLLM's published table. The first fetch is cached under the state directory
//! (`usage-model-rates.json`), a day-old table keeps the page working offline, and an explicit
//! refresh ignores the day but not a table fetched in the last minute. `T3CODE_USAGE_RATES_URL`
//! replaces the origin so tests can serve a loopback table.
//!
//! Parsed transcripts are kept in a persisted scan cache (`usage_scan_cache.rs`, `usage-scan-cache-v5.json`) for 90 days: a file that
//! has not changed is not read again, a file that grew is read from where the last parse stopped (`usage_transcript_reader.rs`), and
//! usage of transcripts since deleted stays in the totals. OpenCode and Antigravity are read from their databases
//! (`usage_readers`) on every scan, as the original does. Grok and Cursor are not read.
use crate::{
    usage_aggregation::{Aggregator, Options, resolve_model_aliases},
    usage_pricing::{RateTable, override_table, parse_rate_table},
    usage_scan_cache::{CachedFile, LEGACY_SCAN_CACHE_FILE_NAME, SCAN_CACHE_FILE_NAME, ScanCache, ScanCacheWriter, decode_scan_cache, prune_scan_cache},
    usage_transcript_reader::{directory_volume_id, list_transcript_files, read_transcript_records},
    usage_transcripts::{UsageRecord, dedupe_within_file},
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
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
/// Longest window the UI offers, plus slack. Older cache entries are pruned.
const CACHE_RETENTION_DAYS: i64 = 90;

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

/// The persisted cache and what it needs to be written back.
#[derive(Default)]
struct CacheState {
    loaded: bool,
    files: ScanCache,
    /// `provider NUL directory` to the canonical directory and volume id it was last seen with, so a source keeps its fingerprint
    /// after its root is cleaned up or recreated.
    sources: IndexMap<String, (String, String)>,
    dirty: bool,
    writer: ScanCacheWriter,
}

pub struct UsageService {
    state_dir: PathBuf,
    rates: tokio::sync::Mutex<RatesState>,
    cache: Mutex<CacheState>,
    /// Scans with different windows can finish together; serialising the writes keeps an older snapshot from landing after a newer one.
    persist: Mutex<()>,
    http: reqwest::Client,
}

/// One service per state directory, so the rate table and scan cache outlive a request.
pub fn of(state_dir: &Path) -> Arc<UsageService> {
    static SERVICES: OnceLock<Mutex<HashMap<PathBuf, Arc<UsageService>>>> = OnceLock::new();
    SERVICES.get_or_init(Default::default).lock().unwrap().entry(state_dir.to_path_buf()).or_insert_with(|| UsageService::new(state_dir)).clone()
}

impl UsageService {
    /// A service of its own, which loads the scan cache from `state_dir` as a restarted server would.
    pub fn new(state_dir: &Path) -> Arc<Self> {
        Arc::new(Self { state_dir: state_dir.to_path_buf(), rates: Default::default(), cache: Default::default(), persist: Default::default(), http: reqwest::Client::new() })
    }
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
        let retention_cutoff_ms = started - CACHE_RETENTION_DAYS * 24 * 60 * 60 * 1000;
        let scan = {
            let this = self.clone();
            let settings = settings.clone();
            tokio::task::spawn_blocking(move || this.scan(&settings, window_start_ms, retention_cutoff_ms))
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
        let shared = shared_codex_sessions(scanned.iter().flat_map(|dir| dir.files.iter().flatten().chain(dir.saved.iter())));
        let host_id = hostname();
        let mut sources = vec![];
        for dir in &scanned {
            let (mut scanned_files, mut skipped_files) = (0u64, 0u64);
            let mut sessions = HashSet::new();
            for file in dir.files.iter().flatten().chain(dir.saved.iter()) {
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
                "status":if missing && scanned_files == 0 { "missing" } else { dir.status },
                "scannedFiles":scanned_files,"skippedFiles":skipped_files,"malformedRecords":0,"distinctSessions":sessions.len(),
                "message":match (dir.message, missing) { (Some(message), _) => json!(message), (None, true) => json!("No transcript directory on this environment."), _ => Value::Null },
            }));
        }
        let aggregated = aggregator.finish();
        Ok(json!({
            "contractVersion":USAGE_CONTRACT_VERSION,"readAt":iso(now_ms()),"timeZone":time_zone,"sinceDay":since_day,"untilDay":until_day,
            "buckets":aggregated.buckets,"sources":sources,"pricing":pricing,"scanDurationMs":(now_ms() - started).max(0),
        }))
    }

    /// Walks each provider's transcript directory and parses the files newer than the window, then adds the saved usage of transcripts
    /// since deleted and writes the cache back.
    fn scan(&self, settings: &Value, window_start_ms: i64, retention_cutoff_ms: i64) -> Vec<ScannedDir> {
        self.ensure_cache_loaded();
        let mut scanned = vec![];
        let mut seen = HashSet::new();
        for (provider, directory) in transcript_dirs(settings) {
            let (dir, volume_id) = self.resolve_source(provider, &directory, retention_cutoff_ms as f64);
            if !seen.insert((provider, dir.clone())) {
                continue;
            }
            if !dir.is_dir() {
                scanned.push(ScannedDir { provider, dir: dir.to_string_lossy().into_owned(), volume_id, files: None, saved: vec![], status: "ok", message: None });
                continue;
            }
            let mut parsed = vec![];
            let mut updates = vec![];
            for file in list_transcript_files(&dir, window_start_ms as f64, None) {
                let path = file.path.to_string_lossy().into_owned();
                let (records, update) = self.file_records(&path, file.size, file.mtime_ms, provider);
                updates.extend(update.map(|update| (path.clone(), update)));
                parsed.push((path, records));
            }
            // Stored in walk order: saved records of deleted transcripts aggregate in cache order, where the first copy of a duplicate wins.
            self.store(updates);
            scanned.push(ScannedDir { provider, dir: dir.to_string_lossy().into_owned(), volume_id, files: Some(parsed), saved: vec![], status: "ok", message: None });
        }
        scanned.extend(self.scan_opencode(window_start_ms));
        scanned.extend(self.scan_antigravity(settings, window_start_ms));
        // Cleanup may remove transcripts, but the usage already saved still contributes to its source. Like the walk, files last written
        // before the window are skipped: they cannot hold records inside it.
        let retained_since = (window_start_ms.max(retention_cutoff_ms)) as f64;
        {
            let cache = self.cache.lock().unwrap();
            for dir in &mut scanned {
                let root = Path::new(&dir.dir);
                let live: HashSet<&str> = dir.files.iter().flatten().map(|(path, _)| path.as_str()).collect();
                let saved = cache.files.iter().filter(|(path, entry)| entry.provider == dir.provider && entry.mtime_ms >= retained_since && !live.contains(path.as_str()) && Path::new(path.as_str()).starts_with(root)).map(|(path, entry)| (path.clone(), Arc::new(entry.all_records()))).collect();
                dir.saved = saved;
            }
        }
        {
            let mut cache = self.cache.lock().unwrap();
            if prune_scan_cache(&mut cache.files, retention_cutoff_ms as f64) > 0 {
                cache.dirty = true;
            }
        }
        self.persist_cache();
        scanned
    }

    /// Loads the persisted cache once per process, from its own file or else the legacy one (rewritten to its own on the next write).
    fn ensure_cache_loaded(&self) {
        let mut cache = self.cache.lock().unwrap();
        if cache.loaded {
            return;
        }
        cache.loaded = true;
        let read = |name: &str| std::fs::read_to_string(self.state_dir.join(name)).ok().and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        let document = match read(SCAN_CACHE_FILE_NAME) {
            Some(document) => Some(document),
            None => {
                let legacy = read(LEGACY_SCAN_CACHE_FILE_NAME);
                cache.dirty = legacy.is_some();
                legacy
            }
        };
        let Some(document) = document else { return };
        cache.files = decode_scan_cache(&document);
        // All or nothing, as the original's decoder of the sources.
        let sources: Option<Vec<(String, (String, String))>> = document["sources"].as_object().and_then(|sources| sources.iter().map(|(key, source)| Some((key.clone(), (source["dir"].as_str()?.to_owned(), source["volumeId"].as_str()?.to_owned())))).collect());
        cache.sources.extend(sources.into_iter().flatten());
    }

    /// The canonical directory and volume id of a transcript source. Both stay as they were last seen after the root is cleaned up
    /// (its aliases and the summaries a client merged from before the cleanup keep matching), and a recreated directory still reports
    /// the retained history under its old identity.
    fn resolve_source(&self, provider: &'static str, directory: &Path, retention_cutoff_ms: f64) -> (PathBuf, String) {
        let key = format!("{provider}\0{}", directory.to_string_lossy());
        let mut cache = self.cache.lock().unwrap();
        let previous = cache.sources.get(&key).cloned();
        let dir = std::fs::canonicalize(directory).unwrap_or_else(|_| previous.as_ref().map_or_else(|| directory.to_path_buf(), |(dir, _)| PathBuf::from(dir)));
        let current_volume = directory_volume_id(&dir);
        let has_retained_history = cache.files.iter().any(|(path, entry)| entry.provider == provider && entry.mtime_ms >= retention_cutoff_ms && !(entry.records.is_empty() && entry.tail_records.is_empty()) && Path::new(path.as_str()).starts_with(&dir));
        let dir_text = dir.to_string_lossy().into_owned();
        let volume_id = match &previous {
            Some((previous_dir, previous_volume)) if *previous_dir == dir_text && (has_retained_history || current_volume.is_empty()) => if previous_volume.is_empty() { current_volume } else { previous_volume.clone() },
            _ => current_volume,
        };
        if previous.as_ref().is_none_or(|(previous_dir, previous_volume)| *previous_dir != dir_text || *previous_volume != volume_id) {
            cache.sources.insert(key, (dir_text, volume_id.clone()));
            cache.dirty = true;
        }
        (dir, volume_id)
    }

    /// Applies fresh parses. A scan of another window may have cached its own read of a file meanwhile; then whichever read saw the
    /// later file stays, so a slower scan never replaces newer usage with older.
    fn store(&self, updates: Vec<(String, (Arc<CachedFile>, Option<Arc<CachedFile>>))>) {
        let mut cache = self.cache.lock().unwrap();
        for (path, (entry, replaces)) in updates {
            let keep_current = match cache.files.get(&path) {
                None => false,
                Some(current) => !replaces.as_ref().is_some_and(|replaces| Arc::ptr_eq(current, replaces)) && current.is_later_read_than(&entry),
            };
            if !keep_current {
                cache.files.insert(path, entry);
                cache.dirty = true;
            }
        }
    }

    /// Writes the cache when it changed. The flag is cleared before encoding, so a scan that changes the cache meanwhile marks it dirty
    /// again; a failed write restores it, and the next scan retries. A cache that cannot be written is a slower next start, not a failed read.
    fn persist_cache(&self) {
        let _persisting = self.persist.lock().unwrap();
        let contents = {
            let mut cache = self.cache.lock().unwrap();
            if !cache.dirty {
                return;
            }
            cache.dirty = false;
            let sources: Map<String, Value> = cache.sources.iter().map(|(key, (dir, volume_id))| (key.clone(), json!({"dir":dir,"volumeId":volume_id}))).collect();
            let mut extra = Map::new();
            extra.insert("sources".into(), Value::Object(sources));
            let CacheState { files, writer, .. } = &mut *cache;
            writer.write(files, &extra)
        };
        let path = self.state_dir.join(SCAN_CACHE_FILE_NAME);
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let written = std::fs::create_dir_all(&self.state_dir).and_then(|()| std::fs::write(&temporary, contents)).and_then(|()| std::fs::rename(&temporary, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
            self.cache.lock().unwrap().dirty = true;
        }
    }

    /// OpenCode's data directories: `OPENCODE_DATA_DIR` (a comma list), else `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode`.
    fn scan_opencode(&self, window_start_ms: i64) -> Vec<ScannedDir> {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let data_home = std::env::var("XDG_DATA_HOME").ok().map(|value| value.trim().to_owned()).filter(|value| Path::new(value).is_absolute());
        let default = data_home.map_or_else(|| home.join(".local").join("share"), PathBuf::from).join("opencode");
        env_roots("OPENCODE_DATA_DIR", vec![default], &home)
            .into_iter()
            .map(|dir| {
                let read = crate::usage_readers::read_opencode_usage(&dir, window_start_ms);
                ScannedDir {
                    provider: "opencode",
                    volume_id: volume_id(&dir),
                    saved: vec![],
                    files: if read.missing && !read.error { None } else { Some(read.files.into_iter().map(|(path, records)| (path.to_string_lossy().into_owned(), Arc::new(records))).collect()) },
                    dir: dir.to_string_lossy().into_owned(),
                    status: if read.error { "partial" } else { "ok" },
                    message: read.error.then_some("Some OpenCode history could not be read."),
                }
            })
            .collect()
    }

    /// Antigravity's conversation directories: `ANTIGRAVITY_DATA_DIR`, else the CLI and IDE defaults, and the profile of every
    /// Antigravity instance in the settings. A store is read once however many names it has.
    fn scan_antigravity(&self, settings: &Value, window_start_ms: i64) -> Vec<ScannedDir> {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let defaults = ["antigravity", "antigravity-cli", "antigravity-ide", "antigravity-backup"].iter().map(|name| home.join(".gemini").join(name)).chain(std::iter::once(home.join(".config").join("antigravity"))).collect();
        let mut roots = env_roots_uncanonical("ANTIGRAVITY_DATA_DIR", defaults, &home);
        for (instance_id, instance) in settings["providerInstances"].as_object().into_iter().flatten() {
            if instance["driver"] == "antigravity" {
                roots.push(crate::antigravity_acp::instance_directories(&self.state_dir, instance_id).0.join("antigravity-acp"));
            }
        }
        let mut directories: Vec<PathBuf> = vec![];
        for root in roots {
            let resolved = std::fs::canonicalize(&root).unwrap_or(root);
            let nested = resolved.join("conversations");
            let dir = if nested.exists() { nested } else { resolved };
            let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
            if !directories.contains(&dir) {
                directories.push(dir);
            }
        }
        let read = crate::usage_readers::read_antigravity_usage(&directories, window_start_ms);
        directories
            .into_iter()
            .map(|dir| {
                let failed = read.errors.iter().any(|error| error.starts_with(&dir));
                let files = read.files.iter().filter(|file| file.root == dir).map(|file| (file.path.to_string_lossy().into_owned(), Arc::new(file.records.clone()))).collect();
                ScannedDir {
                    provider: "antigravity",
                    volume_id: volume_id(&dir),
                    saved: vec![],
                    files: if !dir.exists() && !failed { None } else { Some(files) },
                    dir: dir.to_string_lossy().into_owned(),
                    status: if failed { "partial" } else { "ok" },
                    message: failed.then_some("Some Antigravity history could not be read."),
                }
            })
            .collect()
    }

    /// One transcript's records, reusing the cached parse when the file is unchanged. A file that only grew re-parses from the cached
    /// position, so an actively written rollout costs its appended bytes per scan; the reader verifies the position's guard bytes and
    /// restarts from byte 0 when they no longer match. A fresh parse comes back as an update for the caller to store, with the entry it
    /// was built from.
    #[allow(clippy::type_complexity)]
    fn file_records(&self, path: &str, size: u64, mtime_ms: f64, provider: &'static str) -> (Arc<Vec<UsageRecord>>, Option<(Arc<CachedFile>, Option<Arc<CachedFile>>)>) {
        let cached = self.cache.lock().unwrap().files.get(path).cloned();
        // Provider is part of the identity: if both providers were ever pointed at one directory, a hit parsed by the other parser
        // must not be reused.
        if let Some(cached) = cached.as_ref().filter(|cached| cached.size == size as i64 && cached.mtime_ms == mtime_ms && cached.provider == provider) {
            return (Arc::new(cached.all_records()), None);
        }
        // Only a strictly grown file may resume. The same size with a new mtime, or a shrunken file, is rewritten content.
        let resume_from = cached.as_ref().filter(|cached| cached.provider == provider && size as i64 > cached.size).map(|cached| &cached.position);
        let Some(parsed) = read_transcript_records(Path::new(path), provider, resume_from) else {
            // A read failure is not an empty transcript: keeping it under this (size, mtime) would drop the file's usage until it changes.
            return (Arc::new(cached.filter(|cached| cached.provider == provider).map_or_else(Vec::new, |cached| cached.all_records())), None);
        };
        // Stored already de-duplicated within the file, which is 99% of all duplicates; the aggregator still runs the cross-file pass.
        // One seen set spans the cached base, the new lines and the tail, so a resumed parse dedupes exactly like a full one.
        let base = match (&cached, parsed.resumed) {
            (Some(cached), true) => cached.records.clone(),
            _ => vec![],
        };
        let mut seen = HashSet::new();
        let mut all = base;
        all.extend(parsed.records);
        let records = dedupe_within_file(all, &mut seen);
        let tail_records = dedupe_within_file(parsed.tail_records, &mut seen);
        let entry = Arc::new(CachedFile::new(size as i64, mtime_ms, provider, records, tail_records, parsed.position));
        (Arc::new(entry.all_records()), Some((entry, cached)))
    }
}

struct ScannedDir {
    provider: &'static str,
    dir: String,
    volume_id: String,
    /// Parsed records per file, `None` when the directory does not exist.
    files: Option<Vec<(String, Arc<Vec<UsageRecord>>)>>,
    /// Records saved from transcripts since deleted (or outside this scan's walk), in cache order.
    saved: Vec<(String, Arc<Vec<UsageRecord>>)>,
    /// `partial` when some of the history could not be read; the source then says so.
    status: &'static str,
    message: Option<&'static str>,
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

/// `envRoots`: the directories a comma-separated variable names (default when it names none), with `~` expanded and each resolved to its
/// real path, once however many names it has.
fn env_roots(key: &str, defaults: Vec<PathBuf>, home: &Path) -> Vec<PathBuf> {
    let mut canonical: Vec<PathBuf> = vec![];
    for root in env_roots_uncanonical(key, defaults, home) {
        let resolved = std::fs::canonicalize(&root).unwrap_or(root);
        if !canonical.contains(&resolved) {
            canonical.push(resolved);
        }
    }
    canonical
}

fn env_roots_uncanonical(key: &str, defaults: Vec<PathBuf>, home: &Path) -> Vec<PathBuf> {
    let named: Vec<PathBuf> = std::env::var(key).ok().into_iter().flat_map(|value| value.split(',').map(|part| part.trim().to_owned()).filter(|part| !part.is_empty()).collect::<Vec<_>>()).map(|part| std::path::absolute(crate::claude_skills::expand_home(&part, home)).unwrap_or_default()).collect();
    if named.is_empty() { defaults.into_iter().map(|root| std::path::absolute(&root).unwrap_or(root)).collect() } else { named }
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

/// The transcript directory of every Claude Code and Codex instance, as configured. Disabled accounts still have
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
            // Aliases of one directory are read once, once the service has made it canonical (`resolve_source`).
            dirs.push((provider, std::path::absolute(&resolved).unwrap_or(resolved)));
        }
    }
    dirs
}
