//! `agentSessions.scan` and `agentSessions.import` for Claude Code history (`AgentSessionScanner.ts`,
//! `AgentSessionImporter.ts`).
//!
//! Scanning finds the directories Claude Code has run in, from `<config dir>/projects/*/*.jsonl`,
//! and offers each as a project; importing turns a project's recent sessions into T3 Code threads
//! that resume the native session on their first message. The Claude home is read, never written,
//! and every read is bounded as in the original: file counts, directory operations, bytes and
//! records per source and per transcript, and a selected-history budget per imported transcript.
//!
//! Divergences: Codex rollouts are not scanned or imported (they need the Codex session readers),
//! and a session is imported once, by the existence of its thread, rather than by the per-file
//! identity the original keeps in its runtime rows.
use crate::{persistence::Store, transport::ApiState};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Read},
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
const DEFAULT_CLAUDE_MODEL: &str = "claude-fable-5-1";

fn scan_error(operation: &str, cause: impl std::fmt::Display) -> Value {
    json!({"_tag":"AgentSessionScanError","operation":operation,"cause":cause.to_string()})
}

/// What the scan needs from the host. Injectable so tests read a temp directory.
pub struct Host {
    pub home: PathBuf,
    /// This server's T3 home and its worktrees: sessions started inside them are the app's own.
    pub base_dir: PathBuf,
    pub temp_dirs: Vec<PathBuf>,
}
impl Host {
    pub fn of(state_dir: Option<&Path>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let base_dir = state_dir.map(|dir| if dir.file_name().is_some_and(|name| name == "userdata") { dir.parent().unwrap_or(dir).to_path_buf() } else { dir.to_path_buf() }).unwrap_or_default();
        Self { home, base_dir, temp_dirs: vec![std::env::temp_dir(), PathBuf::from("/tmp"), PathBuf::from("/private/tmp")] }
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
    instance: String,
    thread_count: usize,
    last_active_ms: i64,
    transcripts: Vec<(PathBuf, i64)>,
}

/// The enabled Claude instances and the config directory each reads, the built-in instance first.
fn claude_homes(settings: &Value, host: &Host) -> Vec<(String, PathBuf)> {
    let instances = settings["providerInstances"].as_object().cloned().unwrap_or_default();
    let mut entries: Vec<(String, Value, Value)> = instances.iter().filter(|(_, instance)| instance["driver"] == "claudeAgent" && instance["enabled"] != false).map(|(id, instance)| (id.clone(), instance["config"].clone(), instance["environment"].clone())).collect();
    if !instances.contains_key("claudeAgent") {
        entries.push(("claudeAgent".into(), Value::Null, Value::Null));
    }
    entries.sort_by_key(|(id, _, _)| id != "claudeAgent");
    let mut seen = HashSet::new();
    let mut homes = vec![];
    for (id, config, environment) in entries {
        if !(config.is_null() || config.is_object()) {
            continue;
        }
        let configured = config["homePath"].as_str().map(str::trim).unwrap_or("");
        let from_environment = environment.as_array().into_iter().flatten().rev().find(|variable| variable["name"] == "CLAUDE_CONFIG_DIR").and_then(|variable| variable["value"].as_str().map(str::to_owned)).or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok()).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
        let home = if !configured.is_empty() {
            crate::claude_skills::expand_home(configured, &host.home)
        } else if let Some(dir) = from_environment {
            crate::claude_skills::expand_home(&dir, &host.home)
        } else {
            host.home.join(".claude")
        };
        let home = std::path::absolute(&home).unwrap_or(home);
        if seen.insert(directory_identity(&home)) {
            homes.push((id, home));
        }
    }
    homes
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

fn mtime_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_millis() as i64)
}

