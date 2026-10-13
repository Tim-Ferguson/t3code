//! `agentSessions.scan` and `agentSessions.import` for Claude Code and Codex history
//! (`AgentSessionScanner.ts`, `AgentSessionImporter.ts`).
//!
//! Scanning finds the directories the agents have run in, from `<config dir>/projects/*/*.jsonl` for
//! Claude Code and `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl` for Codex, and offers each as a
//! project; importing turns a project's recent sessions into T3 Code threads that resume the native
//! session on their first message. The agents' homes are read, never written, and every read is
//! bounded as in the original: file counts, directory operations, bytes and records per source and
//! per transcript, and a selected-history budget per imported transcript. A transcript is streamed
//! with only the fields the import reads kept (`agent_session_json.rs`), because a Codex rollout can
//! be several GiB.
//!
//! An import reads at most 100 transcripts, so a project with more recent sessions is imported over
//! repeated calls, as in the original. Each transcript an import has read is recorded with the identity of
//! the file as it was read (`rust_imported_transcripts`, the original's `importedTranscripts` in the
//! thread's runtime row): a later call that finds the same file unchanged counts it as imported without
//! reading it or spending any of its budget, so the budget goes to the next transcripts. A file whose
//! identity changed is read again, and a transcript that was read but skipped is recorded nowhere, so it
//! spends its share again on every call. The record of a new thread is written in the transaction that
//! writes its events (the original writes it afterwards, in a transaction of its own), so there is no
//! thread without its record, only a record-less thread from a build that kept none, which the next
//! call reads once and records. A thread is never rewritten once it exists.
use crate::{
    agent_session_json::{LimitError, ReadError, RecordReader, Seg, TRANSCRIPT_RECORD},
    persistence::Store,
    transport::ApiState,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    path::{Path, PathBuf},
};

pub const METHODS: [&str; 2] = ["agentSessions.scan", "agentSessions.import"];

/// Chunk size for metadata reads: small reads keep long headers from eating the budget.
const METADATA_READ_BYTES: usize = 8 * 1024;
/// Prevent malformed transcripts from turning project discovery into a full file scan.
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 1024 * 1024;
const MAX_TRANSCRIPTS_PER_SOURCE: usize = 5000;
const MAX_DISCOVERY_OPERATIONS_PER_SOURCE: usize = MAX_TRANSCRIPTS_PER_SOURCE * 4;
const MAX_METADATA_BYTES_PER_SOURCE: u64 = 64 * 1024 * 1024;
const MAX_METADATA_OPERATIONS_PER_SOURCE: usize = MAX_TRANSCRIPTS_PER_SOURCE * 4;
const MAX_METADATA_RECORDS_PER_SOURCE: usize = 100_000;
const MAX_METADATA_RECORDS_PER_TRANSCRIPT: usize = 1_000;
const RECENT_THREAD_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_IMPORTED_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_IMPORTED_MESSAGES: usize = 200;
const MAX_IMPORT_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_IMPORT_TRANSCRIPTS: usize = 100;
const MAX_IMPORT_RECORDS: usize = 100_000;
/// `DEFAULT_MODEL_BY_PROVIDER`: what an imported thread selects when its transcript names no model.
const DEFAULT_CLAUDE_MODEL: &str = "claude-fable-5-1";
const DEFAULT_CODEX_MODEL: &str = "gpt-6-astra";
/// `DEFAULT_RUNTIME_MODE`.
const DEFAULT_RUNTIME_MODE: &str = "full-access";

fn scan_error(operation: &str, cause: impl std::fmt::Display) -> Value {
    json!({"_tag":"AgentSessionScanError","operation":operation,"cause":cause.to_string()})
}

/// What the scan needs from the host. Injectable so tests read a temp directory.
pub struct Host {
    pub home: PathBuf,
    /// This server's T3 home and its worktrees: sessions started inside them are the app's own.
    pub base_dir: PathBuf,
    pub temp_dirs: Vec<PathBuf>,
    /// The process environment's `CLAUDE_CONFIG_DIR` and `CODEX_HOME` (`HostProcessEnvironment`).
    pub environment: HashMap<String, String>,
}
impl Host {
    pub fn of(state_dir: Option<&Path>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let base_dir = state_dir.map(|dir| if dir.file_name().is_some_and(|name| name == "userdata") { dir.parent().unwrap_or(dir).to_path_buf() } else { dir.to_path_buf() }).unwrap_or_default();
        let environment = ["CLAUDE_CONFIG_DIR", "CODEX_HOME"].into_iter().filter_map(|name| Some((name.to_owned(), std::env::var(name).ok()?))).collect();
        Self { home, base_dir, temp_dirs: vec![std::env::temp_dir(), PathBuf::from("/tmp"), PathBuf::from("/private/tmp")], environment }
    }
}

/// `AgentSessionSource`: the agents whose history can be imported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Claude,
    Codex,
}
impl Source {
    const ALL: [Source; 2] = [Source::Claude, Source::Codex];
    /// The provider driver the source's sessions belong to.
    fn driver(self) -> &'static str {
        match self {
            Self::Claude => "claudeAgent",
            Self::Codex => "codex",
        }
    }
    fn home_variable(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }
}

/// `normalizeProjectPathForComparison`: separators and a trailing slash do not distinguish projects.
fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let trimmed = text.trim_end_matches('/');
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    if cfg!(windows) { trimmed.to_lowercase() } else { trimmed.to_owned() }
}
fn with_separator(path: &str) -> String {
    format!("{}/", path.replace('\\', "/"))
}
fn is_excluded(host: &Host, candidate: &Path) -> bool {
    let normalized = normalize(candidate);
    if [host.home.as_path()].into_iter().chain(host.temp_dirs.iter().map(PathBuf::as_path)).any(|root| !root.as_os_str().is_empty() && normalize(root) == normalized) {
        return true;
    }
    let spelled = with_separator(&candidate.to_string_lossy());
    let ancestors = [host.home.join("Downloads"), host.home.join("Documents").join("Codex")];
    ancestors.iter().any(|ancestor| spelled.starts_with(&with_separator(&ancestor.to_string_lossy())))
        || (!host.base_dir.as_os_str().is_empty() && spelled.starts_with(&with_separator(&host.base_dir.to_string_lossy())))
        || spelled.contains("/.t3/worktrees/")
}

#[derive(Clone)]
struct Transcript {
    path: PathBuf,
    mtime_ms: i64,
    size: u64,
    instance: String,
}
struct Budget {
    bytes: u64,
    operations: usize,
    records: usize,
    truncated: bool,
}
struct Candidate {
    cwd: String,
    source: Source,
    instance: String,
    thread_count: usize,
    last_active_ms: i64,
    transcripts: Vec<(PathBuf, i64)>,
}

/// `resolveProviderInstanceEnabled`: an explicit false on the envelope or in the config wins; otherwise the
/// envelope, then the config, then the driver's default (every built-in driver defaults to enabled).
fn instance_enabled(instance: &Value, config: &Value) -> bool {
    let config_enabled = config.get("enabled").and_then(Value::as_bool);
    let envelope = instance.get("enabled").and_then(Value::as_bool);
    if envelope == Some(false) || config_enabled == Some(false) {
        return false;
    }
    envelope.or(config_enabled).unwrap_or(true)
}

/// `decodeClaudeSettings` / `decodeCodexSettings` for the fields the scan reads: `None` when the
/// instance's config does not decode, so the instance is left out. A path is trimmed, and absent is empty.
fn decode_paths<const N: usize>(config: &Value, fields: [&str; N]) -> Option<[String; N]> {
    let config = if config.is_null() { &Value::Object(Default::default()) } else { config };
    let object = config.as_object()?;
    if object.get("enabled").is_some_and(|enabled| !enabled.is_boolean()) {
        return None;
    }
    let mut paths = fields.map(|_| String::new());
    for (index, field) in fields.iter().enumerate() {
        match object.get(*field) {
            None => {}
            Some(Value::String(text)) => paths[index] = js_trim(text).to_owned(),
            Some(_) => return None,
        }
    }
    Some(paths)
}

