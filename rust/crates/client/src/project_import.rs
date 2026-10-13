//! Importing projects and history from Claude Code and Codex: what a scan offers, which of it is
//! selected at first, how clones of one repository group, and how a run of imports is worded.
//! Port of `onboarding/projectImport.logic.ts` and the import step of `WelcomeWizard.tsx`,
//! replayed against them by `tests/import_clone_parity.rs`.
use crate::source_control::normalize_project_path;
use chrono::DateTime;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

const RECENT_PROJECT_WINDOW_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;
/// One or two threads in a directory is usually a one-off question, not a project.
const DEFAULT_SELECTION_MIN_THREADS: f64 = 3.0;

pub const SCAN: &str = "agentSessions.scan";
pub const IMPORT: &str = "agentSessions.import";
pub const SCAN_LIMIT_MESSAGE: &str = "Scan limit reached. Some projects or conversations may be missing.";
pub const PERMISSION_MESSAGE: &str = "This connection cannot import projects or thread history.";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Git {
    #[serde(default)]
    pub remote_key: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
}
/// A directory an agent CLI has run in, offered as a project. `git` is `None` for a directory
/// that is not a repository root, and `unknown_git` for a server that predates the git scan.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub thread_count: f64,
    #[serde(default)]
    pub last_active_at: Option<String>,
    #[serde(default)]
    pub already_imported: bool,
    #[serde(default, deserialize_with = "git_field")]
    pub git: GitField,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GitField {
    /// Not reported at all (an older server).
    #[default]
    Missing,
    /// Reported as not a repository.
    Folder,
    Repository(Git),
}
fn git_field<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<GitField, D::Error> {
    Ok(match Option::<Git>::deserialize(deserializer)? {
        Some(git) => GitField::Repository(git),
        None => GitField::Folder,
    })
}
impl Candidate {
    /// Identity of a candidate on its computer: paths mean something only within one.
    pub fn key(&self, environment: &str) -> String {
        serde_json::to_string(&[environment, &self.path]).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scan {
    pub candidates: Vec<Candidate>,
    pub truncated: bool,
}
impl Scan {
    pub fn decode(value: &Value) -> Result<Self, String> {
        let candidates = value["candidates"].as_array().ok_or("The scan did not list projects.")?;
        // A candidate that does not read is left out, as an unknown entry of a forward-compatible list is.
        let candidates = candidates.iter().filter_map(|candidate| serde_json::from_value::<Candidate>(candidate.clone()).ok()).collect();
        Ok(Self { candidates, truncated: value["truncated"] == true })
    }
}

fn time(text: &str) -> f64 {
    DateTime::parse_from_rfc3339(text).map(|at| at.timestamp_millis() as f64).unwrap_or(f64::NAN)
}

/// The default selection: git repositories active in the last 30 days with enough threads to
/// look like real work. Every candidate is offered, because an existing project still needs its
/// history imported.
pub fn recent(candidates: &[Candidate], now_ms: f64) -> Vec<&Candidate> {
    let cutoff = now_ms - RECENT_PROJECT_WINDOW_MS;
    candidates
        .iter()
        .filter(|candidate| {
            if candidate.git == GitField::Folder || candidate.thread_count < DEFAULT_SELECTION_MIN_THREADS {
                return false;
            }
            let Some(last) = candidate.last_active_at.as_deref().map(time) else { return false };
            last >= cutoff && last <= now_ms
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group<'a> {
    pub key: String,
    /// GitHub `owner/name`, or the checkout's folder name when the origin is elsewhere.
    pub label: String,
    pub repository: Option<String>,
    pub candidates: Vec<&'a Candidate>,
    pub thread_count: f64,
    pub last_active_at: Option<String>,
}

fn latest(left: &Option<String>, right: &Option<String>) -> Option<String> {
    match (left, right) {
        (None, other) | (other, None) => other.clone(),
        (Some(left), Some(right)) => Some(if left > right { left.clone() } else { right.clone() }),
    }
}

/// Clones of one repository share a group keyed by their origin; a repository without an origin
/// gets a group of its own; folders that are not repositories come back separately. Groups sort
/// by their most recent activity, newest first.
pub fn group(candidates: &[Candidate]) -> (Vec<Group<'_>>, Vec<&Candidate>) {
    let mut groups: IndexMap<String, Group> = IndexMap::new();
    let mut other = Vec::new();
    for candidate in candidates {
        let git = match &candidate.git {
            GitField::Folder => {
                other.push(candidate);
                continue;
            }
            GitField::Missing => Git { remote_key: None, repository: None },
            GitField::Repository(git) => git.clone(),
        };
        let key = match &git.remote_key {
            None => format!("path:{}", candidate.path),
            Some(remote) => format!("remote:{remote}"),
        };
        match groups.get_mut(&key) {
            None => {
                groups.insert(
                    key.clone(),
                    Group { key, label: git.repository.clone().unwrap_or_else(|| candidate.title.clone()), repository: git.repository, candidates: vec![candidate], thread_count: candidate.thread_count, last_active_at: candidate.last_active_at.clone() },
                );
            }
            Some(existing) => {
                existing.candidates.push(candidate);
                existing.thread_count += candidate.thread_count;
                existing.last_active_at = latest(&existing.last_active_at, &candidate.last_active_at);
            }
        }
    }
    let mut repositories: Vec<Group> = groups.into_values().collect();
    repositories.sort_by(|left, right| match (&left.last_active_at, &right.last_active_at) {
        (a, b) if a == b => left.label.cmp(&right.label),
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => b.cmp(a),
    });
    (repositories, other)
}

/// The sources behind a group of candidates, each once, in the order first seen: the row says "Claude Code, Codex" for
/// a repository with history from both however its checkouts interleave (`new Set(candidates.flatMap(sources))`).
pub fn sources_of<'a>(candidates: &[&'a Candidate]) -> Vec<&'a str> {
    let mut sources: Vec<&str> = Vec::new();
    for source in candidates.iter().flat_map(|candidate| candidate.sources.iter().map(String::as_str)) {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    sources
}

/// `normalizeProjectPathForComparison`: dispatch form, and on Windows paths lowercase with backslashes.
pub fn comparable(path: &str) -> String {
    let normalized = normalize_project_path(path);
    let bytes = normalized.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if drive || normalized.starts_with("\\\\") { normalized.replace('/', "\\").to_lowercase() } else { normalized }
}

/// The server's project match first, then the snapshot's, which can lag behind the scan.
pub fn resolve_project_id(projects: &[(String, String)], candidate: &Candidate) -> Option<String> {
    if let Some(id) = &candidate.project_id {
        return Some(id.clone());
    }
    let wanted = comparable(&candidate.path);
    if wanted.is_empty() {
        return None;
    }
    projects.iter().find(|(_, root)| comparable(root) == wanted).map(|(id, _)| id.clone())
}

fn plural(count: f64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1.0 { one } else { many })
}
/// What a finished run says when some projects did not import completely.
pub fn warning(imported_threads: f64, skipped_threads: f64) -> String {
    if imported_threads > 0.0 && skipped_threads > 0.0 {
        format!("Imported {}. {} could not be imported.", plural(imported_threads, "thread", "threads"), plural(skipped_threads, "thread", "threads"))
    } else if skipped_threads > 0.0 {
        format!("{} not be imported.", plural(skipped_threads, "thread could", "threads could"))
    } else if imported_threads > 0.0 {
        format!("Imported {}. Some thread history could not be imported.", plural(imported_threads, "thread", "threads"))
    } else {
        "Could not import thread history.".to_owned()
    }
}
/// The notice after a run that imported everything it was given.
pub fn imported_title(count: f64) -> String {
    format!("Imported {}", plural(count, "thread", "threads"))
}
pub fn import_label(selected: usize, importing: bool) -> String {
    if importing { "Importing\u{2026}".to_owned() } else { format!("Import {selected} {}", if selected == 1 { "project" } else { "projects" }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(path: &str, sources: &[&str]) -> Candidate {
        Candidate { path: path.into(), title: path.into(), project_id: None, sources: sources.iter().map(|source| source.to_string()).collect(), thread_count: 1.0, last_active_at: None, already_imported: false, git: GitField::Missing }
    }

    #[test]
    fn a_group_lists_each_source_once_in_the_order_first_seen() {
        let (first, second, third) = (candidate("/a", &["claudeAgent"]), candidate("/b", &["codex"]), candidate("/c", &["claudeAgent", "codex"]));
        assert_eq!(sources_of(&[&first, &second, &first, &third]), ["claudeAgent", "codex"]);
        assert_eq!(sources_of(&[&second, &first]), ["codex", "claudeAgent"]);
        assert_eq!(sources_of(&[&second]), ["codex"]);
        assert!(sources_of(&[]).is_empty());
    }

    #[test]
    fn a_scan_with_a_codex_candidate_decodes_its_sources_and_count() {
        let scan = Scan::decode(&serde_json::json!({"candidates":[
            {"path":"/work/app","title":"app","sources":["claudeAgent","codex"],"threadCount":7,"lastActiveAt":"2026-08-24T10:00:00.000Z","alreadyImported":false,"git":null},
            {"path":"/work/other","title":"other","sources":["codex"],"threadCount":2,"lastActiveAt":null,"alreadyImported":true,"projectId":"p"}],
            "scannedAt":"2026-08-24T10:00:00.000Z","truncated":true}))
        .unwrap();
        assert!(scan.truncated);
        let seen: Vec<_> = scan.candidates.iter().map(|candidate| (candidate.sources.clone(), candidate.thread_count)).collect();
        assert_eq!(seen, [(vec!["claudeAgent".to_owned(), "codex".to_owned()], 7.0), (vec!["codex".to_owned()], 2.0)]);
        assert_eq!(scan.candidates[1].git, GitField::Missing);
    }
}