/// `discoverClaudeTranscripts`: `projects/<dir>/*.jsonl` files, within an operation budget.
fn discover(home: &Path, instance: &str, budget: usize) -> (Vec<Transcript>, bool) {
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
        fn list(&mut self, directory: &Path) -> Vec<String> {
            if !self.spend() {
                return vec![];
            }
            std::fs::read_dir(directory).map(|entries| entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default()
        }
    }
    let projects = home.join("projects");
    let mut operations = Operations { remaining: budget, truncated: false };
    let mut transcripts = vec![];
    for project in operations.list(&projects) {
        let directory = projects.join(&project);
        for entry in operations.list(&directory).into_iter().filter(|entry| entry.ends_with(".jsonl")) {
            if !operations.spend() {
                break;
            }
            let path = directory.join(entry);
            let Ok(metadata) = std::fs::metadata(&path) else { continue };
            let Some(mtime_ms) = mtime_ms(&metadata).filter(|_| metadata.is_file()) else { continue };
            transcripts.push(Transcript { path, mtime_ms, size: metadata.len(), instance: instance.to_owned() });
        }
        if operations.remaining == 0 {
            operations.truncated = true;
            break;
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
    let mut reserve = |budget: &mut Budget, records_read: &mut usize| {
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

/// Every candidate directory Claude Code has run in, plus whether a bound cut the search short.
fn collect(settings: &Value, host: &Host) -> (Vec<Candidate>, bool) {
    let homes = claude_homes(settings, host);
    let mut transcripts = vec![];
    let mut truncated = false;
    let base = MAX_DISCOVERY_OPERATIONS_PER_SOURCE / homes.len().max(1);
    let extra = MAX_DISCOVERY_OPERATIONS_PER_SOURCE % homes.len().max(1);
    for (index, (instance, home)) in homes.iter().enumerate() {
        let budget = base + usize::from(index < extra);
        if budget == 0 {
            truncated = true;
            continue;
        }
        let (found, cut) = discover(home, instance, budget);
        truncated |= cut;
        transcripts.extend(found);
    }
    transcripts.sort_by(|left, right| right.mtime_ms.cmp(&left.mtime_ms).then_with(|| left.path.cmp(&right.path)));
    if transcripts.len() > MAX_TRANSCRIPTS_PER_SOURCE {
        truncated = true;
    }
    let selected = select_metadata_transcripts(transcripts);
    let mut budget = Budget { bytes: MAX_METADATA_BYTES_PER_SOURCE, operations: MAX_METADATA_OPERATIONS_PER_SOURCE, records: MAX_METADATA_RECORDS_PER_SOURCE, truncated: false };
    let mut groups: Vec<Candidate> = vec![];
    for transcript in &selected {
        let Some(cwd) = read_cwd(transcript, &mut budget) else { continue };
        match groups.iter_mut().find(|group| group.instance == transcript.instance && group.cwd == cwd) {
            Some(group) => {
                group.last_active_ms = group.last_active_ms.max(transcript.mtime_ms);
                group.thread_count += 1;
                group.transcripts.push((transcript.path.clone(), transcript.mtime_ms));
            }
            None => groups.push(Candidate { cwd, instance: transcript.instance.clone(), thread_count: 1, last_active_ms: transcript.mtime_ms, transcripts: vec![(transcript.path.clone(), transcript.mtime_ms)] }),
        }
    }
    (groups, truncated || budget.truncated)
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

/// `scan`: the directories Claude Code has run in, offered as projects, newest first.
pub fn scan(settings: &Value, store: &Store, host: &Host) -> Result<Value, Value> {
    let (raw, truncated) = collect(settings, host);
    struct Merged {
        path: PathBuf,
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
                existing.thread_count += candidate.thread_count;
                existing.last_active_ms = existing.last_active_ms.max(Some(candidate.last_active_ms));
            }
            None => merged.push((key.clone(), Merged { path: resolved, thread_count: candidate.thread_count, last_active_ms: Some(candidate.last_active_ms), git: git_identities.get(&key).cloned().unwrap_or(Value::Null) })),
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
        let mut candidate = json!({"path":root,"title":title,"sources":["claudeAgent"],"threadCount":entry.thread_count,"lastActiveAt":entry.last_active_ms.map(iso),"alreadyImported":project_id.is_some(),"git":entry.git});
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

struct Imported {
    session_id: String,
    title: String,
    model: Option<String>,
    created_at: String,
    updated_at: String,
    messages: Vec<(String, String, String)>,
}

fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(blocks) => blocks.iter().filter(|block| matches!(block["type"].as_str(), Some("text" | "input_text" | "output_text"))).map(|block| block["text"].as_str().map(str::trim).unwrap_or("").to_owned()).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}
fn normalize_timestamp(value: &Value, fallback: &str) -> String {
    value.as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map(|time| iso(time.timestamp_millis())).unwrap_or_else(|| fallback.to_owned())
}
fn session_id_is_valid(id: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap()).is_match(id)
}
/// `shouldRetainDecodedRecord` for Claude: only what the import reads is kept in memory.
fn retain(record: &Value) -> bool {
    let cwd = |value: &Value| value.as_str().is_some_and(|cwd| !cwd.trim().is_empty());
    cwd(&record["cwd"]) || cwd(&record["payload"]["cwd"]) || matches!(record["type"].as_str(), Some("user" | "assistant")) || record["sessionId"].is_string() || record["aiTitle"].is_string() || record["message"]["model"].is_string()
}

/// `parseAgentSessionRecords` for Claude: visible user and assistant text only; tools, reasoning
/// and sidechains are dropped. At most 200 messages are kept, the first user message always.
fn parse_session(records: &[Value], fallback_session: &str, last_active_ms: i64) -> Option<Imported> {
    let fallback = iso(last_active_ms);
    let (mut session_id, mut title, mut model) = (fallback_session.to_owned(), None::<String>, None::<String>);
    let mut messages: std::collections::VecDeque<(String, String, String)> = Default::default();
    let mut first_user: Option<(String, String, String)> = None;
    for record in records {
        if record["isSidechain"] == true || record["isMeta"] == true || record["isCompactSummary"] == true {
            continue;
        }
        if let Some(id) = record["sessionId"].as_str().map(str::trim).filter(|id| !id.is_empty()) {
            session_id = id.to_owned();
        }
        if let Some(ai_title) = record["aiTitle"].as_str().map(str::trim).filter(|title| !title.is_empty()) {
            title = Some(ai_title.to_owned());
        }
        if let Some(candidate) = record["message"]["model"].as_str().map(str::trim).filter(|model| !model.is_empty() && *model != "<synthetic>") {
            model = Some(candidate.to_owned());
        }
        let Some(role) = record["type"].as_str().filter(|role| matches!(*role, "user" | "assistant")) else { continue };
        let text = extract_text(&record["message"]["content"]);
        if text.is_empty() {
            continue;
        }
        let message = (role.to_owned(), text, normalize_timestamp(&record["timestamp"], &fallback));
        if first_user.is_none() && role == "user" {
            first_user = Some(message.clone());
        }
        messages.push_back(message);
        if messages.len() > MAX_IMPORTED_MESSAGES {
            messages.pop_front();
        }
    }
    let first_user = first_user?;
    if session_id.trim().is_empty() {
        return None;
    }
    let mut retained: Vec<_> = messages.into_iter().collect();
    // The conversation's opening request survives even when 200 later messages crowd it out.
    if !retained.iter().any(|message| *message == first_user) {
        retained.drain(..retained.len().saturating_sub(MAX_IMPORTED_MESSAGES - 1));
        retained.insert(0, first_user.clone());
    }
    let derived: String = first_user.1.trim().split('\n').next().unwrap_or("").chars().take(100).collect::<String>().trim().to_owned();
    Some(Imported {
        title: title.unwrap_or(if derived.is_empty() { "Imported thread".into() } else { derived }),
        created_at: retained.first().map(|message| message.2.clone()).unwrap_or_else(|| fallback.clone()),
        updated_at: fallback,
        model,
        session_id,
        messages: retained,
    })
}

struct Identity {
    size: u64,
    mtime_ms: Option<i64>,
    #[cfg(unix)]
    ino: u64,
}
fn identity_of(metadata: &std::fs::Metadata) -> Identity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Identity { size: metadata.len(), mtime_ms: mtime_ms(metadata), ino: metadata.ino() }
    }
    #[cfg(not(unix))]
    Identity { size: metadata.len(), mtime_ms: mtime_ms(metadata) }
}
fn same_identity(left: &Identity, right: &Identity) -> bool {
    #[cfg(unix)]
    let same_inode = left.ino == right.ino;
    #[cfg(not(unix))]
    let same_inode = true;
    left.size == right.size && left.mtime_ms == right.mtime_ms && same_inode
}

/// `readTranscript`: selected history only, within the memory and record budgets. The file's
/// identity is checked on both sides of the read, so a file rewritten meanwhile is skipped.
fn read_transcript(path: &Path, expected: &Identity, record_limit: usize) -> Option<(Vec<Value>, usize)> {
    if expected.size > MAX_IMPORTED_TRANSCRIPT_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if !same_identity(expected, &identity_of(&file.metadata().ok()?)) {
        return None;
    }
    let mut reader = std::io::BufReader::with_capacity(32 * 1024, file).take(expected.size);
    let (mut records, mut count, mut history_bytes) = (vec![], 0usize, 0usize);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        count += 1;
        if count > record_limit {
            return None;
        }
        let text = String::from_utf8_lossy(&line);
        let Ok(record) = serde_json::from_str::<Value>(text.trim()) else { continue };
        if !record.is_object() || !retain(&record) {
            continue;
        }
        history_bytes += line.len();
        if history_bytes > MAX_IMPORT_HISTORY_BYTES {
            return None;
        }
        records.push(record);
    }
    let after = std::fs::metadata(path).ok()?;
    same_identity(expected, &identity_of(&after)).then_some((records, count))
}

pub struct ImportRequest<'a> {
    pub project_id: &'a str,
    pub expected_workspace_root: Option<&'a str>,
}