/// The enabled instances of a source and the home each reads, the built-in instance first: a home that
/// several instances share is scanned once, for the first of them (`collectCandidates`).
fn source_homes(source: Source, settings: &Value, host: &Host) -> Vec<(String, PathBuf)> {
    let driver = source.driver();
    let instances = settings["providerInstances"].as_object().cloned().unwrap_or_default();
    // (instance id, config, environment)
    let mut entries: Vec<(String, Value, Value)> = instances.iter().filter(|(_, instance)| instance["driver"] == driver && instance_enabled(instance, &instance["config"])).map(|(id, instance)| (id.clone(), instance["config"].clone(), instance["environment"].clone())).collect();
    if !instances.contains_key(driver) {
        // Settings that predate provider instances: the built-in instance is configured under `providers`.
        let legacy = settings["providers"][driver].clone();
        if instance_enabled(&Value::Null, &legacy) {
            entries.push((driver.into(), legacy, Value::Null));
        }
    }
    entries.sort_by_key(|(id, _, _)| id != driver);
    let mut seen = HashSet::new();
    let mut homes = vec![];
    for (id, config, environment) in entries {
        let variable = source.home_variable();
        let from_environment = environment
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|entry| entry["name"] == variable)
            .and_then(|entry| entry["value"].as_str().map(str::to_owned))
            .or_else(|| host.environment.get(variable).cloned());
        let home = match source {
            Source::Claude => {
                let Some([configured]) = decode_paths(&config, ["homePath"]) else { continue };
                resolve_claude_config_dir(&configured, from_environment.as_deref(), host)
            }
            Source::Codex => {
                let Some([mut home_path, shadow_home_path]) = decode_paths(&config, ["homePath", "shadowHomePath"]) else { continue };
                if home_path.is_empty() && shadow_home_path.is_empty() && from_environment.as_deref().is_some_and(|value| !js_trim(value).is_empty()) {
                    home_path = from_environment.clone().unwrap_or_default();
                }
                let environment = HashMap::from([("HOME".to_owned(), host.home.to_string_lossy().into_owned())]);
                normalize_path(&crate::codex_home::resolve(&home_path, &shadow_home_path, &environment).shared)
            }
        };
        if seen.insert(directory_identity(&home)) {
            homes.push((id, home));
        }
    }
    homes
}

/// `resolveClaudeConfigDir`: the instance's `homePath` (exported as `CLAUDE_CONFIG_DIR`), then a
/// `CLAUDE_CONFIG_DIR` already in the environment, then `~/.claude`.
fn resolve_claude_config_dir(home_path: &str, environment_home: Option<&str>, host: &Host) -> PathBuf {
    let configured = js_trim(home_path);
    let chosen = if !configured.is_empty() { configured } else { environment_home.map(js_trim).unwrap_or("") };
    if chosen.is_empty() {
        return host.home.join(".claude");
    }
    let expanded = crate::claude_skills::expand_home(chosen, &host.home);
    normalize_path(&std::path::absolute(&expanded).unwrap_or(expanded))
}

/// `String.prototype.trim`: ECMAScript white space and line terminators, which is not quite Unicode's.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|character: char| matches!(character, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

/// Matches directory aliases without assuming the host volume is case-insensitive.
#[cfg(unix)]
fn directory_identity(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.ino() > 0 => format!("inode:{}:{}", metadata.dev(), metadata.ino()),
        _ => format!("path:{}", normalize(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))),
    }
}
#[cfg(not(unix))]
fn directory_identity(path: &Path) -> String {
    format!("path:{}", normalize(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())))
}

fn time_ms(time: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    time.ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_millis() as i64)
}
fn mtime_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    time_ms(metadata.modified())
}

/// The discovery operations a home may spend: each directory read and each stat takes one, and once they
/// run out the walk stops and the scan reports itself truncated.
struct Operations {
    remaining: usize,
    truncated: bool,
}
impl Operations {
    /// Spends one operation, or notes that the budget ran out.
    fn spend(&mut self) -> bool {
        if self.remaining == 0 {
            self.truncated = true;
            return false;
        }
        self.remaining -= 1;
        true
    }
    /// A directory's entries, in the order the filesystem gives them.
    fn list(&mut self, directory: &Path) -> Vec<String> {
        if !self.spend() {
            return vec![];
        }
        std::fs::read_dir(directory).map(|entries| entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default()
    }
    /// Names in reverse order: date-partitioned directories sort chronologically, so walking them backwards
    /// spends each home's share of the budget on recent sessions.
    fn list_newest_first(&mut self, directory: &Path) -> Vec<String> {
        let mut names = self.list(directory);
        names.sort();
        names.reverse();
        names
    }
    /// One stat of a file: its transcript, if it is a file with a modification time.
    fn transcript(&mut self, path: PathBuf, instance: &str) -> Option<Transcript> {
        let metadata = std::fs::metadata(&path).ok()?;
        let mtime_ms = mtime_ms(&metadata).filter(|_| metadata.is_file())?;
        Some(Transcript { path, mtime_ms, size: metadata.len(), instance: instance.to_owned() })
    }
}

/// `discoverClaudeTranscripts`: `projects/<dir>/*.jsonl` files, within an operation budget.
fn discover_claude(home: &Path, instance: &str, budget: usize) -> (Vec<Transcript>, bool) {
    let projects = home.join("projects");
    let mut operations = Operations { remaining: budget, truncated: false };
    let mut transcripts = vec![];
    for project in operations.list(&projects) {
        if operations.remaining == 0 {
            operations.truncated = true;
            break;
        }
        let directory = projects.join(&project);
        for entry in operations.list(&directory).into_iter().filter(|entry| entry.ends_with(".jsonl")) {
            if !operations.spend() {
                break;
            }
            transcripts.extend(operations.transcript(directory.join(entry), instance));
        }
    }
    (transcripts, operations.truncated)
}

/// `discoverCodexTranscripts`: `sessions/YYYY/MM/DD/rollout-*.jsonl` files, newest date first, within an
/// operation budget.
fn discover_codex(home: &Path, instance: &str, budget: usize) -> (Vec<Transcript>, bool) {
    let sessions = home.join("sessions");
    let mut operations = Operations { remaining: budget, truncated: false };
    let mut transcripts = vec![];
    for year in operations.list_newest_first(&sessions) {
        if operations.remaining == 0 {
            operations.truncated = true;
            break;
        }
        for month in operations.list_newest_first(&sessions.join(&year)) {
            if operations.remaining == 0 {
                operations.truncated = true;
                break;
            }
            for day in operations.list_newest_first(&sessions.join(&year).join(&month)) {
                if operations.remaining == 0 {
                    operations.truncated = true;
                    break;
                }
                let directory = sessions.join(&year).join(&month).join(&day);
                for entry in operations.list_newest_first(&directory) {
                    if !entry.starts_with("rollout-") || !entry.ends_with(".jsonl") {
                        continue;
                    }
                    if !operations.spend() {
                        break;
                    }
                    transcripts.extend(operations.transcript(directory.join(entry), instance));
                }
            }
        }
    }
    (transcripts, operations.truncated)
}

/// `selectMetadataTranscripts`: each account takes a turn before another file from the same home.
fn select_metadata_transcripts(transcripts: Vec<Transcript>) -> Vec<Transcript> {
    let mut groups: Vec<std::vec::IntoIter<Transcript>> = vec![];
    let mut order: Vec<String> = vec![];
    let mut by_instance: HashMap<String, Vec<Transcript>> = HashMap::new();
    for transcript in transcripts {
        if !by_instance.contains_key(&transcript.instance) {
            order.push(transcript.instance.clone());
        }
        by_instance.entry(transcript.instance.clone()).or_default().push(transcript);
    }
    for instance in order {
        groups.push(by_instance.remove(&instance).unwrap_or_default().into_iter());
    }
    let mut selected = vec![];
    while !groups.is_empty() && selected.len() < MAX_TRANSCRIPTS_PER_SOURCE {
        let mut next = vec![];
        for mut group in groups {
            if selected.len() == MAX_TRANSCRIPTS_PER_SOURCE {
                break;
            }
            if let Some(transcript) = group.next() {
                selected.push(transcript);
                next.push(group);
            }
        }
        groups = next;
    }
    selected
}

/// `extractCwd`: the working directory a record names, at the top level or under `payload`.
fn extract_cwd(line: &str) -> Option<String> {
    let record: Value = serde_json::from_str(line).ok()?;
    if !record.is_object() {
        return None;
    }
    [&record["cwd"], &record["payload"]["cwd"]].into_iter().find_map(|cwd| cwd.as_str().filter(|cwd| !cwd.trim().is_empty()).map(str::to_owned))
}

/// `readCwd`: bounded chunks until a complete record names its cwd or the safety budget ends.
fn read_cwd(transcript: &Transcript, budget: &mut Budget) -> Option<String> {
    if transcript.size == 0 {
        return None;
    }
    if budget.bytes == 0 || budget.operations < 2 || budget.records == 0 {
        budget.truncated = true;
        return None;
    }
    budget.operations -= 1;
    let mut file = std::fs::File::open(&transcript.path).ok()?;
    let max_bytes = MAX_TRANSCRIPT_SCAN_BYTES.min(transcript.size);
    let (mut read_total, mut records_read) = (0u64, 0usize);
    let mut remaining = Vec::<u8>::new();
    let reserve = |budget: &mut Budget, records_read: &mut usize| {
        if *records_read == MAX_METADATA_RECORDS_PER_TRANSCRIPT || budget.records == 0 {
            budget.truncated = true;
            return false;
        }
        *records_read += 1;
        budget.records -= 1;
        true
    };
    while read_total < max_bytes {
        if budget.bytes == 0 || budget.operations == 0 {
            budget.truncated = true;
            return None;
        }
        let size = (METADATA_READ_BYTES as u64).min(max_bytes - read_total).min(budget.bytes) as usize;
        budget.operations -= 1;
        budget.bytes -= size as u64;
        let mut chunk = vec![0u8; size];
        let count = file.read(&mut chunk).ok()?;
        if count == 0 {
            let last = String::from_utf8_lossy(&remaining).trim().to_owned();
            return if last.is_empty() || !reserve(budget, &mut records_read) { None } else { extract_cwd(&last) };
        }
        read_total += count as u64;
        remaining.extend_from_slice(&chunk[..count]);
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = remaining.drain(..=newline).collect();
            if !reserve(budget, &mut records_read) {
                return None;
            }
            if let Some(cwd) = extract_cwd(String::from_utf8_lossy(&line).trim()) {
                return Some(cwd);
            }
        }
    }
    if read_total < transcript.size {
        budget.truncated = true;
        return None;
    }
    let last = String::from_utf8_lossy(&remaining).trim().to_owned();
    if last.is_empty() || !reserve(budget, &mut records_read) { None } else { extract_cwd(&last) }
}

