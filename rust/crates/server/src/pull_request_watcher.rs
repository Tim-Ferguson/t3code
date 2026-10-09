//! Wakes a thread's agent when a pull request it watches (`watch_pull_request`) needs a look: the port
//! of `orchestration-v2/PullRequestWatchReactor.ts`. A pass looks at each watched pull request once
//! for all the threads of a project that watch it, through the cached `PullRequestService`, so threads
//! share reads and every read is a background one: it never spends the interactive reserve of the
//! GraphQL quota, and a rate-limit pause is waited out for free. One timer, armed for the next pass on
//! the injected clock, drives it; with no watch anywhere it is not armed at all. Settling a thread
//! ends its watches, and a merged or closed pull request ends its own. What a pass saw is judged by
//! `pull_request_watch.rs`, and recorded with the wake it earns in one commit.
//!
//! Not ported: the host fingerprint (one batched read of every watched pull request that tells which
//! moved), so every pass takes the original's path for a host without one: a pull request is read
//! again when its sync snapshot moved, a check is running or mergeability is unknown, or after ten
//! quiet minutes.
use crate::{
    execution::ExecutionService,
    persistence::{Decision, Event, StoreError, read_projection},
    pull_request_service::{Env, PrError, PullRequestService},
    pull_request_watch::{self as watch_rules, Report},
    thread_pull_requests::{links, normalize, visible},
};
use chrono::SecondsFormat;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_vcs::clock::Clock;
use tokio::sync::{Notify, watch};

/// Minutes between passes. Checks take minutes, so a faster pass mostly spends the host's rate limit,
/// which every machine on the same account shares.
pub const SWEEP: Duration = Duration::from_secs(2 * 60);
/// Reads in a row that failed for a reason other than a rate limit before the watch ends.
const READ_FAILURE_LIMIT: u32 = 8;
/// Without a host fingerprint, a pull request with nothing in flight is read again only when its sync
/// snapshot moves, or after this long, for news the snapshot cannot show, such as a bot editing its review.
const QUIET_REREAD: Duration = Duration::from_secs(10 * 60);

/// One watch on one thread.
#[derive(Clone)]
struct Target {
    thread_id: String,
    project_id: String,
    thread: Value,
    link: Value,
    watch: Value,
}

impl Target {
    fn key(&self) -> Value {
        normalize(&self.link)
    }
    fn watch_key(&self) -> String {
        format!("{} {}", self.thread_id, self.watch["startedAt"].as_str().unwrap_or_default())
    }
    fn number(&self) -> u64 {
        self.link["number"].as_u64().unwrap_or(0)
    }
    fn url(&self) -> &str {
        self.link["url"].as_str().unwrap_or_default()
    }
}

/// Every thread of one project that watches one pull request, read once per pass.
struct Group {
    key: String,
    targets: Vec<Target>,
    /// What sync last saw of the pull request, to skip a read when nothing moved.
    fingerprint: String,
}

/// The last successful read of a pull request, kept in memory: a restart reads each once.
#[derive(Clone)]
struct LastRead {
    at: Duration,
    fingerprint: String,
    /// A check was still running or mergeability unknown, so the detail can move unannounced.
    in_flight: bool,
    /// The last activity read listed every remark; an incomplete one is read again.
    remarks_complete: bool,
    /// The watches this read evaluated; a watch started since takes its first look next pass.
    watches: HashSet<String>,
}

#[derive(Default)]
struct State {
    last_reads: HashMap<String, LastRead>,
    read_failures: HashMap<String, u32>,
    /// Replies past each long thread's first page, per pull request, so a pass pages a thread again only
    /// when the host's count of it moves.
    thread_tails: HashMap<String, HashMap<String, (u64, Vec<Value>)>>,
    next_sweep: Option<Duration>,
}

struct Inner {
    execution: ExecutionService,
    env: Env,
    service: Arc<PullRequestService>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    armed: Notify,
    stop: watch::Sender<bool>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct PullRequestWatcher {
    inner: Arc<Inner>,
}

/// Why a watch ends without a host read.
fn ends_without_read(target: &Target) -> Option<&'static str> {
    let thread = &target.thread;
    // A merged pull request cannot reopen. Settling and archiving end watches, and a subagent cannot
    // start one; a watch left from before those rules ends here.
    if target.link["snapshot"]["state"] == "merged" {
        Some("merged")
    } else if thread["settledOverride"] == "settled" || !thread["settledAt"].is_null() {
        Some("settled")
    } else if thread["lineage"]["relationshipToParent"] == "subagent" {
        Some("subagent")
    } else {
        None
    }
}

