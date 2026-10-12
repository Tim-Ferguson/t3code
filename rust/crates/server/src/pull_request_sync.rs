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
//! `request_sync` (`requestSync`) names a pull request to read again at once, even when its snapshot is terminal, in a pass of its own
//! that reads only what was named. It is asked for by `pullRequests.runAction` and `pullRequests.invalidate` after they act, by a client's
//! reading that sees a pull request change state, by an event that links a pull request with no snapshot yet, and by the end of a run that
//! ran `gh pr merge` or a like command (which sends no merge notification).
//!
//! A host that answers rate-limited is paused for the project that asked, until the time it names: the passes leave that host's pull
//! requests due and read them again once the pause has ended, rather than failing each of them every minute. A pass reports its failed
//! reads once per reason, not once per pull request.
use crate::{
    execution::ExecutionService,
    pull_request_github::Reason,
    pull_request_service::{Env, PrError, PullRequestService},
    thread_pull_requests::{normalize, visible},
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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

/// `threadPullRequestKeyOf` of a link or a key: the host-level identity requests are filed under.
fn pr_key(link: &Value) -> String {
    let key = normalize(link);
    format!("{}/{}#{}", key["host"].as_str().unwrap_or_default(), key["repository"].as_str().unwrap_or_default(), key["number"])
}

/// Shell commands that can merge or close a pull request without a merge notification (`PULL_REQUEST_CLOSE_COMMAND`).
fn closes_pull_request(command: &str) -> bool {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?-u:\b)(?:gh\s+pr|glab\s+mr)\s+(?:merge|close)(?-u:\b)").unwrap());
    PATTERN.is_match(command)
}

/// What a pass reads: every due pull request, or only those `request_sync` named.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Requested,
}

/// `rateLimitRetryAt`: when the host's pause ends, if a failed read says the host is rate limited.
fn rate_limit_retry_at(error: &PrError) -> Option<i64> {
    match error {
        PrError::Operation { reason: Some(Reason::RateLimited), retry_at, .. } => *retry_at,
        _ => None,
    }
}

/// `skipReason`: the failure's own words, which is what a pass groups its skips by.
fn skip_reason(error: &PrError) -> String {
    let wire = error.wire();
    match error {
        PrError::Operation { error, .. } => error.message(),
        PrError::Unavailable(_) => wire["message"].as_str().or(wire["_tag"].as_str()).unwrap_or("unavailable").to_owned(),
    }
}

struct Inner {
    execution: ExecutionService,
    env: Env,
    service: Arc<PullRequestService>,
    last_read: Mutex<HashMap<String, std::time::Instant>>,
    /// Pull requests whose stack read failed, or whose write did, until a pass completes them.
    retry_stacks: Mutex<HashSet<String>>,
    /// The pull requests `request_sync` named, by `pr_key`, with the generation of the request: a request that arrives while the host is
    /// being read belongs to the next pass.
    requested: Mutex<HashMap<String, u64>>,
    generation: AtomicU64,
    /// Requests wait for one queued pass, so a burst of links (an agent linking dozens of pull requests) is read together.
    requested_pass_queued: AtomicBool,
    /// Rate limit pauses by project and host, since each project reads with its own credential: until when (epoch milliseconds). A paused
    /// host refuses every read without being asked, so a pass leaves its pull requests due until the pause ends rather than failing each
    /// of them every minute.
    paused_until: Mutex<HashMap<String, i64>>,
    /// One pass at a time (the original's drainable worker).
    passes: tokio::sync::Mutex<()>,
    queued: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Threads whose current run ran a merge or close command, until that run ends.
    close_command_threads: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct PullRequestSync {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<PullRequestSync>);

impl PullRequestSync {
    pub fn new(execution: ExecutionService, env: Env, service: Arc<PullRequestService>) -> Self {
        Self {
            inner: Arc::new(Inner {
                execution,
                env,
                service,
                last_read: Mutex::default(),
                retry_stacks: Mutex::default(),
                requested: Mutex::default(),
                generation: AtomicU64::new(0),
                requested_pass_queued: AtomicBool::new(false),
                paused_until: Mutex::default(),
                passes: tokio::sync::Mutex::new(()),
                queued: Mutex::default(),
                close_command_threads: Mutex::default(),
            }),
        }
    }

    /// The sync the server runs (`spawn_timer` installs it), which the socket methods that change a pull request ask to read it.
    pub fn current(store: &crate::persistence::Store) -> Option<Self> {
        store.extension(Slot::default).0.get().cloned()
    }

    /// Forces the next pass to read this pull request again, even when its snapshot is terminal (`requestSync`). `key` names it by
    /// `host`, `repository` and `number`, as a thread's link does. While its host is rate limited, the read waits for the first pass after
    /// the pause. Needs a runtime: the pass is queued on one.
    pub fn request_sync(&self, key: &Value) {
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.requested.lock().unwrap().insert(pr_key(key), generation);
        if self.inner.requested_pass_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        let sync = self.clone();
        let task = tokio::spawn(async move {
            let _turn = sync.inner.passes.lock().await;
            // Requests from here on queue another pass; the ones already recorded are read by this.
            sync.inner.requested_pass_queued.store(false, Ordering::SeqCst);
            sync.pass(Scope::Requested).await;
        });
        let mut queued = self.inner.queued.lock().unwrap();
        queued.retain(|task| !task.is_finished());
        queued.push(task);
    }