/// `collectCandidates`: every directory the agents have run in, plus whether a bound cut the search short.
/// Each source has its own homes, discovery budget and metadata budget.
fn collect(settings: &Value, host: &Host) -> (Vec<Candidate>, bool) {
    let mut groups: Vec<Candidate> = vec![];
    let mut truncated = false;
    for source in Source::ALL {
        let homes = source_homes(source, settings, host);
        let mut transcripts = vec![];
        let base = MAX_DISCOVERY_OPERATIONS_PER_SOURCE / homes.len().max(1);
        let extra = MAX_DISCOVERY_OPERATIONS_PER_SOURCE % homes.len().max(1);
        for (index, (instance, home)) in homes.iter().enumerate() {
            let budget = base + usize::from(index < extra);
            if budget == 0 {
                truncated = true;
                continue;
            }
            let (found, cut) = match source {
                Source::Claude => discover_claude(home, instance, budget),
                Source::Codex => discover_codex(home, instance, budget),
            };
            truncated |= cut;
            transcripts.extend(found);
        }
        transcripts.sort_by(|left, right| right.mtime_ms.cmp(&left.mtime_ms).then_with(|| left.path.cmp(&right.path)));
        if transcripts.len() > MAX_TRANSCRIPTS_PER_SOURCE {
            truncated = true;
        }
        // Give each account a turn before taking another file from the same home.
        let selected = select_metadata_transcripts(transcripts);
        let mut budget = Budget { bytes: MAX_METADATA_BYTES_PER_SOURCE, operations: MAX_METADATA_OPERATIONS_PER_SOURCE, records: MAX_METADATA_RECORDS_PER_SOURCE, truncated: false };
        let first_of_source = groups.len();
        for transcript in &selected {
            let Some(cwd) = read_cwd(transcript, &mut budget) else { continue };
            match groups[first_of_source..].iter_mut().find(|group| group.instance == transcript.instance && group.cwd == cwd) {
                Some(group) => {
                    group.last_active_ms = group.last_active_ms.max(transcript.mtime_ms);
                    group.thread_count += 1;
                    group.transcripts.push((transcript.path.clone(), transcript.mtime_ms));
                }
                None => groups.push(Candidate { cwd, source, instance: transcript.instance.clone(), thread_count: 1, last_active_ms: transcript.mtime_ms, transcripts: vec![(transcript.path.clone(), transcript.mtime_ms)] }),
            }
        }
        truncated |= budget.truncated;
    }
    (groups, truncated)
}

// --- git identity without spawning git -------------------------------------------------------

enum GitIdentity {
    Repository(Value),
    Worktree,
    NotGit,
}
fn read_git_identity(directory: &Path) -> GitIdentity {
    let git_path = directory.join(".git");
    let Ok(metadata) = std::fs::metadata(&git_path) else { return GitIdentity::NotGit };
    let mut git_dir = git_path.clone();
    if !metadata.is_dir() {
        let pointer = std::fs::read_to_string(&git_path).unwrap_or_default();
        let Some(target) = pointer.lines().find_map(|line| line.strip_prefix("gitdir:")).map(str::trim).filter(|target| !target.is_empty()) else { return GitIdentity::NotGit };
        git_dir = directory.join(target);
        let spelled = git_dir.to_string_lossy().into_owned();
        static WORKTREE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        if WORKTREE.get_or_init(|| regex::Regex::new(r"[\\/]worktrees[\\/][^\\/]+[\\/]?$").unwrap()).is_match(&spelled) {
            return GitIdentity::Worktree;
        }
    }
    let config = std::fs::read_to_string(git_dir.join("config")).unwrap_or_default();
    let origin = parse_origin_url(&config);
    GitIdentity::Repository(json!({"remoteKey":origin.as_deref().map(normalize_remote_url),"repository":origin.as_deref().and_then(github_repository)}))
}
fn parse_config_value(raw: &str) -> String {
    let (mut out, mut quoted) = (String::new(), false);
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(next) = characters.next() {
                out.push(next);
                continue;
            }
        }
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if !quoted && (character == '#' || character == ';') {
            break;
        }
        out.push(character);
    }
    out.trim().to_owned()
}
/// `parseOriginUrlFromGitConfig`: `remote.origin.url`, else the first remote's.
fn parse_origin_url(config: &str) -> Option<String> {
    static HEADER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let header = HEADER.get_or_init(|| regex::Regex::new(r#"(?i)^\[\s*remote(?:\s+"([^"]+)"|\.([^\]\s]+))\s*\](?:\s*[#;].*)?$"#).unwrap());
    let joined = regex::Regex::new(r"\\\r?\n[ \t]*").unwrap().replace_all(config, "").into_owned();
    let (mut section, mut origin, mut first): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    for raw in joined.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(captures) = header.captures(line) {
            section = captures.get(1).map(|name| name.as_str().to_owned()).or_else(|| captures.get(2).map(|name| name.as_str().to_lowercase()));
            continue;
        }
        if line.starts_with('[') {
            section = None;
            continue;
        }
        let Some(section) = &section else { continue };
        let lowered = line.to_lowercase();
        let Some(rest) = lowered.strip_prefix("url") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('=') else { continue };
        let value = parse_config_value(&line[line.len() - rest.len()..]);
        if value.is_empty() {
            continue;
        }
        if section == "origin" { origin.get_or_insert(value); } else { first.get_or_insert(value); }
    }
    origin.or(first)
}
fn azure_key(host: &str, segments: &[&str]) -> Option<String> {
    let (organization, project, repository) = match (host, segments) {
        ("ssh.dev.azure.com", ["v3", organization, project, repository, ..]) => (*organization, *project, *repository),
        ("dev.azure.com", [organization, project, "_git", repository, ..]) => (*organization, *project, *repository),
        (host, [project, "_git", repository, ..]) if host.ends_with(".visualstudio.com") => (host.trim_end_matches(".visualstudio.com"), *project, *repository),
        _ => return None,
    };
    Some(if host == "ssh.dev.azure.com" { format!("dev.azure.com/{organization}/{project}/_git/{repository}") } else { format!("{organization}.visualstudio.com/{project}/_git/{repository}") })
}
/// `normalizeGitRemoteUrl`: a stable comparison key shared by every clone of a repository.
fn normalize_remote_url(value: &str) -> String {
    let mut normalized = value.trim().trim_end_matches('/').to_lowercase();
    if let Some(stripped) = normalized.strip_suffix(".git") {
        normalized = stripped.to_owned();
    }
    if ["ssh://", "http://", "https://", "git://"].iter().any(|scheme| normalized.starts_with(scheme)) {
        match url::Url::parse(&normalized) {
            Ok(url) => {
                let segments: Vec<&str> = url.path().split('/').filter(|segment| !segment.is_empty()).collect();
                if let Some(host) = url.host_str().filter(|_| segments.len() > 1) {
                    return azure_key(host, &segments).unwrap_or_else(|| format!("{host}/{}", segments.join("/")));
                }
            }
            Err(_) => return normalized,
        }
    }
    static SCP: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let scp = SCP.get_or_init(|| regex::Regex::new(r"(?i)^[a-z0-9._-]+@([^:/\s]+):([^/\s]+(?:/[^/\s]+)+)$").unwrap());
    if let Some(captures) = scp.captures(&normalized) {
        let (host, path) = (captures[1].to_owned(), captures[2].to_owned());
        return azure_key(&host, &path.split('/').collect::<Vec<_>>()).unwrap_or_else(|| format!("{host}/{path}"));
    }
    normalized
}
fn github_repository(url: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"(?i)^(?:git@github\.com:|ssh://(?:git@)?github\.com/|https://github\.com/|git://github\.com/)([^/\s]+/[^/\s]+?)(?:\.git)?/?$").unwrap());
    pattern.captures(url.trim()).map(|captures| captures[1].trim().to_owned()).filter(|name| !name.is_empty())
}