/// `importRecentAgentThreads`: the project's sessions from the last 30 days, newest first, within
/// the transcript, byte and record budgets. Returns `(imported, skipped)`.
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
    let mut eligible: Vec<(String, PathBuf, i64)> = vec![];
    for candidate in &raw {
        let expanded = crate::claude_skills::expand_home(candidate.cwd.trim(), &host.home);
        if !expanded.is_absolute() || directory_identity(&normalize_path(&expanded)) != root_identity {
            continue;
        }
        for (path, mtime_ms) in &candidate.transcripts {
            if *mtime_ms >= now - RECENT_THREAD_WINDOW_MS && *mtime_ms <= now {
                eligible.push((candidate.instance.clone(), path.clone(), *mtime_ms));
            }
        }
    }
    eligible.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.1.cmp(&right.1)));
    let (mut imported, mut skipped) = (0u64, 0u64);
    let (mut bytes_remaining, mut transcripts_remaining, mut records_remaining) = (MAX_IMPORT_BYTES, MAX_IMPORT_TRANSCRIPTS, MAX_IMPORT_RECORDS);
    let mut seen_sessions = HashSet::new();
    for (instance, path, mtime_ms) in eligible {
        let Ok(metadata) = std::fs::metadata(&path) else { skipped += 1; continue };
        if !metadata.is_file() {
            skipped += 1;
            continue;
        }
        let identity = identity_of(&metadata);
        if transcripts_remaining == 0 || records_remaining == 0 || identity.size > MAX_IMPORTED_TRANSCRIPT_BYTES || identity.size > bytes_remaining {
            skipped += 1;
            continue;
        }
        // Reserve the whole file even if its read or parse fails.
        transcripts_remaining -= 1;
        bytes_remaining -= identity.size;
        let Some((records, count)) = read_transcript(&path, &identity, records_remaining) else { skipped += 1; continue };
        records_remaining -= count;
        // A stable replacement file can belong to a different project than the cached candidate.
        let cwd = records.iter().find_map(|record| [&record["cwd"], &record["payload"]["cwd"]].into_iter().find_map(|cwd| cwd.as_str().map(str::trim).filter(|cwd| !cwd.is_empty()).map(str::to_owned)));
        let belongs = cwd.is_some_and(|cwd| {
            let expanded = crate::claude_skills::expand_home(&cwd, &host.home);
            expanded.is_absolute() && directory_identity(&normalize_path(&expanded)) == root_identity
        });
        let fallback = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(thread) = belongs.then(|| parse_session(&records, &fallback, mtime_ms)).flatten() else { skipped += 1; continue };
        if !seen_sessions.insert(format!("{instance}\0{}", thread.session_id)) {
            continue;
        }
        if !session_id_is_valid(&thread.session_id) {
            skipped += 1;
            continue;
        }
        match write_thread(store, request.project_id, &instance, &thread) {
            Ok(_) => imported += 1,
            Err(error) => {
                tracing::warn!(session = %thread.session_id, %error, "Could not import an agent session");
                skipped += 1;
            }
        }
    }
    Ok((imported, skipped))
}

