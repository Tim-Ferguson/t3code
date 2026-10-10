//! The files a reader has cleared, for a host that keeps no such record of its own (port of
//! `pullRequestViewedFiles.ts`). GitLab keeps them in one browser's local storage, so this
//! environment keeps them instead: they follow the reader between the clients connected to it,
//! but they are not the ones gitlab.com shows.
//!
//! Only cleared files are stored; unticking deletes. A mark carries the revision the head had of
//! the file when it was cleared, so a file the branch has since moved on from reads back as
//! dismissed. Revisions are asked for the marked paths alone, so a reader who has marked nothing
//! costs no host call. The marks live in the store's projection table under their own kind.
use crate::{
    persistence::{StoreError, write_projection},
    pull_request_service::{Env, PrError, PrResult, PullRequestService, Resolved},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::*;

const PROJECTION_KIND: &str = "pullRequestFilesViewed";
/// How many marks one read carries; a reader over that is told so, like a paginated read.
pub const MAX_FILES_VIEWED_ROWS: usize = 500;
/// How long the head's version of a file is believed, and how long a held answer may still be
/// merged into. This is the host call behind the **Changed** badge alone.
const FILE_REVISIONS_CACHE_TTL: Duration = Duration::from_secs(60);
const FILE_REVISIONS_STALE_WINDOW: Duration = Duration::from_secs(10 * 60);
const FILE_REVISIONS_CACHE_CAPACITY: usize = 64;
/// How many paths one scope's held entry carries.
pub const MAX_FILE_REVISION_PATHS: usize = 1_000;

/// What the head has of the files a reader has marked, held between reads. The entry tracks what
/// was asked as well as what was heard: a path missing from an answer keeps its last version.
#[derive(Default)]
struct HeldRevisions {
    at: Duration,
    asked: Vec<String>,
    revisions: BTreeMap<String, String>,
}

/// Held revisions and the gates that order one reader's presses on one change request.
#[derive(Default)]
pub struct ViewedState {
    held: Mutex<(HashMap<String, HeldRevisions>, Vec<String>)>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn store_failure(operation: &str) -> PrError {
    PrError::operation(operation, "This environment could not reach its record of which files you have seen.")
}

impl PullRequestService {
    /// Who the host says the reader is, for the paths whose rows are keyed by it. A lookup that
    /// failed is refused rather than answered as the unnamed reader: a momentarily signed-out CLI
    /// would otherwise hide every tick this reader has made and file the next press under rows
    /// that are orphaned once it recovers.
    async fn required_viewer(&self, env: &Env, resolved: &Resolved, operation: &str) -> PrResult<String> {
        let kind = resolved.project.kind;
        self.viewer_of(env, kind, &resolved.project.project.workspace_root, &resolved.project.host)
            .await
            .map_err(|failure| PrError::from_failure(operation, failure, kind))
    }

    /// Provider and host lead the key because the same repository can exist on more than one
    /// install; the reader is part of it because a host's own record is per-account.
    fn scope_key(resolved: &Resolved, viewer: &str) -> String {
        json!([resolved.project.kind.as_str(), resolved.project.host, resolved.project.remote, resolved.number, viewer]).to_string()
    }

    /// What the head has of these files, or none where the host cannot say (not an error: the
    /// marks just stop reporting staleness). A held answer that covers every path is believed for
    /// a minute; `fresh` is for the press itself, which must not store a revision the head already
    /// moved off.
    async fn file_revisions_of(&self, env: &Env, resolved: &Resolved, paths: &[String], fresh: bool) -> Option<BTreeMap<String, String>> {
        let provider = self.provider(env, resolved.project.kind)?;
        let key = self.key(resolved);
        let now = self.now();
        {
            let mut guard = self.viewed.held.lock().unwrap();
            let (held, order) = &mut *guard;
            if let Some(entry) = held.get(&key) {
                let usable = now.saturating_sub(entry.at) <= FILE_REVISIONS_STALE_WINDOW && paths.iter().all(|path| entry.asked.contains(path));
                if usable && !fresh && now.saturating_sub(entry.at) <= FILE_REVISIONS_CACHE_TTL {
                    let revisions = entry.revisions.clone();
                    // Put back at the end on every read, so the scope a reader is working through
                    // is not the one evicted.
                    order.retain(|existing| *existing != key);
                    order.push(key.clone());
                    return Some(revisions);
                }
            }
        }
        let answer = provider.file_revisions(&resolved.subject(), paths).await.ok()?;
        let mut guard = self.viewed.held.lock().unwrap();
        let (held, order) = &mut *guard;
        // Past the stale window the old entry is not worth merging into: it would carry paths
        // nobody has asked about since, at revisions the head has long moved off.
        let carried = held.remove(&key).filter(|entry| now.saturating_sub(entry.at) <= FILE_REVISIONS_STALE_WINDOW);
        order.retain(|existing| *existing != key);
        let carried_at = carried.as_ref().map(|entry| entry.at);
        let (mut asked, mut revisions) = carried.map(|entry| (entry.asked, entry.revisions)).unwrap_or_default();
        // The paths asked for go last, so a whole-change answer wider than the cap is trimmed
        // down to the reader's own files rather than over them.
        let learned: Vec<String> = if answer.complete { answer.revisions.keys().cloned().chain(paths.iter().cloned()).collect() } else { paths.to_vec() };
        for path in learned {
            asked.retain(|existing| *existing != path);
            asked.push(path.clone());
            // A path left out of the answer keeps its last known version rather than being cleared.
            if let Some(revision) = answer.revisions.get(&path) {
                revisions.insert(path, revision.clone());
            }
        }
        while asked.len() > MAX_FILE_REVISION_PATHS {
            let dropped = asked.remove(0);
            revisions.remove(&dropped);
        }
        if held.len() >= FILE_REVISIONS_CACHE_CAPACITY && !order.is_empty() {
            let oldest = order.remove(0);
            held.remove(&oldest);
        }
        // The entry is only as fresh as its oldest revision: stamping it with `now` on a partial
        // answer would let an old revision ride past the point it should have been re-read.
        let at = if revisions.keys().all(|path| answer.revisions.contains_key(path)) { now } else { carried_at.unwrap_or(now) };
        held.insert(key.clone(), HeldRevisions { at, asked, revisions: revisions.clone() });
        order.push(key);
        Some(revisions)
    }

    fn read_marks(&self, env: &Env, scope: &str, operation: &str) -> PrResult<(BTreeMap<String, Option<String>>, bool)> {
        let stored = env.store.projection(PROJECTION_KIND, scope).map_err(|_| store_failure(operation))?;
        let marks: BTreeMap<String, Option<String>> = stored
            .as_ref()
            .and_then(|state| state["marks"].as_object())
            .map(|marks| marks.iter().map(|(path, mark)| (path.clone(), mark["revision"].as_str().map(str::to_string))).collect())
            .unwrap_or_default();
        // Ordered by path and read one past the ceiling, so the same marks come back on every
        // read, and having more than were carried is known rather than guessed at.
        let truncated = marks.len() > MAX_FILES_VIEWED_ROWS;
        Ok((marks.into_iter().take(MAX_FILES_VIEWED_ROWS).collect(), truncated))
    }

    /// The marks this environment keeps. A file the head still has at the revision it was cleared
    /// at is cleared; one the head has moved on from is reported as changed.
    pub(crate) async fn environment_files_viewed(&self, env: &Env, resolved: &Resolved) -> PrResult<PullRequestFilesViewedResult> {
        let viewer = self.required_viewer(env, resolved, "filesViewed").await?;
        let (marks, truncated) = self.read_marks(env, &Self::scope_key(resolved, &viewer), "filesViewed")?;
        if marks.is_empty() {
            return Ok(PullRequestFilesViewedResult { files: vec![], truncated });
        }
        // A host that won't say what its head has costs the marks their staleness, rather than
        // costing the reader every tick they've made.
        let paths: Vec<String> = marks.keys().cloned().collect();
        let revisions = self.file_revisions_of(env, resolved, &paths, false).await;
        let files = marks
            .into_iter()
            .map(|(path, stamped)| {
                // A mark stamped with no baseline holds until the reader presses it again. A
                // deleted file is answered as the empty revision, matching its stamp, so it stays
                // cleared; a path the host had no answer for also holds as cleared.
                let state = match (&stamped, revisions.as_ref().and_then(|revisions| revisions.get(&path))) {
                    (None, _) | (_, None) => PullRequestFileViewedState::Viewed,
                    (Some(stamp), Some(revision)) if stamp == revision => PullRequestFileViewedState::Viewed,
                    _ => PullRequestFileViewedState::Dismissed,
                };
                PullRequestFileViewed { path, state }
            })
            .collect();
        Ok(PullRequestFilesViewedResult { files, truncated })
    }

    /// One environment-backed write at a time per change request. A tick's host round trip is
    /// slower than an untick's, so unordered presses could finish out of order and leave a stale
    /// tick standing over a later untick.
    pub(crate) async fn environment_set_files_viewed(&self, env: &Env, resolved: &Resolved, files: &[PullRequestSetFileViewed]) -> PrResult<()> {
        let gate_key = format!("{} {} {}", resolved.project.project.id, resolved.project.remote, resolved.number);
        let gate = self.viewed.gates.lock().unwrap().entry(gate_key.clone()).or_default().clone();
        let result = {
            let _turn = gate.lock().await;
            self.write_marks(env, resolved, files).await
        };
        // Dropped once nobody is queued behind it, so a long-lived server does not keep a gate per
        // change request anyone has ever ticked a file in.
        let mut gates = self.viewed.gates.lock().unwrap();
        if gates.get(&gate_key).is_some_and(|held| Arc::strong_count(held) <= 2) {
            gates.remove(&gate_key);
        }
        result
    }

    async fn write_marks(&self, env: &Env, resolved: &Resolved, files: &[PullRequestSetFileViewed]) -> PrResult<()> {
        let viewer = self.required_viewer(env, resolved, "setFilesViewed").await?;
        // Only the files being cleared need a revision. An unticked one is about to lose its row.
        let cleared: Vec<String> = files.iter().filter(|file| file.viewed).map(|file| file.path.clone()).collect();
        let revisions = if cleared.is_empty() { None } else { self.file_revisions_of(env, resolved, &cleared, true).await };
        let viewed_at = self.clock.utc_now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let scope = Self::scope_key(resolved, &viewer);
        let store = env.store.clone();
        let files = files.to_vec();
        tokio::task::spawn_blocking(move || {
            store.transaction(|transaction| {
                let mut state = crate::persistence::read_projection(transaction, PROJECTION_KIND, &scope)?.unwrap_or_else(|| json!({"marks": {}}));
                let marks = state["marks"].as_object_mut().ok_or_else(|| StoreError::InvalidCommand("The viewed files record is not an object.".into()))?;
                for file in &files {
                    if file.viewed {
                        // A path left out of the answer stores with no baseline, not the empty
                        // revision, since the empty revision is itself an answer.
                        let revision = revisions.as_ref().and_then(|revisions| revisions.get(&file.path)).map_or(Value::Null, |revision| json!(revision));
                        marks.insert(file.path.clone(), json!({"revision": revision, "viewedAt": viewed_at}));
                    } else {
                        marks.shift_remove(&file.path);
                    }
                }
                write_projection(transaction, PROJECTION_KIND, &scope, &state)
            })
        })
        .await
        .map_err(|_| store_failure("setFilesViewed"))?
        .map_err(|_| store_failure("setFilesViewed"))
    }
}