// --- scan ------------------------------------------------------------------------------------

fn projects_by_root(store: &Store, host: &Host) -> Result<Vec<(String, String, String)>, Value> {
    let projects = store.read(|connection| Ok(crate::persistence::read_projections(connection, "project"))).map_err(|error| scan_error("read-projects", error))?.map_err(|error| scan_error("read-projects", error))?;
    Ok(projects.iter().filter(|project| project["deletedAt"].is_null()).filter_map(|project| {
        let root = crate::claude_skills::expand_home(project["workspaceRoot"].as_str()?, &host.home);
        let root = std::path::absolute(&root).unwrap_or(root);
        Some((normalize(&root), directory_identity(&root), format!("{}\0{}", project["projectId"].as_str()?, project["workspaceRoot"].as_str()?)))
    }).collect())
}

/// `scan`: the directories the agents have run in, offered as projects, newest first.
pub fn scan(settings: &Value, store: &Store, host: &Host) -> Result<Value, Value> {
    let (raw, truncated) = collect(settings, host);
    struct Merged {
        path: PathBuf,
        sources: Vec<&'static str>,
        thread_count: usize,
        last_active_ms: Option<i64>,
        git: Value,
    }
    let mut merged: Vec<(String, Merged)> = vec![];
    let mut keys: HashMap<PathBuf, String> = HashMap::new();
    let mut git_identities: HashMap<String, Value> = HashMap::new();
    for candidate in &raw {
        let expanded = crate::claude_skills::expand_home(candidate.cwd.trim(), &host.home);
        if !expanded.is_absolute() {
            continue;
        }
        let resolved = normalize_path(&expanded);
        if is_excluded(host, &resolved) {
            continue;
        }
        let key = match keys.get(&resolved) {
            Some(key) => key.clone(),
            None => {
                let key = (|| {
                    // Directories that no longer exist cannot be imported.
                    if !std::fs::metadata(&resolved).is_ok_and(|metadata| metadata.is_dir()) {
                        return String::new();
                    }
                    let real = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
                    // A symlink can point somewhere excluded even when its own spelling does not.
                    if is_excluded(host, &real) {
                        return String::new();
                    }
                    match read_git_identity(&resolved) {
                        GitIdentity::Worktree => String::new(),
                        identity => {
                            let key = directory_identity(&resolved);
                            git_identities.insert(key.clone(), if let GitIdentity::Repository(git) = identity { git } else { Value::Null });
                            key
                        }
                    }
                })();
                keys.insert(resolved.clone(), key.clone());
                key
            }
        };
        if key.is_empty() {
            continue;
        }
        match merged.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, existing)) => {
                if !existing.sources.contains(&candidate.source.driver()) {
                    existing.sources.push(candidate.source.driver());
                }
                existing.thread_count += candidate.thread_count;
                existing.last_active_ms = existing.last_active_ms.max(Some(candidate.last_active_ms));
            }
            None => merged.push((key.clone(), Merged { path: resolved, sources: vec![candidate.source.driver()], thread_count: candidate.thread_count, last_active_ms: Some(candidate.last_active_ms), git: git_identities.get(&key).cloned().unwrap_or(Value::Null) })),
        }
    }
    let imported = projects_by_root(store, host)?;
    let mut candidates = vec![];
    for (key, entry) in &merged {
        let project = imported.iter().find(|(root, identity, _)| *root == normalize(&entry.path) || identity == key);
        let (project_id, root) = match project {
            Some((_, _, joined)) => {
                let (id, root) = joined.split_once('\0').unwrap();
                (Some(id.to_owned()), root.to_owned())
            }
            None => (None, entry.path.to_string_lossy().into_owned()),
        };
        let title = Path::new(&root).file_name().map(|name| name.to_string_lossy().into_owned()).filter(|name| !name.is_empty()).unwrap_or_else(|| root.clone());
        let mut candidate = json!({"path":root,"title":title,"sources":entry.sources,"threadCount":entry.thread_count,"lastActiveAt":entry.last_active_ms.map(iso),"alreadyImported":project_id.is_some(),"git":entry.git});
        if let Some(id) = project_id {
            candidate["projectId"] = json!(id);
        }
        candidates.push(candidate);
    }
    // Newest first, undated candidates last.
    candidates.sort_by(|left, right| match (left["lastActiveAt"].as_str(), right["lastActiveAt"].as_str()) {
        (Some(left_at), Some(right_at)) if left_at == right_at => left["path"].as_str().cmp(&right["path"].as_str()),
        (None, None) => left["path"].as_str().cmp(&right["path"].as_str()),
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(left_at), Some(right_at)) => right_at.cmp(left_at),
    });
    let mut result = json!({"candidates":candidates,"scannedAt":iso(chrono::Utc::now().timestamp_millis())});
    if truncated {
        result["truncated"] = json!(true);
    }
    Ok(result)
}
fn iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}
/// `path.resolve`: absolute, with `.` and `..` folded away.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

// --- import ----------------------------------------------------------------------------------

/// `AgentSessionThreadMessage`.
struct Message {
    role: &'static str,
    text: String,
    created_at: String,
}

/// `AgentSessionThread`: a transcript's visible history.
struct Imported {
    source: Source,
    session_id: String,
    title: String,
    model: Option<String>,
    created_at: String,
    updated_at: String,
    messages: Vec<Message>,
}

/// `extractText`: the visible text of a message's content, one block per line.
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => js_trim(text).to_owned(),
        Value::Array(blocks) => blocks.iter().filter(|block| matches!(block["type"].as_str(), Some("text" | "input_text" | "output_text"))).map(|block| js_trim(block["text"].as_str().unwrap_or("")).to_owned()).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
/// `normalizeTimestamp`.
fn normalize_timestamp(value: &Value, fallback: &str) -> String {
    value.as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map(|time| iso(time.timestamp_millis())).unwrap_or_else(|| fallback.to_owned())
}
fn session_id_is_valid(id: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap()).is_match(id)
}

/// `Schema.decodeUnknownOption(TranscriptRecord)` over a record already cut down to the fields the schema names:
/// the record, or `None` when any of them has the wrong type, in which case the whole record is dropped.
fn decode_record(record: Value) -> Option<Value> {
    fn optional(object: &Value, key: &str, valid: fn(&Value) -> bool) -> bool {
        object.get(key).is_none_or(valid)
    }
    fn string(value: &Value) -> bool {
        value.is_string()
    }
    fn boolean(value: &Value) -> bool {
        value.is_boolean()
    }
    fn block(value: &Value) -> bool {
        value.is_object() && optional(value, "type", string) && optional(value, "text", string)
    }
    fn blocks(value: &Value) -> bool {
        value.as_array().is_some_and(|blocks| blocks.iter().all(block))
    }
    fn message(value: &Value) -> bool {
        value.is_object() && optional(value, "role", string) && optional(value, "model", string) && optional(value, "content", |content| content.is_string() || blocks(content))
    }
    fn payload(value: &Value) -> bool {
        value.is_object() && ["id", "session_id", "type", "role", "message", "model", "cwd"].iter().all(|key| optional(value, key, string)) && optional(value, "content", blocks)
    }
    let valid = record.is_object()
        && ["type", "timestamp", "cwd", "sessionId", "aiTitle"].iter().all(|key| optional(&record, key, string))
        && ["isSidechain", "isMeta", "isCompactSummary"].iter().all(|key| optional(&record, key, boolean))
        && optional(&record, "message", message)
        && optional(&record, "payload", payload);
    valid.then_some(record)
}