/// The thread, its messages, and the provider thread bound to the native session. Re-importing a
/// session whose thread exists leaves it untouched.
fn write_thread(store: &Store, project_id: &str, instance: &str, thread: &Imported) -> Result<bool, String> {
    let thread_id = format!("import:{instance}:{}", thread.session_id);
    if store.projection("thread", &thread_id).map_err(|error| error.to_string())?.is_some() {
        return Ok(true);
    }
    let model = thread.model.clone().unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into());
    let title = if thread.title.trim().is_empty() { "Untitled thread".to_owned() } else { thread.title.clone() };
    let command = json!({
        "type":"thread.create","commandId":format!("agent-session-import:v2:{thread_id}"),"threadId":thread_id,"projectId":project_id,"title":title,
        "modelSelection":{"instanceId":instance,"model":model},"runtimeMode":"approval-required","interactionMode":"default",
        "importedNativeThread":{"ref":{"driver":"claudeAgent","nativeId":thread.session_id,"strength":"strong"},"metadata":{"itemIdentityVersion":2}},
        "branch":null,"worktreePath":null,"createdBy":"system","creationSource":"server"
    });
    let receipt = crate::thread::ThreadService::new(store.clone()).dispatch(&command, chrono::Utc::now()).map_err(|error| error.to_string())?;
    if receipt.status == "rejected" {
        return Err("The thread was rejected.".into());
    }
    let events: Vec<(&'static str, Value)> = thread.messages.iter().enumerate().flat_map(|(index, (role, text, at))| message_events(&thread_id, index, role, text, at)).collect();
    crate::codex_runtime::commit(store, &thread_id, |_| Ok(events.clone())).map_err(|error| error.to_string())?;
    Ok(true)
}
fn message_events(thread_id: &str, index: usize, role: &str, text: &str, at: &str) -> Vec<(&'static str, Value)> {
    let suffix = format!("{index:06}");
    let message_id = format!("{thread_id}:{suffix}");
    let item_id = format!("agent-session-import:v2:turn-item:{thread_id}:{suffix}");
    let message = json!({"createdBy":if role == "user" { "user" } else { "agent" },"creationSource":"server","id":message_id,"threadId":thread_id,"runId":null,"nodeId":null,"role":role,"text":text,"attachments":[],"streaming":false,"createdAt":at,"updatedAt":at});
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
    vec![("message.updated", message), ("turn-item.updated", item)]
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

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let settings = settings(state).await?;
    let host = Host::of(state.store.state_dir());
    match tag {
        "agentSessions.scan" => {
            let store = state.store.clone();
            tokio::task::spawn_blocking(move || scan(&settings, &store, &host)).await.map_err(|error| scan_error("read-projects", error))?
        }
        "agentSessions.import" => {
            let Some(project_id) = payload["projectId"].as_str().filter(|id| !id.trim().is_empty()) else { return Err(json!({"_tag":"SchemaDecodeError","message":"agentSessions.import needs a projectId."})) };
            let store = state.store.clone();
            let project_id = project_id.to_owned();
            let expected = payload["expectedWorkspaceRoot"].as_str().map(str::to_owned);
            let (imported, skipped) = tokio::task::spawn_blocking(move || import(&settings, &store, &host, &ImportRequest { project_id: &project_id, expected_workspace_root: expected.as_deref() })).await.map_err(|error| scan_error("read-projects", error))??;
            Ok(json!({"importedCount":imported,"skippedCount":skipped}))
        }
        other => Err(json!({"_tag":"NativeMethodUnsupportedError","method":other})),
    }
}