/// `[state, updatedAt, checksState, mergeability, reviewDecision, isDraft].join(" ")` of the sync snapshot.
fn snapshot_fingerprint(target: &Target) -> String {
    let snapshot = &target.link["snapshot"];
    if snapshot.is_null() {
        return String::new();
    }
    ["state", "updatedAt", "checksState", "mergeability", "reviewDecision", "isDraft"]
        .iter()
        .map(|field| match &snapshot[*field] {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn rate_limited(error: &PrError) -> bool {
    matches!(error, PrError::Operation { reason: Some(crate::pull_request_github::Reason::RateLimited), .. })
}

impl PullRequestWatcher {
    pub fn new(execution: ExecutionService, env: Env, service: Arc<PullRequestService>, clock: Arc<dyn Clock>) -> Self {
        Self { inner: Arc::new(Inner { execution, env, service, clock, state: Mutex::default(), armed: Notify::new(), stop: watch::channel(false).0, timer: Mutex::new(None) }) }
    }

    /// The watches the store holds, in thread order.
    fn targets(&self) -> Result<Vec<Target>, StoreError> {
        let store = &self.inner.execution.store;
        // The pre-filter reads the text; each projection is then looked at as data.
        let ids: Vec<String> = store.read(|connection| {
            let mut statement = connection.prepare("SELECT aggregate_id FROM rust_projections WHERE aggregate_kind='thread' AND state_json LIKE '%\"watch\":{%' ORDER BY json_extract(state_json,'$.createdAt'),aggregate_id")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })?;
        let mut targets = Vec::new();
        for id in ids {
            let Some(projection) = store.projection("thread", &id)? else { continue };
            let thread = projection["thread"].clone();
            if !thread["deletedAt"].is_null() || !thread["archivedAt"].is_null() {
                continue;
            }
            for link in visible(&links(&thread)) {
                if link.get("watch").is_some_and(|watch| !watch.is_null()) {
                    targets.push(Target { thread_id: id.clone(), project_id: thread["projectId"].as_str().unwrap_or_default().to_owned(), thread: thread.clone(), watch: link["watch"].clone(), link });
                }
            }
        }
        Ok(targets)
    }

    /// Records what a pass saw and wakes the agent with it, in one commit, only while the same watch
    /// is on: a stop or restart that lands during the host read wins.
    fn record(&self, target: &Target, next: Option<&Value>, wake: Option<(String, Value)>) -> Result<(), StoreError> {
        let execution = &self.inner.execution;
        let now = self.inner.clock.utc_now();
        let uuid = uuid::Uuid::new_v4();
        let command_id = format!("server:pr-watch:{}:{uuid}", target.thread_id);
        let started_at = target.watch["startedAt"].as_str().unwrap_or_default().to_owned();
        let key = target.key();
        let thread_id = target.thread_id.clone();
        let (receipt, _) = execution.store.dispatch_resolved(
            &command_id,
            "thread",
            "thread.pull-request-watch.sync",
            now,
            |_, _| Ok(thread_id.clone()),
            |transaction, _| {
                let reject = |cause: &str| Ok::<Decision, StoreError>(Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":"thread.pull-request-watch.sync","cause":cause})));
                let Some(mut projection) = read_projection(transaction, "thread", &thread_id)? else { return reject(&format!("Thread {thread_id} does not exist.")) };
                let held = links(&projection["thread"]).into_iter().find(|link| link["source"] != "stack-dismissed" && normalize(link) == key);
                // Same rule as a direct message.dispatch: a provider-native subagent takes no messages.
                let inactive = !projection["thread"]["archivedAt"].is_null()
                    || projection["thread"]["settledOverride"] == "settled"
                    || !projection["thread"]["settledAt"].is_null()
                    || crate::subagents::is_provider_native_subagent_thread(&projection["thread"]);
                if held.as_ref().is_none_or(|link| link["watch"]["startedAt"] != started_at.as_str()) || (wake.is_some() && inactive) {
                    return reject("The pull request watch ended or its thread settled while it was read.");
                }
                let mut thread = projection["thread"].clone();
                let mut events = Vec::new();
                if crate::thread::apply_pull_request_watch_sync(&mut thread, &key, &started_at, next) {
                    let thread = serde_json::from_value::<t3_contracts::AppThread>(thread).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    let thread = serde_json::to_value(thread).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    projection["thread"] = thread.clone();
                    events.push(Event {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        aggregate_kind: "thread".into(),
                        aggregate_id: thread_id.clone(),
                        occurred_at: now.to_rfc3339_opts(SecondsFormat::Millis, true),
                        command_id: Some(command_id.clone()),
                        causation_event_id: None,
                        correlation_id: Some(command_id.clone()),
                        event_type: "thread.pull-request-synced".into(),
                        payload: thread,
                        metadata: json!({}),
                    });
                }
                let mut effects = Vec::new();
                if let Some((text, notification)) = &wake {
                    let message = json!({"type":"message.dispatch","commandId":command_id,"threadId":thread_id,"messageId":format!("message:pr-watch:{uuid}"),"text":text,"notification":notification,"attachments":[],"dispatchMode":{"type":"queue_after_active"},"createdBy":"agent","creationSource":"server"});
                    let message = serde_json::from_value::<t3_contracts::ProviderCommand>(message)?.service_payload()?;
                    let model = &projection["thread"]["modelSelection"];
                    let driver = execution.providers.driver(model["instanceId"].as_str().unwrap_or_default()).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    match crate::execution::plan_message_in_transaction(&message, &projection, transaction, now, driver)? {
                        Decision::Accepted { events: planned, effects: planned_effects } => {
                            events.extend(planned);
                            effects = planned_effects;
                        }
                        other => return Ok(other),
                    }
                }
                Ok(Decision::Accepted { events, effects })
            },
            crate::thread::reduce,
        )?;
        if receipt.status != "accepted" {
            return Err(StoreError::InvalidCommand(receipt.error.map_or_else(|| "The pull request watch was refused.".into(), |error| error["cause"].as_str().unwrap_or("The pull request watch was refused.").to_owned())));
        }
        if wake.is_some() {
            execution.wake();
        }
        Ok(())
    }

    /// Runs one thread's step for each thread in a group, so one refusal does not skip the rest.
    /// Whether every step landed.
    fn each_target(&self, group: &Group, mut step: impl FnMut(&Target) -> Result<(), StoreError>) -> bool {
        let mut all = true;
        for target in &group.targets {
            if let Err(error) = step(target) {
                // A thread that did not get its update must not wait for the quiet reread.
                self.inner.state.lock().unwrap().last_reads.remove(&group.key);
                tracing::warn!(thread = %target.thread_id, pull_request = %group.key, %error, "pull request watch update failed");
                all = false;
            }
        }
        all
    }

    fn ended(&self, target: &Target, reason: &str) {
        tracing::info!(thread = %target.thread_id, pull_request = %format!("{}#{}", target.link["repository"].as_str().unwrap_or_default(), target.number()), reason, "pull request watch ended");
    }

    /// A watch that cannot read its pull request ends with a wake saying so, rather than showing
    /// "Watching" while it learns nothing.
    fn give_up(&self, target: &Target) -> Result<(), StoreError> {
        let number = target.number();
        let wake = (
            format!("T3 Code stopped watching pull request #{number} ({}) because it failed to read it from the host {READ_FAILURE_LIMIT} times in a row. Check it yourself, and call watch_pull_request to watch it again.", target.url()),
            json!({"source":{"kind":"monitor"},"outcome":"failed","summary":format!("#{number}: stopped watching, could not read it")}),
        );
        let recorded = self.record(target, None, Some(wake)).or_else(|_| self.record(target, None, None));
        self.ended(target, "unreadable");
        recorded
    }

    // A closed pull request can reopen, but the watch has nothing to report until then.
    fn closed(&self, target: &Target) -> Result<(), StoreError> {
        let number = target.number();
        let wake = (
            format!("Pull request #{number} ({}) was closed, so T3 Code stopped watching it. Call watch_pull_request if it reopens.", target.url()),
            json!({"source":{"kind":"monitor"},"outcome":"updated","summary":format!("#{number}: closed, stopped watching")}),
        );
        self.record(target, None, Some(wake))?;
        self.ended(target, "closed");
        Ok(())
    }

    fn reference(&self, group: &Group) -> Value {
        let first = &group.targets[0];
        let key = first.key();
        json!({"projectId":first.project_id,"host":key["host"],"repository":first.link["repository"],"number":first.link["number"],"allowStale":false})
    }

    /// Every remark of the pull request, or None when the read could have missed some: the remark
    /// watermark must never pass comments an incomplete read could not see.
    async fn read_remarks(&self, group_key: &str, reference: &Value, activity: &Value) -> Option<Vec<Value>> {
        // Comment cursors cannot account for missing threads. Only finish a truncated read when the host
        // confirms that every thread was listed.
        if activity["commentsTruncated"] == true && activity["reviewThreadsTruncated"] != false {
            return None;
        }
        let mut remarks: Vec<Value> = activity["comments"].as_array().cloned().unwrap_or_default();
        for thread in activity["reviewThreads"].as_array().into_iter().flatten() {
            let Some(mut cursor) = thread["nextCommentsCursor"].as_str().map(str::to_owned) else { continue };
            let (count, id) = (thread["commentCount"].as_u64().unwrap_or(0), thread["id"].as_str().unwrap_or_default().to_owned());
            let held = self.inner.state.lock().unwrap().thread_tails.get(group_key).and_then(|tails| tails.get(&id)).cloned();
            let tail = match held {
                Some((held_count, comments)) if held_count == count => comments,
                _ => {
                    let mut comments: Vec<(String, Value)> = Vec::new();
                    let mut cursors = HashSet::new();
                    loop {
                        if !cursors.insert(cursor.clone()) {
                            return None;
                        }
                        let input = serde_json::from_value::<t3_contracts::PullRequestThreadCommentsInput>(json!({"projectId":reference["projectId"],"host":reference["host"],"repository":reference["repository"],"number":reference["number"],"threadId":id,"cursor":cursor})).ok()?;
                        let page = match self.inner.service.thread_comments(&self.inner.env, &input).await {
                            Ok(page) => serde_json::to_value(page).ok()?,
                            Err(error) => {
                                tracing::warn!(?error, "pull request watch comment pagination failed");
                                return None;
                            }
                        };
                        for comment in page["comments"].as_array().into_iter().flatten() {
                            let mut comment = comment.clone();
                            comment["kind"] = json!("review-comment");
                            comment["path"] = thread["path"].clone();
                            comment["reviewState"] = Value::Null;
                            let comment_id = comment["id"].as_str().unwrap_or_default().to_owned();
                            match comments.iter_mut().find(|(known, _)| *known == comment_id) {
                                Some(slot) => slot.1 = comment,
                                None => comments.push((comment_id, comment)),
                            }
                        }
                        match page["nextCursor"].as_str() {
                            Some(next) => cursor = next.to_owned(),
                            None => break,
                        }
                    }
                    if (thread["comments"].as_array().map_or(0, Vec::len) + comments.len()) < count as usize {
                        return None;
                    }
                    let comments: Vec<Value> = comments.into_iter().map(|(_, comment)| comment).collect();
                    self.inner.state.lock().unwrap().thread_tails.entry(group_key.to_owned()).or_default().insert(id, (count, comments.clone()));
                    comments
                }
            };
            remarks.extend(tail);
        }
        remarks.sort_by(|left, right| left["createdAt"].as_str().unwrap_or_default().cmp(right["createdAt"].as_str().unwrap_or_default()));
        Some(remarks)
    }

    async fn read_group(&self, group: &Group) {
        let now = self.inner.clock.monotonic();
        let last = self.inner.state.lock().unwrap().last_reads.get(&group.key).cloned();
        // A watch takes its first look with everything.
        let read = match &last {
            None => true,
            Some(last) => group.targets.iter().any(|target| !last.watches.contains(&target.watch_key())) || last.in_flight || !last.remarks_complete || last.fingerprint != group.fingerprint || now.saturating_sub(last.at) >= QUIET_REREAD,
        };
        if !read {
            return;
        }
        let reference_json = self.reference(group);
        let Ok(reference) = serde_json::from_value::<t3_contracts::PullRequestRef>(reference_json.clone()) else { return };
        let (service, env) = (&self.inner.service, &self.inner.env);
        let (detail, activity) = tokio::join!(service.detail(env, &reference, None), service.activity(env, &reference, None));
        let read = detail.and_then(|detail| activity.map(|activity| (detail, activity)));
        let (detail, activity) = match read {
            Ok((detail, activity)) => (serde_json::to_value(detail).unwrap_or(Value::Null), serde_json::to_value(activity).unwrap_or(Value::Null)),
            Err(error) => {
                let mut state = self.inner.state.lock().unwrap();
                state.last_reads.remove(&group.key);
                // The host's pause refuses later reads without a request, so waiting it out is free.
                if rate_limited(&error) {
                    return;
                }
                let failures = state.read_failures.entry(group.key.clone()).or_insert(0);
                *failures += 1;
                let failures = *failures;
                drop(state);
                tracing::warn!(pull_request = %group.key, ?error, "pull request watch check failed");
                // The count stays until every stop lands, so a failed stop is tried again on the next
                // failed read, and a watch started after the stops begins from zero.
                if failures >= READ_FAILURE_LIMIT && self.each_target(group, |target| self.give_up(target)) {
                    self.inner.state.lock().unwrap().read_failures.remove(&group.key);
                }
                return;
            }
        };
        self.inner.state.lock().unwrap().read_failures.remove(&group.key);
        if detail["state"] != "open" {
            self.inner.state.lock().unwrap().last_reads.remove(&group.key);
            self.each_target(group, |target| {
                if detail["state"] == "closed" {
                    self.closed(target)
                } else {
                    self.record(target, None, None)?;
                    self.ended(target, "merged");
                    Ok(())
                }
            });
            return;
        }
        // Never advance the remark watermark past comments an incomplete read could have missed.
        let remarks = self.read_remarks(&group.key, &reference_json, &activity).await;
        let in_flight = detail["mergeability"] == "unknown" || detail["checks"].as_array().is_some_and(|checks| checks.iter().any(|check| check["status"] == "pending"));
        self.inner.state.lock().unwrap().last_reads.insert(
            group.key.clone(),
            LastRead { at: now, fingerprint: group.fingerprint.clone(), in_flight, remarks_complete: remarks.is_some(), watches: group.targets.iter().map(Target::watch_key).collect() },
        );
        let base_branch = detail["baseBranch"].as_str().unwrap_or_default().to_owned();
        self.each_target(group, |target| {
            let report: Report = watch_rules::evaluate(&target.watch, &detail, remarks.as_deref());
            if !report.changes.is_empty() {
                let (text, notification) = watch_rules::message(target.number(), target.url(), &base_branch, &report.next["headSha"], &report);
                self.record(target, (!report.exhausted).then_some(&report.next), Some((text, notification)))?;
                if report.exhausted {
                    self.ended(target, "comment-limit");
                }
                return Ok(());
            }
            if report.next == target.watch { Ok(()) } else { self.record(target, Some(&report.next), None) }
        });
    }

    /// One pass over every watched pull request.
    pub async fn sweep(&self) {
        let targets = match self.targets() {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(%error, "pull request watch sweep failed");
                return;
            }
        };
        let mut by_pull_request: Vec<(String, Vec<Target>)> = Vec::new();
        let mut ending = Vec::new();
        for target in targets {
            if let Some(reason) = ends_without_read(&target) {
                ending.push((target, reason));
                continue;
            }
            // Grouped per project too: each project reads through its own checkout, so one that cannot
            // read the pull request must not end another project's watches.
            let pull_request = target.key();
            let key = format!("{} {}/{}#{}", target.project_id, pull_request["host"].as_str().unwrap_or_default(), pull_request["repository"].as_str().unwrap_or_default(), pull_request["number"]);
            match by_pull_request.iter_mut().find(|(known, _)| *known == key) {
                Some((_, members)) => members.push(target),
                None => by_pull_request.push((key, vec![target])),
            }
        }
        {
            let mut state = self.inner.state.lock().unwrap();
            let present: HashSet<&String> = by_pull_request.iter().map(|(key, _)| key).collect();
            state.last_reads.retain(|key, _| present.contains(key));
            state.read_failures.retain(|key, _| present.contains(key));
            state.thread_tails.retain(|key, _| present.contains(key));
        }
        for (target, reason) in &ending {
            match self.record(target, None, None) {
                Ok(()) => self.ended(target, reason),
                Err(error) => tracing::warn!(thread = %target.thread_id, %error, "pull request watch stop failed"),
            }
        }
        for (key, targets) in by_pull_request {
            let fingerprint = targets.iter().map(snapshot_fingerprint).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("\n");
            let group = Group { key, targets, fingerprint };
            Box::pin(self.read_group(&group)).await;
        }
    }

    /// The pass that is due on the clock, if one is. The timer calls this; tests advance a manual clock
    /// and call it.
    pub async fn tick(&self) -> bool {
        let now = self.inner.clock.monotonic();
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.next_sweep.is_some_and(|due| now < due) {
                return false;
            }
            state.next_sweep = Some(now + SWEEP);
        }
        Box::pin(self.sweep()).await;
        true
    }

    /// How long until the next pass is due on the clock; None while no pass is armed, which is while no
    /// pull request is watched.
    pub fn next_pass_in(&self) -> Option<Duration> {
        let due = self.inner.state.lock().unwrap().next_sweep?;
        Some(due.saturating_sub(self.inner.clock.monotonic()))
    }

    /// Resolves once a pass is armed (a watch was committed, or one was already there).
    pub async fn wait_armed(&self) {
        loop {
            let armed = self.inner.armed.notified();
            if self.next_pass_in().is_some() {
                return;
            }
            armed.await;
        }
    }

    /// Arms the timer for a watch that was just started, unless one is already armed.
    fn arm(&self) {
        let now = self.inner.clock.monotonic();
        let mut state = self.inner.state.lock().unwrap();
        if state.next_sweep.is_none() {
            state.next_sweep = Some(now + SWEEP);
        }
        drop(state);
        self.inner.armed.notify_one();
    }

    /// Starts the single timer task. With no watch it sleeps until a watch is committed; with one it
    /// sleeps until the next pass is due on the injected clock, and passes while any watch remains.
    pub fn spawn_timer(&self) {
        let mut timer = self.inner.timer.lock().unwrap();
        if timer.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return;
        }
        self.inner.stop.send_replace(false);
        let watcher = self.clone();
        let mut events = self.inner.execution.store.subscribe();
        let mut stopped = self.inner.stop.subscribe();
        *timer = Some(tokio::spawn(async move {
            // A watch from before this run is picked up by the first pass.
            if watcher.targets().is_ok_and(|targets| !targets.is_empty()) {
                watcher.arm();
            }
            loop {
                let due = watcher.inner.state.lock().unwrap().next_sweep;
                let delay = due.map(|due| due.saturating_sub(watcher.inner.clock.monotonic()));
                tokio::select! {
                    _ = stopped.changed() => break,
                    event = events.recv() => {
                        // Only a committed watch can start the timer; a lagging receiver may have missed one.
                        let relevant = match event {
                            Ok(batch) => batch.iter().any(|stored| stored.event.event_type == "thread.pull-request-synced"),
                            Err(_) => true,
                        };
                        if relevant && watcher.inner.state.lock().unwrap().next_sweep.is_none() && watcher.targets().is_ok_and(|targets| !targets.is_empty()) {
                            watcher.arm();
                        }
                    }
                    _ = async { match delay { Some(delay) => tokio::time::sleep(delay).await, None => std::future::pending().await } } => {
                        if watcher.tick().await && watcher.targets().is_ok_and(|targets| targets.is_empty()) {
                            // Nothing left to watch: park until a watch is committed.
                            watcher.inner.state.lock().unwrap().next_sweep = None;
                        }
                    }
                }
            }
        }));
    }

    /// Stops the timer after the pass in progress.
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        let handle = self.inner.timer.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}