/// `extractDecodedCwd`.
fn record_cwd(record: &Value) -> Option<String> {
    [&record["cwd"], &record["payload"]["cwd"]].into_iter().find_map(|cwd| cwd.as_str().map(js_trim).filter(|cwd| !cwd.is_empty()).map(str::to_owned))
}

/// `shouldRetainDecodedRecord`: only what the import reads is kept in memory.
fn should_retain(source: Source, record: &Value) -> bool {
    if record_cwd(record).is_some() {
        return true;
    }
    let kind = record["type"].as_str();
    let payload = &record["payload"];
    match source {
        Source::Claude => matches!(kind, Some("user" | "assistant")) || !record["sessionId"].is_null() || !record["aiTitle"].is_null() || !record["message"]["model"].is_null(),
        Source::Codex => {
            matches!(kind, Some("session_meta" | "turn_context"))
                || (kind == Some("event_msg") && payload["type"] == "user_message")
                || (kind == Some("response_item") && payload["type"] == "message" && matches!(payload["role"].as_str(), Some("user" | "assistant")))
        }
    }
}

/// `codexTurnId`: the turn a response item belongs to, when its metadata names one.
fn codex_turn_id(metadata: &Value) -> Option<String> {
    metadata.as_object()?.get("turn_id")?.as_str().filter(|turn| !js_trim(turn).is_empty()).map(str::to_owned)
}

/// The first pass of `parseAgentSessionRecords` for Codex. A response item can include generated setup text
/// beside the real prompt. A response-user record is suppressed only when the shared turn id and a verbatim
/// event copy prove which prompt the user submitted. Returns the indices of the records to suppress.
fn canonical_codex_response_users(records: &[Value]) -> HashSet<usize> {
    struct Turn {
        canonical_texts: HashSet<String>,
        response_users: Vec<(usize, String, String)>,
    }
    fn finish(turn: &mut Turn, suppressed: &mut HashSet<usize>) {
        let canonical_turns: HashSet<&String> = turn.response_users.iter().filter(|(_, _, text)| turn.canonical_texts.contains(text)).map(|(_, turn_id, _)| turn_id).collect();
        for (index, turn_id, _) in &turn.response_users {
            if canonical_turns.contains(turn_id) {
                suppressed.insert(*index);
            }
        }
        turn.canonical_texts.clear();
        turn.response_users.clear();
    }
    let mut suppressed = HashSet::new();
    let mut turn = Turn { canonical_texts: HashSet::new(), response_users: vec![] };
    for (index, record) in records.iter().enumerate() {
        let (kind, payload) = (record["type"].as_str(), &record["payload"]);
        let is_message = kind == Some("response_item") && payload["type"] == "message";
        if is_message && payload["role"] == "assistant" {
            finish(&mut turn, &mut suppressed);
        } else if kind == Some("event_msg") && payload["type"] == "user_message" {
            let text = js_trim(payload["message"].as_str().unwrap_or(""));
            if !text.is_empty() {
                turn.canonical_texts.insert(text.to_owned());
            }
        } else if is_message && payload["role"] == "user" {
            let text = extract_text(&payload["content"]);
            if let (Some(turn_id), false) = (codex_turn_id(&payload["internal_chat_message_metadata_passthrough"]), text.is_empty()) {
                turn.response_users.push((index, turn_id, text));
            }
        }
    }
    finish(&mut turn, &mut suppressed);
    suppressed
}

/// The messages kept so far: the newest 200, and the conversation's first user message wherever it went.
struct History {
    messages: VecDeque<Retained>,
    first_user: Option<Retained>,
    next_sequence: usize,
}
#[derive(Clone)]
struct Retained {
    /// Identity, since two messages can carry the same words.
    sequence: usize,
    role: &'static str,
    text: String,
    created_at: String,
    codex_response_user: bool,
}
impl History {
    fn retain(&mut self, role: &'static str, text: String, created_at: String, codex_response_user: bool) {
        let message = Retained { sequence: self.next_sequence, role, text, created_at, codex_response_user };
        self.next_sequence += 1;
        if self.first_user.is_none() && role == "user" {
            self.first_user = Some(message.clone());
        }
        self.messages.push_back(message);
        if self.messages.len() > MAX_IMPORTED_MESSAGES {
            self.messages.pop_front();
        }
    }
    /// `hasMatchingCodexEventInTurn`: the turn so far already holds the canonical event copy of this prompt.
    fn has_matching_codex_event_in_turn(&self, text: &str) -> bool {
        let wanted = js_trim(text);
        for message in self.messages.iter().rev() {
            if message.role == "assistant" {
                return false;
            }
            if message.role == "user" && !message.codex_response_user && js_trim(&message.text) == wanted {
                return true;
            }
        }
        false
    }
}

/// The first `units` UTF-16 code units of `text` (`String.prototype.slice(0, units)`), without a split character.
fn utf16_prefix(text: &str, units: usize) -> &str {
    let mut used = 0;
    for (offset, character) in text.char_indices() {
        used += character.len_utf16();
        if used > units {
            return &text[..offset];
        }
    }
    text
}

/// `parseAgentSessionRecords`: keep visible user and assistant text while ignoring tools, reasoning and
/// malformed records. Claude filenames are session ids; Codex rollout filenames include extra timestamp
/// text, so only the transcript's metadata can provide a resumable id.
fn parse_records(source: Source, records: &[Value], fallback_session: &str, last_active_ms: i64) -> Option<Imported> {
    let fallback = iso(last_active_ms);
    let mut session_id = if source == Source::Codex { String::new() } else { fallback_session.to_owned() };
    let (mut title, mut model, mut has_codex_session_id) = (None::<String>, None::<String>, false);
    let mut history = History { messages: VecDeque::new(), first_user: None, next_sequence: 0 };
    let canonical = if source == Source::Codex { canonical_codex_response_users(records) } else { HashSet::new() };
    for (index, record) in records.iter().enumerate() {
        let (kind, payload) = (record["type"].as_str(), &record["payload"]);
        let created_at = || normalize_timestamp(&record["timestamp"], &fallback);
        if source == Source::Claude {
            if record["isSidechain"] == true || record["isMeta"] == true || record["isCompactSummary"] == true {
                continue;
            }
            if let Some(id) = record["sessionId"].as_str().map(js_trim).filter(|id| !id.is_empty()) {
                session_id = id.to_owned();
            }
            if let Some(ai_title) = record["aiTitle"].as_str().map(js_trim).filter(|title| !title.is_empty()) {
                title = Some(ai_title.to_owned());
            }
            // Claude uses this sentinel for local error responses. It is not a model id that can be
            // selected when the imported session resumes.
            if let Some(candidate) = record["message"]["model"].as_str().map(js_trim).filter(|model| !model.is_empty() && *model != "<synthetic>") {
                model = Some(candidate.to_owned());
            }
            let Some(role) = kind.and_then(|kind| ["user", "assistant"].into_iter().find(|role| *role == kind)) else { continue };
            let text = extract_text(&record["message"]["content"]);
            if !text.is_empty() {
                history.retain(role, text, created_at(), false);
            }
            continue;
        }
        if kind == Some("session_meta") {
            let id = payload["id"].as_str().map(js_trim).filter(|id| !id.is_empty()).or_else(|| payload["session_id"].as_str().map(js_trim).filter(|id| !id.is_empty()));
            if let (false, Some(id)) = (has_codex_session_id, id) {
                session_id = id.to_owned();
                has_codex_session_id = true;
            }
            continue;
        }
        if kind == Some("turn_context") {
            if let Some(candidate) = payload["model"].as_str().map(js_trim).filter(|model| !model.is_empty()) {
                model = Some(candidate.to_owned());
                continue;
            }
        }
        if kind == Some("event_msg") && payload["type"] == "user_message" {
            let text = payload["message"].as_str().unwrap_or("");
            if js_trim(text).is_empty() {
                continue;
            }
            // Codex can write the same prompt as both a response item and an event. Remove only the matching
            // response copy so mixed-format logs keep every distinct user message.
            for position in (0..history.messages.len()).rev() {
                let message = &history.messages[position];
                if message.role == "assistant" {
                    break;
                }
                if message.codex_response_user && js_trim(&message.text) == js_trim(text) {
                    if history.first_user.as_ref().is_some_and(|first| first.sequence == message.sequence) {
                        history.first_user = None;
                    }
                    history.messages.remove(position);
                    break;
                }
            }
            history.retain("user", text.to_owned(), created_at(), false);
            continue;
        }
        let Some(role) = (kind == Some("response_item") && payload["type"] == "message").then(|| ["user", "assistant"].into_iter().find(|role| payload["role"] == *role)).flatten() else { continue };
        let text = extract_text(&payload["content"]);
        if text.is_empty() || (role == "user" && (canonical.contains(&index) || history.has_matching_codex_event_in_turn(&text))) {
            continue;
        }
        history.retain(role, text, created_at(), role == "user");
    }
    let first_user = history.first_user.take()?;
    if js_trim(&session_id).is_empty() {
        return None;
    }
    let first_retained = history.messages.iter().any(|message| message.sequence == first_user.sequence);
    let visible = |message: Retained| Message { role: message.role, text: message.text, created_at: message.created_at };
    let messages: Vec<Message> = if first_retained {
        history.messages.into_iter().map(visible).collect()
    } else {
        // The conversation's opening request survives even when 200 later messages crowd it out.
        let keep = history.messages.len().saturating_sub(MAX_IMPORTED_MESSAGES - 1);
        std::iter::once(first_user.clone()).chain(history.messages.into_iter().skip(keep)).map(visible).collect()
    };
    let derived = js_trim(utf16_prefix(js_trim(&first_user.text).split('\n').next().unwrap_or(""), 100));
    Some(Imported {
        source,
        title: title.unwrap_or_else(|| if derived.is_empty() { "Imported thread".into() } else { derived.to_owned() }),
        created_at: messages.first().map_or_else(|| fallback.clone(), |message| message.created_at.clone()),
        updated_at: fallback,
        model,
        session_id,
        messages,
    })
}

