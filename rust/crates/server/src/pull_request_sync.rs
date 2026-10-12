//! Keeps the snapshot of every thread's pull request link current (`PullRequestSyncReactor`): the state,
//! title, branches and review of the pull request as the host reports it. One sweep a minute reads each
//! pull request that linked threads still watch once, however many threads share it, and writes back, as
//! `thread.pull-request-link.sync`, only what changed. Without it a merge is noticed by whoever else looks
//! at the pull request, and a settled-on-merge thread waits.
//!
//! A link is due while it has no snapshot or its pull request is open; a closed one is read again every
//! fifteen minutes, since it can reopen, and a merged one is not read again. A settled thread stops
//! watching: its pull request is read for the threads that are still active, and takes the fresh snapshot
//! when it shares one.
//!
//! A link also holds the host-native stack its pull request belongs to (`stack`, what the UI draws the saved stack from). The stack
//! is read when the link has no snapshot, the snapshot changed, or the summary says the pull request moved to another stack; a link
//! of an active thread then gains one `source: "stack"` link per layer it does not hold (a dismissed layer counts as held, so it is
//! not brought back).
//!
//! Not ported: the refresh a shell command that merges asks for (`requestSync`), and the pause a rate-limited host asks for (a failed
//! stack read is only retried on the next pass).
use crate::{
    execution::ExecutionService,
    pull_request_service::{Env, PullRequestService},
    thread_pull_requests::{normalize, visible},
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

const SWEEP: Duration = Duration::from_secs(60);
/// A closed pull request can reopen on the host.
const SLOW_SYNC: Duration = Duration::from_secs(15 * 60);

struct Entry {
    thread_id: String,
    project_id: String,
    link: Value,
    unsettled: bool,
    /// Every link the thread holds, tombstones included: a layer already filed under the thread is not linked again.
    thread_links: Arc<Vec<Value>>,
}

/// `ThreadPullRequestStack` of a host's `PullRequestStack`: only the fields a thread keeps, and the kind `native`.
fn thread_stack(stack: &t3_contracts::PullRequestStack) -> Value {
    let stack = serde_json::to_value(stack).unwrap_or(Value::Null);
    let layers: Vec<Value> = stack["layers"].as_array().into_iter().flatten().map(|layer| json!({"number":layer["number"],"headBranch":layer["headBranch"],"state":layer["state"]})).collect();
    json!({"kind":"native","id":stack["id"],"number":stack["number"],"url":stack["url"],"base":stack["base"],"layers":layers})
}

/// `stacksEqual`.
fn same_stack(left: &Value, right: &Value) -> bool {
    if left.is_null() || right.is_null() {
        return left.is_null() && right.is_null();
    }
    let (left_layers, right_layers) = (left["layers"].as_array().map_or(&[][..], Vec::as_slice), right["layers"].as_array().map_or(&[][..], Vec::as_slice));
    ["kind", "id", "number", "url", "base"].iter().all(|field| left[*field] == right[*field])
        && left_layers.len() == right_layers.len()
        && left_layers.iter().zip(right_layers).all(|(a, b)| ["number", "headBranch", "state"].iter().all(|field| a[*field] == b[*field]))
}

/// The snapshot fields a host summary gives a link (`snapshotFieldsOf`).
pub(crate) fn snapshot_fields(summary: &Value) -> Value {
    let mut fields = json!({"state":summary["state"],"title":summary["title"],"headBranch":summary["headBranch"],"baseBranch":summary["baseBranch"],"isDraft":summary["isDraft"].as_bool().unwrap_or(false),"updatedAt":summary["updatedAt"],"closedAt":summary.get("closedAt").cloned().unwrap_or(Value::Null),"mergedAt":summary.get("mergedAt").cloned().unwrap_or(Value::Null)});
    for field in ["author", "additions", "deletions", "changedFiles", "reviewDecision", "checksState", "mergeability"] {
        if let Some(value) = summary.get(field).filter(|value| !value.is_null() || field == "author") {
            fields[field] = value.clone();
        }
    }
    fields
}

/// Whether the snapshot a link holds already says what the host did (`snapshotFieldsEqual`).
pub(crate) fn same_snapshot(snapshot: &Value, fields: &Value) -> bool {
    let null_if_absent = |value: &Value, field: &str| value.get(field).cloned().unwrap_or(Value::Null);
    ["state", "title", "headBranch", "baseBranch", "isDraft", "updatedAt", "closedAt", "mergedAt", "reviewDecision", "checksState"].iter().all(|field| null_if_absent(snapshot, field) == null_if_absent(fields, field))
        && null_if_absent(&snapshot["author"], "login") == null_if_absent(&fields["author"], "login")
        && null_if_absent(&snapshot["author"], "avatarUrl") == null_if_absent(&fields["author"], "avatarUrl")
        && ["additions", "deletions", "changedFiles", "mergeability"].iter().all(|field| snapshot.get(*field) == fields.get(*field))
}

/// `isDue`: whether the sweep reads this pull request now.
fn due(entries: &[Entry], since_last_read: Option<Duration>) -> bool {
    if entries.iter().any(|entry| entry.link["snapshot"].is_null()) {
        return true;
    }
    // Settled threads stop watching their pull requests.
    let active: Vec<&Entry> = entries.iter().filter(|entry| entry.unsettled).collect();
    if active.iter().all(|entry| entry.link["snapshot"]["state"] == "merged") {
        return false;
    }
    if active.iter().any(|entry| entry.link["snapshot"]["state"] == "open") {
        return true;
    }
    since_last_read.is_none_or(|elapsed| elapsed >= SLOW_SYNC)
}

struct Inner {
    execution: ExecutionService,
    env: Env,
    service: Arc<PullRequestService>,
    last_read: Mutex<HashMap<String, std::time::Instant>>,
    /// Pull requests whose stack read failed, or whose write did, until a pass completes them.
    retry_stacks: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct PullRequestSync {
    inner: Arc<Inner>,
}

impl PullRequestSync {
    pub fn new(execution: ExecutionService, env: Env, service: Arc<PullRequestService>) -> Self {
        Self { inner: Arc::new(Inner { execution, env, service, last_read: Mutex::default(), retry_stacks: Mutex::default() }) }
    }

    /// The links of active threads, grouped by pull request.
    fn groups(&self) -> HashMap<String, Vec<Entry>> {
        let mut groups: HashMap<String, Vec<Entry>> = HashMap::new();
        for projection in self.inner.execution.store.projections("thread").unwrap_or_default() {
            let thread = &projection["thread"];
            if !thread["deletedAt"].is_null() || !thread["archivedAt"].is_null() {
                continue;
            }
            let links = crate::thread_pull_requests::links(thread);
            let thread_links = Arc::new(links.clone());
            let unsettled = thread["settledOverride"] != "settled" && thread["settledAt"].is_null();
            for link in visible(&links) {
                let key = normalize(&link);
                groups.entry(format!("{}:{}/{}#{}", thread["projectId"].as_str().unwrap_or_default(), key["host"].as_str().unwrap_or_default(), key["repository"].as_str().unwrap_or_default(), key["number"])).or_default().push(Entry { thread_id: thread["id"].as_str().unwrap_or_default().to_owned(), project_id: thread["projectId"].as_str().unwrap_or_default().to_owned(), link, unsettled, thread_links: thread_links.clone() });
            }
        }
        groups
    }

    /// One pass: every due pull request is read once and its changed snapshots are written back.
    pub async fn sweep(&self) {
        let groups = self.groups();
        self.inner.last_read.lock().unwrap().retain(|key, _| groups.contains_key(key));
        self.inner.retry_stacks.lock().unwrap().retain(|key| groups.contains_key(key));
        // Layers linked this pass, so two links of one thread that share a stack do not both add the same sibling.
        let mut linked_this_pass: HashSet<String> = HashSet::new();
        for (key, entries) in groups {
            let since = self.inner.last_read.lock().unwrap().get(&key).map(|read| read.elapsed());
            if !due(&entries, since) && !self.inner.retry_stacks.lock().unwrap().contains(&key) {
                continue;
            }
            let first = &entries[0];
            let normalized = normalize(&first.link);
            let reference = json!({"projectId":first.project_id,"host":normalized["host"],"repository":first.link["repository"],"number":first.link["number"]});
            let Ok(reference) = serde_json::from_value::<t3_contracts::PullRequestRef>(reference) else { continue };
            let summary = match self.inner.service.summary(&self.inner.env, &reference, None, false).await {
                Ok(summary) => serde_json::to_value(summary).unwrap_or(Value::Null),
                Err(error) => {
                    tracing::warn!(pull_request = %key, ?error, "pull request sync skipped");
                    continue;
                }
            };
            let fields = snapshot_fields(&summary);
            // The stack is read when something about the pull request moved; a summary that says it is in no stack, for links that
            // hold none, already answers what the stack read would.
            let needs_stack = self.inner.retry_stacks.lock().unwrap().contains(&key)
                || entries.iter().any(|entry| entry.link["snapshot"].is_null() || !same_snapshot(&entry.link["snapshot"], &fields) || (summary.get("stack").is_some() && entry.link["stack"]["number"] != summary["stack"]["number"]));
            let known_unstacked = summary.get("stack").is_some_and(Value::is_null) && entries.iter().all(|entry| entry.link["stack"].is_null());
            // `Some(None)` is a pull request in no stack; `None` is a stack that was not read.
            let fetched: Option<Option<Value>> = if !needs_stack {
                None
            } else if known_unstacked {
                Some(None)
            } else {
                match self.inner.service.stack(&self.inner.env, &reference, None, false).await {
                    Ok(stack) => Some(stack.as_ref().map(thread_stack)),
                    Err(error) => {
                        tracing::warn!(pull_request = %key, ?error, "pull request stack lookup failed");
                        self.inner.retry_stacks.lock().unwrap().insert(key);
                        continue;
                    }
                }
            };
            if needs_stack {
                self.inner.retry_stacks.lock().unwrap().remove(&key);
            }
            // The host answered, so the cadence clock ticks even if a write below is rejected.
            self.inner.last_read.lock().unwrap().insert(key.clone(), std::time::Instant::now());
            let now = Utc::now();
            for entry in &entries {
                let current_stack = entry.link.get("stack").cloned().unwrap_or(Value::Null);
                let next_stack = match &fetched {
                    Some(stack) => stack.clone().unwrap_or(Value::Null),
                    None => current_stack.clone(),
                };
                let changed = entry.link["snapshot"].is_null() || !same_snapshot(&entry.link["snapshot"], &fields) || !same_stack(&current_stack, &next_stack);
                // Siblings are filed before a terminal snapshot can settle the thread. A settled thread that shares the pull request
                // takes the fresh snapshot but gains no links.
                let layers = if entry.unsettled { next_stack["layers"].as_array().filter(|_| fetched.is_some()).cloned().unwrap_or_default() } else { vec![] };
                let host = normalize(&entry.link)["host"].clone();
                let mut failed = false;
                for layer in layers {
                    let layer_key = json!({"host":host,"repository":entry.link["repository"],"number":layer["number"]});
                    let normalized = normalize(&layer_key);
                    let dedupe = format!("{}:{}/{}#{}", entry.thread_id, normalized["host"].as_str().unwrap_or_default(), normalized["repository"].as_str().unwrap_or_default(), normalized["number"]);
                    if linked_this_pass.contains(&dedupe) || entry.thread_links.iter().any(|existing| normalize(existing) == normalized) {
                        continue;
                    }
                    let Some(url) = entry.link["url"].as_str().zip(layer["number"].as_u64()).and_then(|(url, number)| crate::thread_pull_requests::sibling_pull_request_url(url, number)) else { continue };
                    let command = json!({"type":"thread.pull-request.link","commandId":format!("server:pr-stack-link:{}:{}",entry.thread_id,uuid::Uuid::new_v4()),"threadId":entry.thread_id,"host":layer_key["host"],"repository":layer_key["repository"],"number":layer_key["number"],"url":url,"source":"stack"});
                    let store = self.inner.execution.store.clone();
                    let outcome = tokio::task::spawn_blocking(move || crate::thread::ThreadService::new(store).dispatch(&command, now)).await;
                    if !matches!(outcome, Ok(Ok(_))) {
                        failed = true;
                        break;
                    }
                    linked_this_pass.insert(dedupe);
                }
                if !failed && changed {
                    let mut snapshot = fields.clone();
                    snapshot["syncedAt"] = json!(now.to_rfc3339_opts(SecondsFormat::Millis, true));
                    let command = json!({"type":"thread.pull-request-link.sync","commandId":format!("server:pr-sync:{}:{}",entry.thread_id,uuid::Uuid::new_v4()),"threadId":entry.thread_id,"host":host,"repository":entry.link["repository"],"number":entry.link["number"],"snapshot":snapshot,"stack":next_stack});
                    let store = self.inner.execution.store.clone();
                    let outcome = tokio::task::spawn_blocking(move || crate::service_commands::dispatch(&store, None, &command, now)).await;
                    failed = !matches!(outcome, Ok(Ok(_)));
                }
                if failed {
                    self.inner.retry_stacks.lock().unwrap().insert(key.clone());
                    tracing::warn!(thread = %entry.thread_id, pull_request = %key, "pull request sync skipped");
                }
            }
        }
    }

    /// Sweeps once a minute for as long as the server runs.
    pub fn spawn_timer(&self) {
        let sync = self.clone();
        tokio::spawn(async move {
            loop {
                sync.sweep().await;
                tokio::time::sleep(SWEEP).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(snapshot: Value, unsettled: bool) -> Entry {
        Entry { thread_id: "t".into(), project_id: "p".into(), link: json!({"snapshot":snapshot}), unsettled, thread_links: Arc::default() }
    }

    #[test]
    fn a_stack_is_kept_as_a_thread_keeps_it_and_compared_by_what_a_link_shows() {
        let stack: t3_contracts::PullRequestStack = serde_json::from_value(json!({"id":"s1","number":4,"url":"https://github.com/acme/web/pull/4","base":"main","layers":[{"number":4,"title":"Base","isDraft":false,"headSha":"abc","headBranch":"a","state":"open"},{"number":5,"headBranch":"b","state":"merged"}]})).unwrap();
        let kept = thread_stack(&stack);
        assert_eq!(kept, json!({"kind":"native","id":"s1","number":4,"url":"https://github.com/acme/web/pull/4","base":"main","layers":[{"number":4,"headBranch":"a","state":"open"},{"number":5,"headBranch":"b","state":"merged"}]}));
        assert!(same_stack(&kept, &kept.clone()) && same_stack(&Value::Null, &Value::Null));
        assert!(!same_stack(&kept, &Value::Null) && !same_stack(&Value::Null, &kept));
        let mut merged = kept.clone();
        merged["layers"][0]["state"] = json!("merged");
        let mut shorter = kept.clone();
        shorter["layers"].as_array_mut().unwrap().pop();
        assert!(!same_stack(&kept, &merged) && !same_stack(&kept, &shorter));
    }

    #[test]
    fn a_link_is_read_until_it_is_merged_and_a_closed_one_only_now_and_then() {
        let open = json!({"state":"open"});
        let merged = json!({"state":"merged"});
        let closed = json!({"state":"closed"});
        assert!(due(&[entry(Value::Null, true)], Some(Duration::ZERO)));
        assert!(due(&[entry(open.clone(), true)], Some(Duration::ZERO)));
        assert!(!due(&[entry(merged.clone(), true)], None));
        assert!(!due(&[entry(closed.clone(), true)], Some(Duration::from_secs(60))));
        assert!(due(&[entry(closed.clone(), true)], Some(SLOW_SYNC)));
        assert!(due(&[entry(closed, true)], None));
        // A settled thread's open snapshot does not keep the pull request being read.
        assert!(!due(&[entry(open, false), entry(merged, true)], None));
    }

    #[test]
    fn a_snapshot_is_current_when_the_host_says_nothing_new() {
        let summary = json!({"state":"open","title":"T","headBranch":"h","baseBranch":"main","updatedAt":"2026-01-01T00:00:00.000Z","author":{"login":"a","avatarUrl":"u"},"additions":3,"reviewDecision":"approved"});
        let fields = snapshot_fields(&summary);
        assert_eq!((fields["isDraft"].clone(), fields["closedAt"].clone(), fields["mergedAt"].clone()), (json!(false), Value::Null, Value::Null));
        let mut snapshot = fields.clone();
        snapshot["syncedAt"] = json!("2026-01-02T00:00:00.000Z");
        assert!(same_snapshot(&snapshot, &fields));
        let mut merged = summary.clone();
        merged["state"] = json!("merged");
        assert!(!same_snapshot(&snapshot, &snapshot_fields(&merged)));
        let mut retitled = summary;
        retitled["author"]["login"] = json!("b");
        assert!(!same_snapshot(&snapshot, &snapshot_fields(&retitled)));
    }
}