    /// Waits for the passes `request_sync` queued.
    pub async fn drain(&self) {
        loop {
            let tasks: Vec<_> = self.inner.queued.lock().unwrap().drain(..).collect();
            if tasks.is_empty() {
                return;
            }
            for task in tasks {
                let _ = task.await;
            }
        }
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
        let _turn = self.inner.passes.lock().await;
        self.pass(Scope::All).await;
    }

    async fn pass(&self, scope: Scope) {
        let groups = self.groups();
        self.inner.last_read.lock().unwrap().retain(|key, _| groups.contains_key(key));
        self.inner.retry_stacks.lock().unwrap().retain(|key| groups.contains_key(key));
        {
            let held: HashSet<String> = groups.values().filter_map(|entries| entries.first()).map(|entry| pr_key(&entry.link)).collect();
            self.inner.requested.lock().unwrap().retain(|key, _| held.contains(key));
        }
        // Layers linked this pass, so two links of one thread that share a stack do not both add the same sibling.
        let mut linked_this_pass: HashSet<String> = HashSet::new();
        // Failed host reads by reason: how many, and the first pull request that failed that way.
        let mut skips: Vec<(String, usize, String)> = vec![];
        for (key, entries) in groups {
            let first = &entries[0];
            let requested = self.inner.requested.lock().unwrap().get(&pr_key(&first.link)).copied();
            let since = self.inner.last_read.lock().unwrap().get(&key).map(|read| read.elapsed());
            let wanted = requested.is_some() || self.inner.retry_stacks.lock().unwrap().contains(&key) || due(&entries, since);
            if !(wanted && (scope == Scope::All || requested.is_some())) {
                continue;
            }
            // Checked against the clock as each read starts, so a pause found earlier in this pass holds the rest, and one that ends
            // during the pass lets the rest through.
            let pause_key = format!("{}\0{}", first.project_id, normalize(&first.link)["host"].as_str().unwrap_or_default());
            if self.inner.paused_until.lock().unwrap().get(&pause_key).is_some_and(|until| *until > self.inner.service.now_ms()) {
                continue;
            }
            if let Err(error) = self.sync_group(&key, &entries, requested, &mut linked_this_pass).await {
                if let Some(retry_at) = rate_limit_retry_at(&error) {
                    let mut paused = self.inner.paused_until.lock().unwrap();
                    let until = paused.get(&pause_key).copied().unwrap_or(0).max(retry_at);
                    paused.insert(pause_key, until);
                }
                let reason = skip_reason(&error);
                match skips.iter_mut().find(|(known, ..)| *known == reason) {
                    Some((_, count, _)) => *count += 1,
                    None => skips.push((reason, 1, key)),
                }
            }
        }
        // A host failure such as a signed-out CLI fails every due pull request the same way, so a pass reports one line per reason rather
        // than one per pull request.
        for (reason, count, key) in skips {
            tracing::warn!(count, pull_request = %key, %reason, "pull request sync skipped");
        }
    }