/// `transcriptIdentity` without the path, which is the key a transcript is looked up by.
#[derive(Clone)]
struct Identity {
    size: u64,
    mtime_ms: Option<i64>,
    /// `None` where the platform or file system keeps no creation time.
    birthtime_ms: Option<i64>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    ino: u64,
}
fn identity_of(metadata: &std::fs::Metadata) -> Identity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Identity { size: metadata.len(), mtime_ms: mtime_ms(metadata), birthtime_ms: time_ms(metadata.created()), device: metadata.dev(), ino: metadata.ino() }
    }
    #[cfg(not(unix))]
    Identity { size: metadata.len(), mtime_ms: mtime_ms(metadata), birthtime_ms: time_ms(metadata.created()) }
}
/// `sameTranscriptIdentity`.
fn same_identity(left: &Identity, right: &Identity) -> bool {
    #[cfg(unix)]
    let same_file = left.device == right.device && left.ino == right.ino;
    #[cfg(not(unix))]
    let same_file = true;
    left.size == right.size && left.mtime_ms == right.mtime_ms && left.birthtime_ms == right.birthtime_ms && same_file
}

/// The selected, decoded records of a transcript stream and how many records it held, or `None` when it held
/// more than `record_limit`. Each record is cut down to the fields the import reads while it is read, within the
/// selected-history budget shared by the whole transcript; running out of it is an error for the transcript.
fn read_records(reader: impl std::io::BufRead, source: Source, record_limit: usize) -> Result<Option<(Vec<Value>, usize)>, ReadError> {
    let mut records = RecordReader::new(reader);
    let (mut selected, mut count, mut history_bytes) = (vec![], 0usize, 0usize);
    loop {
        let mut record_bytes = 0usize;
        let history = history_bytes;
        let next = records.next(
            &mut |bytes| {
                record_bytes += bytes;
                if history + record_bytes > MAX_IMPORT_HISTORY_BYTES {
                    return Err(LimitError("Transcript selected history exceeds the 32 MiB memory budget"));
                }
                Ok(())
            },
            &|path: &[Seg]| TRANSCRIPT_RECORD.includes(path, 0),
        )?;
        let Some(projected) = next else { return Ok(Some((selected, count))) };
        count += 1;
        if count > record_limit {
            return Ok(None);
        }
        if let Some(record) = projected.and_then(decode_record).filter(|record| should_retain(source, record)) {
            history_bytes += record_bytes;
            selected.push(record);
        }
    }
}

/// `readTranscript`: selected history only, within the memory and record budgets. The file's identity is
/// checked on both sides of the read, so a file rewritten meanwhile is skipped.
fn read_transcript(path: &Path, expected: &Identity, record_limit: usize, source: Source) -> Option<(Vec<Value>, usize)> {
    if expected.size > MAX_IMPORTED_TRANSCRIPT_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if !same_identity(expected, &identity_of(&file.metadata().ok()?)) {
        return None;
    }
    let mut reader = std::io::BufReader::with_capacity(32 * 1024, file.take(expected.size));
    let read = read_records(&mut reader, source, record_limit);
    let read = match read {
        Ok(read) => read?,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Could not read imported transcript");
            return None;
        }
    };
    same_identity(expected, &identity_of(&reader.get_ref().get_ref().metadata().ok()?)).then_some(read)
}

/// The transcripts an import has read, saved so a later call can pass over them (`AgentSessionImportSource`, which the original keeps
/// in the `importedTranscripts` of the runtime row of the thread: `ProviderSessionRuntime.recordImportedTranscript`). A row is the
/// file's identity as it was read and the session it held, keyed as the original keys it: the file within a provider instance, per
/// thread. `workspace_root` is the project's root as `normalize` spells it, which is how the original finds the rows of a project
/// (by the `cwd` of the runtime row). Created when the store opens, like the other tables that came after the first build
/// (`shared_rows`); nothing about it needs a migration, and a store from before it opens with the table empty.
///
/// TODO(original AgentSessionScanner.prepareRecentThreads): rows are found by root path, not project id, so a project deleted and
/// made again at the same path counts its sessions as imported without getting their threads.
pub(crate) const IMPORTED_TRANSCRIPTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rust_imported_transcripts (
  provider_instance_id TEXT NOT NULL, file_path TEXT NOT NULL, provider_session_id TEXT NOT NULL, workspace_root TEXT NOT NULL,
  provider TEXT NOT NULL, size INTEGER NOT NULL, mtime_ms INTEGER, birthtime_ms INTEGER, device INTEGER NOT NULL, inode INTEGER,
  PRIMARY KEY(provider_instance_id, file_path, provider_session_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS rust_imported_transcripts_root ON rust_imported_transcripts(workspace_root);";

/// `AgentSessionImportSource`: the file a session was read from, as it was when read.
struct ImportSource {
    provider: &'static str,
    instance: String,
    session_id: String,
    path: String,
    identity: Identity,
}

/// What a recorded file held: the provider and session it was imported as, and its identity then.
struct Completed {
    provider: String,
    session_id: String,
    identity: Identity,
}

/// The device and inode as the columns hold them: an unsigned id keeps its bits in a signed column.
fn device_and_inode(identity: &Identity) -> (i64, Option<i64>) {
    #[cfg(unix)]
    return (identity.device as i64, Some(identity.ino as i64));
    #[cfg(not(unix))]
    return (0, { let _ = identity; None });
}
fn identity_from_columns(size: i64, mtime_ms: Option<i64>, birthtime_ms: Option<i64>, device: i64, inode: Option<i64>) -> Identity {
    #[cfg(not(unix))]
    let _ = (device, inode);
    Identity {
        size: size as u64,
        mtime_ms,
        birthtime_ms,
        #[cfg(unix)]
        device: device as u64,
        #[cfg(unix)]
        ino: inode.unwrap_or(0) as u64,
    }
}

/// `recordImportedTranscript`: replaces the record of this file for this session, leaving the others.
fn record_source(connection: &rusqlite::Connection, root_key: &str, source: &ImportSource) -> rusqlite::Result<()> {
    let (device, inode) = device_and_inode(&source.identity);
    connection.execute(
        "INSERT OR REPLACE INTO rust_imported_transcripts(provider_instance_id,file_path,provider_session_id,workspace_root,provider,size,mtime_ms,birthtime_ms,device,inode) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![source.instance, source.path, source.session_id, root_key, source.provider, source.identity.size as i64, source.identity.mtime_ms, source.identity.birthtime_ms, device, inode],
    )?;
    Ok(())
}

/// The recorded files of the project rooted at `root_key`, by provider instance and path (`completedByFile`).
fn completed_sources(store: &Store, root_key: &str) -> Result<HashMap<(String, String), Vec<Completed>>, crate::persistence::StoreError> {
    store.read(|connection| {
        let mut statement = connection.prepare("SELECT provider_instance_id,file_path,provider_session_id,provider,size,mtime_ms,birthtime_ms,device,inode FROM rust_imported_transcripts WHERE workspace_root=?1")?;
        let rows = statement.query_map([root_key], |row| {
            let identity = identity_from_columns(row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?);
            Ok(((row.get::<_, String>(0)?, row.get::<_, String>(1)?), Completed { session_id: row.get(2)?, provider: row.get(3)?, identity }))
        })?;
        let mut completed: HashMap<(String, String), Vec<Completed>> = HashMap::new();
        for row in rows {
            let (key, entry) = row?;
            completed.entry(key).or_default().push(entry);
        }
        Ok(completed)
    })
}

pub struct ImportRequest<'a> {
    pub project_id: &'a str,
    pub expected_workspace_root: Option<&'a str>,
}

/// `importRecentAgentThreads`: the project's sessions from the last 30 days, newest first, within the
/// transcript, byte and record budgets. Returns `(imported, skipped)`.
pub fn import(settings: &Value, store: &Store, host: &Host, request: &ImportRequest<'_>) -> Result<(u64, u64), Value> {
    let project = store.projection("project", request.project_id).map_err(|error| scan_error("read-projects", error))?.filter(|project| project["deletedAt"].is_null());
    let Some(project) = project else { return Err(json!({"_tag":"AgentSessionImportProjectNotFoundError","projectId":request.project_id})) };
    let root = project["workspaceRoot"].as_str().unwrap_or("").to_owned();
    if request.expected_workspace_root.is_some_and(|expected| normalize(Path::new(&root)) != normalize(Path::new(expected))) {
        return Err(json!({"_tag":"AgentSessionImportProjectChangedError","projectId":request.project_id}));
    }
    let root_path = normalize_path(&std::path::absolute(crate::claude_skills::expand_home(&root, &host.home)).unwrap_or_default());
    let real_root = std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
    if is_excluded(host, &root_path) || is_excluded(host, &real_root) {
        return Ok((0, 0));
    }
    let root_identity = directory_identity(&root_path);
    let now = chrono::Utc::now().timestamp_millis();
    let (raw, _) = collect(settings, host);
    let mut eligible: Vec<(Source, String, PathBuf, i64)> = vec![];
    for candidate in &raw {
        let expanded = crate::claude_skills::expand_home(candidate.cwd.trim(), &host.home);
        if !expanded.is_absolute() || directory_identity(&normalize_path(&expanded)) != root_identity {
            continue;
        }
        for (path, mtime_ms) in &candidate.transcripts {
            if *mtime_ms >= now - RECENT_THREAD_WINDOW_MS && *mtime_ms <= now {
                eligible.push((candidate.source, candidate.instance.clone(), path.clone(), *mtime_ms));
            }
        }
    }
    eligible.sort_by(|left, right| right.3.cmp(&left.3).then_with(|| left.2.cmp(&right.2)));
    let (mut imported, mut skipped) = (0u64, 0u64);
    let (mut bytes_remaining, mut transcripts_remaining, mut records_remaining) = (MAX_IMPORT_BYTES, MAX_IMPORT_TRANSCRIPTS, MAX_IMPORT_RECORDS);
    // Files an earlier call recorded, which this one passes over while they are unchanged. The scanner's `importedSessions` is
    // `seen_sessions`; the importer's `importedThreadIds` is `imported_threads`.
    let root_key = normalize(Path::new(&root));
    let completed = completed_sources(store, &root_key).map_err(|error| scan_error("read-projects", error))?;
    let (mut seen_sessions, mut imported_threads) = (HashSet::new(), HashSet::new());
    for (source, instance, path, mtime_ms) in eligible {
        let path_text = path.to_string_lossy().into_owned();
        let recorded = completed.get(&(instance.clone(), path_text.clone()));
        // A recorded file is looked at even when the budget is gone: passing over it costs nothing.
        if recorded.is_none() && (transcripts_remaining == 0 || bytes_remaining == 0 || records_remaining == 0) {
            skipped += 1;
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else { skipped += 1; continue };
        if !metadata.is_file() {
            skipped += 1;
            continue;
        }
        let identity = identity_of(&metadata);
        if let Some(done) = recorded.and_then(|recorded| recorded.iter().find(|done| done.provider == source.driver() && same_identity(&done.identity, &identity))) {
            // `AlreadyImported`: it counts as imported, once for the session, and spends none of the budget.
            if seen_sessions.insert(format!("{instance}\0{}", done.session_id)) {
                imported_threads.insert(thread_id_of(&instance, &done.session_id));
                imported += 1;
            }
            continue;
        }
        if transcripts_remaining == 0 || records_remaining == 0 || identity.size > MAX_IMPORTED_TRANSCRIPT_BYTES || identity.size > bytes_remaining {
            skipped += 1;
            continue;
        }
        // Reserve the whole file even if its read or parse fails.
        transcripts_remaining -= 1;
        bytes_remaining -= identity.size;
        let Some((records, count)) = read_transcript(&path, &identity, records_remaining, source) else { skipped += 1; continue };
        records_remaining -= count;
        // A stable replacement file can belong to a different project than the cached candidate.
        let belongs = records.iter().find_map(record_cwd).is_some_and(|cwd| {
            let expanded = crate::claude_skills::expand_home(&cwd, &host.home);
            expanded.is_absolute() && directory_identity(&normalize_path(&expanded)) == root_identity
        });
        let fallback = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        // TODO(original AgentSessionScanner.prepareRecentThreads): a transcript that was read and skipped is recorded nowhere, so
        // it is read again, and spends the same share of the budget again, on every call. More than 100 of them newer than
        // the rest keep the rest from ever being reached.
        let Some(thread) = belongs.then(|| parse_records(source, &records, &fallback, mtime_ms)).flatten() else { skipped += 1; continue };
        let thread_id = thread_id_of(&instance, &thread.session_id);
        let file = ImportSource { provider: source.driver(), instance: instance.clone(), session_id: thread.session_id.clone(), path: path_text, identity };
        if !seen_sessions.insert(format!("{instance}\0{}", thread.session_id)) {
            // `Duplicate`: neither imported nor skipped. A copy of a session this call imported is recorded too, so it is
            // passed over next time. The original ignores a failure to record it.
            if imported_threads.contains(&thread_id) {
                let _ = store.transaction(|transaction| Ok(record_source(transaction, &root_key, &file)?));
            }
            continue;
        }
        if thread.source == Source::Claude && !session_id_is_valid(&thread.session_id) {
            skipped += 1;
            continue;
        }
        match write_thread(store, request.project_id, &root_key, &instance, &thread, &file) {
            Ok(()) => {
                imported_threads.insert(thread_id);
                imported += 1;
            }
            Err(error) => {
                tracing::warn!(session = %thread.session_id, %error, "Could not import an agent session");
                skipped += 1;
            }
        }
    }
    Ok((imported, skipped))
}

const IMPORT_EVENT_PREFIX: &str = "agent-session-import:v2";

/// The id of the thread a session is imported as.
fn thread_id_of(instance: &str, session_id: &str) -> String {
    format!("import:{instance}:{session_id}")
}

/// The thread, its messages, and the provider thread bound to the native session, as the original's importer
/// writes them (`AgentSessionImporter`): a settled thread of imported history, created and last updated when
/// the transcript was, whose first message resumes the native session. An existing import is left untouched,
/// unless its thread belongs to another project or has activity of its own.
///
/// The original saves a runtime row whose resume cursor names the session and marks the provider thread `idle`.
/// This server's adapters resume from the provider thread's native reference alone, and the Claude adapter
/// knows an imported session by its provider thread being `not_loaded` (as `thread.create` leaves one made
/// from an imported native thread), so the provider thread is written that way.
///
/// `file` is recorded (`record_source`) when the import succeeds: in the transaction that writes a new thread's events, so a
/// crash leaves either both or neither, and by itself for a thread that already exists and is the import's (one an earlier
/// build wrote has no record until now). Where the original records after the events, in a transaction of its own.
fn write_thread(store: &Store, project_id: &str, root_key: &str, instance: &str, thread: &Imported, file: &ImportSource) -> Result<(), String> {
    let thread_id = thread_id_of(instance, &thread.session_id);
    if let Some(existing) = store.projection("thread", &thread_id).map_err(|error| error.to_string())? {
        if existing["thread"]["projectId"] != project_id {
            return Err(format!("Imported thread '{thread_id}' belongs to project '{}', not '{project_id}'.", existing["thread"]["projectId"].as_str().unwrap_or("")));
        }
        // `AgentSessionThreadModifiedError`. A thread an earlier build of this port imported has no history origin
        // but is just as untouched: it was made by the server, and has never run.
        let imported_only = existing["thread"]["historyOrigin"] == "v1_import" || (existing["thread"]["createdBy"] == "system" && existing["thread"]["creationSource"] == "server" && existing["runs"].as_array().is_none_or(Vec::is_empty));
        if !imported_only {
            return Err(format!("Imported thread '{thread_id}' already contains non-imported activity."));
        }
        return store.transaction(|transaction| Ok(record_source(transaction, root_key, file)?)).map_err(|error| error.to_string());
    }
    let driver = thread.source.driver();
    let model = thread.model.clone().unwrap_or_else(|| match thread.source {
        Source::Claude => DEFAULT_CLAUDE_MODEL,
        Source::Codex => DEFAULT_CODEX_MODEL,
    }.into());
    let title = if js_trim(&thread.title).is_empty() { "Untitled thread".to_owned() } else { thread.title.clone() };
    let provider_thread_id = format!("provider-thread:provider:{driver}:native-thread:{}", crate::provider_maintenance::encode_uri_component(&thread.session_id));
    let (created_at, updated_at) = (thread.created_at.as_str(), thread.updated_at.as_str());
    let command_id = format!("{IMPORT_EVENT_PREFIX}:{thread_id}");
    let make = |event_id: String, kind: &'static str, occurred_at: &str, payload: Value| -> Result<crate::persistence::Event, String> {
        let payload = t3_contracts::normalize_event_payload(kind, payload).map_err(|error| error.to_string())?;
        Ok(crate::persistence::Event {
            event_id,
            aggregate_kind: "thread".into(),
            aggregate_id: thread_id.clone(),
            occurred_at: occurred_at.to_owned(),
            command_id: Some(command_id.clone()),
            causation_event_id: None,
            correlation_id: Some(command_id.clone()),
            event_type: kind.into(),
            payload,
            metadata: json!({"providerInstanceId":instance}),
        })
    };
    let mut events = vec![make(
        format!("{IMPORT_EVENT_PREFIX}:thread:{thread_id}:created"),
        "thread.created",
        created_at,
        json!({
            "createdBy":"system","creationSource":"server","id":thread_id,"projectId":project_id,"title":title,
            "providerInstanceId":instance,"modelSelection":{"instanceId":instance,"model":model},
            "runtimeMode":DEFAULT_RUNTIME_MODE,"interactionMode":"default","branch":null,"worktreePath":null,
            "linkedPullRequest":null,"branchPullRequest":null,"activeProviderThreadId":provider_thread_id,
            "historyOrigin":"v1_import","lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":thread_id},"forkedFrom":null,
            "createdAt":created_at,"updatedAt":updated_at,"archivedAt":null,"settledOverride":"settled","settledAt":updated_at,
            "unsettledAt":null,"snoozedUntil":null,"snoozedAt":null,"pinnedAt":null,"pinOrderKey":null,"activeOrderKey":null,
            "lastVisitedAt":null,"deletedAt":null
        }),
    )?];
    for (index, message) in thread.messages.iter().enumerate() {
        let (message_event, item_event) = message_events(&thread_id, index, message);
        events.push(make(format!("{IMPORT_EVENT_PREFIX}:message:{thread_id}:{index:06}"), "message.updated", &message.created_at, message_event)?);
        events.push(make(format!("{IMPORT_EVENT_PREFIX}:turn-item:{thread_id}:{index:06}"), "turn-item.updated", &message.created_at, item_event)?);
    }
    events.push(make(
        format!("{IMPORT_EVENT_PREFIX}:provider-thread:{provider_thread_id}"),
        "provider-thread.updated",
        updated_at,
        json!({
            "id":provider_thread_id,"driver":driver,"providerInstanceId":instance,"providerSessionId":null,"appThreadId":thread_id,"ownerNodeId":null,
            "nativeThreadRef":{"driver":driver,"nativeId":thread.session_id,"strength":"strong"},"nativeConversationHeadRef":null,"status":"not_loaded",
            "firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,"pendingBackgroundTasks":[],"contextUsage":null,
            "nativeMetadata":null,"goal":null,"createdAt":created_at,"updatedAt":updated_at
        }),
    )?);
    store
        .dispatch_unreceipted(&command_id, "thread", &thread_id, "agent-session.import", chrono::Utc::now(), |transaction| {
            record_source(transaction, root_key, file)?;
            Ok(crate::persistence::Decision::Accepted { events, effects: vec![] })
        }, crate::thread::reduce)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// `messageEvents`: the message and the turn item that shows it.
fn message_events(thread_id: &str, index: usize, message: &Message) -> (Value, Value) {
    let suffix = format!("{index:06}");
    let message_id = format!("{thread_id}:{suffix}");
    let item_id = format!("{IMPORT_EVENT_PREFIX}:turn-item:{thread_id}:{suffix}");
    let (role, text, at) = (message.role, &message.text, &message.created_at);
    let row = json!({"createdBy":if role == "user" { "user" } else { "agent" },"creationSource":"server","id":message_id,"threadId":thread_id,"runId":null,"nodeId":null,"role":role,"text":text,"attachments":[],"streaming":false,"createdAt":at,"updatedAt":at});
    let mut item = json!({"id":item_id,"threadId":thread_id,"runId":null,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":index + 1,"status":"completed","title":null,"startedAt":at,"completedAt":at,"updatedAt":at,"messageId":message_id,"text":text});
    if role == "user" {
        item["createdBy"] = json!("user");
        item["creationSource"] = json!("server");
        item["type"] = json!("user_message");
        item["inputIntent"] = json!("turn_start");
        item["attachments"] = json!([]);
    } else {
        item["type"] = json!("assistant_message");
        item["streaming"] = json!(false);
    }
    (row, item)
}

// --- RPC -------------------------------------------------------------------------------------

async fn settings(state: &ApiState) -> Result<Value, Value> {
    if let Some(providers) = &state.providers {
        return Ok(serde_json::to_value(providers.settings()).unwrap_or(Value::Null));
    }
    match &state.settings {
        Some(settings) => settings.snapshot().await.map(|snapshot| serde_json::to_value(snapshot).unwrap_or(Value::Null)).map_err(|error| scan_error("read-settings", error)),
        None => Ok(Value::Null),
    }
}

/// The project an import is for.
fn decode_project(payload: &Value) -> Result<&str, Value> {
    payload["projectId"].as_str().filter(|id| !id.trim().is_empty()).ok_or_else(|| json!({"_tag":"SchemaDecodeError","message":"agentSessions.import needs a projectId."}))
}
pub(crate) fn decode_import(payload: &Value) -> Result<(), Value> {
    decode_project(payload).map(|_| ())
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let settings = settings(state).await?;
    let host = Host::of(state.store.state_dir());
    match tag {
        "agentSessions.scan" => {
            let store = state.store.clone();
            tokio::task::spawn_blocking(move || scan(&settings, &store, &host)).await.map_err(|error| scan_error("read-projects", error))?
        }
        "agentSessions.import" => {
            let project_id = decode_project(&payload)?;
            let store = state.store.clone();
            let project_id = project_id.to_owned();
            let expected = payload["expectedWorkspaceRoot"].as_str().map(str::to_owned);
            let (imported, skipped) = tokio::task::spawn_blocking(move || import(&settings, &store, &host, &ImportRequest { project_id: &project_id, expected_workspace_root: expected.as_deref() })).await.map_err(|error| scan_error("read-projects", error))??;
            Ok(json!({"importedCount":imported,"skippedCount":skipped}))
        }
        other => Err(json!({"_tag":"NativeMethodUnsupportedError","method":other})),
    }
}

#[cfg(test)]
#[path = "agent_session_tests.rs"]
mod tests;