    /// Reads one pull request and writes back what changed for each thread that links it. A failed host read is the error.
    async fn sync_group(&self, key: &str, entries: &[Entry], requested: Option<u64>, linked_this_pass: &mut HashSet<String>) -> Result<(), PrError> {
        let first = &entries[0];
        let normalized = normalize(&first.link);
        let reference = json!({"projectId":first.project_id,"host":normalized["host"],"repository":first.link["repository"],"number":first.link["number"]});
        let Ok(reference) = serde_json::from_value::<t3_contracts::PullRequestRef>(reference) else { return Ok(()) };
        // A requested read goes to the host, not to what the readers hold.
        if requested.is_some() {
            let invalidate = t3_contracts::PullRequestInvalidateInput { reference: Some(reference.clone()), files_viewed_only: None };
            self.inner.service.invalidate(&self.inner.env, &invalidate, false).await;
        }
        let summary = serde_json::to_value(self.inner.service.summary(&self.inner.env, &reference, None, false).await?).unwrap_or(Value::Null);
        let fields = snapshot_fields(&summary);
        // The stack is read when something about the pull request moved; a summary that says it is in no stack, for links that hold none,
        // already answers what the stack read would.
        let needs_stack = requested.is_some()
            || self.inner.retry_stacks.lock().unwrap().contains(key)
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
                    self.inner.retry_stacks.lock().unwrap().insert(key.to_owned());
                    // The pass records a rate limit's pause and holds the host's other reads until it ends.
                    if rate_limit_retry_at(&error).is_some() {
                        return Err(error);
                    }
                    tracing::warn!(pull_request = %key, "pull request stack lookup failed");
                    return Ok(());
                }
            }
        };
        if needs_stack {
            self.inner.retry_stacks.lock().unwrap().remove(key);
        }
        // The host answered, so the cadence clock ticks even if a write below is rejected.
        self.inner.last_read.lock().unwrap().insert(key.to_owned(), std::time::Instant::now());
        // A refresh requested while the host read was in flight belongs to the next pass.
        {
            let mut held = self.inner.requested.lock().unwrap();
            let name = pr_key(&first.link);
            if held.get(&name).copied() == requested {
                held.remove(&name);
            }
        }
        let now = Utc::now();
        for entry in entries {
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
                self.inner.retry_stacks.lock().unwrap().insert(key.to_owned());
                tracing::warn!(thread = %entry.thread_id, pull_request = %key, "pull request sync skipped");
            }
        }
        Ok(())
    }

    /// `refreshOpenLinks`: after a run that ran a merge or close command, the thread's open links are read fresh, so settlement does not
    /// wait for the next pass and the cached summary.
    fn refresh_open_links(&self, thread_id: &str) {
        let Ok(Some(projection)) = self.inner.execution.store.projection("thread", thread_id) else { return };
        for link in visible(&crate::thread_pull_requests::links(&projection["thread"])) {
            if link["snapshot"]["state"] == "open" {
                self.request_sync(&link);
            }
        }
    }

    /// What a committed event asks of the sync (the original's `start` streams).
    fn on_event(&self, event: &crate::persistence::Event) {
        let thread_id = event.payload["threadId"].as_str().unwrap_or(&event.aggregate_id);
        match event.event_type.as_str() {
            // A link with no snapshot is read soon.
            "thread.pull-request-synced" => {
                for link in visible(&crate::thread_pull_requests::links(&event.payload)) {
                    if link["snapshot"].is_null() {
                        self.request_sync(&link);
                    }
                }
            }
            // An agent can merge or close its pull request from a shell (`gh pr merge`), which sends no merge notification. Other runs add
            // no host reads.
            "turn-item.updated" => {
                if event.payload["type"] == "command_execution" && event.payload["input"].as_str().is_some_and(closes_pull_request) {
                    self.inner.close_command_threads.lock().unwrap().insert(thread_id.to_owned());
                }
            }
            "run.updated" => {
                let terminal = matches!(event.payload["status"].as_str(), Some("completed" | "failed" | "cancelled" | "interrupted" | "rolled_back"));
                if terminal && self.inner.close_command_threads.lock().unwrap().remove(thread_id) {
                    self.refresh_open_links(thread_id);
                }
            }
            _ => {}
        }
    }

    /// Sweeps once a minute for as long as the server runs, and reads the pull requests that `request_sync` is asked for: by the events
    /// that link a pull request, end a run that ran a merge command, and by a client's reading that sees a pull request change state.
    /// Installs itself as the server's sync.
    pub fn spawn_timer(&self) {
        let _ = self.inner.execution.store.extension(Slot::default).0.set(self.clone());
        let sync = self.clone();
        tokio::spawn(async move {
            loop {
                sync.sweep().await;
                tokio::time::sleep(SWEEP).await;
            }
        });
        let (sync, mut events) = (self.clone(), self.inner.execution.store.subscribe());
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(batch) => batch.iter().for_each(|stored| sync.on_event(&stored.event)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        let (sync, mut changes) = (self.clone(), self.inner.service.subscribe_state_changes());
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(key) => sync.request_sync(&key),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
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
    fn a_shell_command_that_merges_or_closes_a_pull_request_is_known_by_its_words() {
        for command in ["gh pr merge 12 --squash", "cd x && gh  pr\tclose 3", "glab mr merge 4", "echo done; gh pr merge"] {
            assert!(closes_pull_request(command), "{command}");
        }
        for command in ["gh pr view 12", "gh pr merged", "ggh pr merge", "gh pr list --state merge", "glab mr create"] {
            assert!(!closes_pull_request(command), "{command}");
        }
    }

    #[test]
    fn requests_are_filed_under_the_host_level_identity_of_the_pull_request() {
        let link = json!({"host":"GitHub.com","repository":"Acme/Web","number":7,"url":"https://github.com/acme/web/pull/7"});
        assert_eq!(pr_key(&link), "github.com/acme/web#7");
        assert_eq!(pr_key(&json!({"host":"github.com","repository":"acme/web","number":7})), pr_key(&link));
    }

    #[test]
    fn only_a_rate_limited_failure_that_names_its_end_pauses_the_host() {
        let limited = |retry_at| PrError::Operation { error: t3_contracts::PullRequestOperationError::new("summary", "slow down"), reason: Some(Reason::RateLimited), retry_at };
        assert_eq!(rate_limit_retry_at(&limited(Some(5_000))), Some(5_000));
        assert_eq!(rate_limit_retry_at(&limited(None)), None);
        let failed = PrError::Operation { error: t3_contracts::PullRequestOperationError::new("summary", "boom"), reason: Some(Reason::Failed), retry_at: Some(5_000) };
        assert_eq!(rate_limit_retry_at(&failed), None);
        assert_eq!(skip_reason(&failed), "Pull request operation summary failed: boom");
        assert_eq!(rate_limit_retry_at(&PrError::unsupported()), None);
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
