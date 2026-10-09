//! Releasing a provider process nobody is using. A thread's actor starts its provider CLI on the first
//! turn and, without this, keeps it for the life of the server. Port of the idle release in the original's
//! `ProviderSessionManager`: a session with no turn running and no background work pending is released
//! once it has been idle for 30 minutes; work still pending keeps it for further windows, but never for
//! more than four hours in all. A released session is persisted as stopped (by the actor's own cleanup)
//! and its still-unanswered live requests expire; the next message starts a new process, which resumes
//! the native conversation exactly as after a server restart.
//!
//! The effect loop owns one [`Tracker`] and arms a single timer for the earliest expiry on the injected
//! clock. The clock is the scheduler's [`SchedulerClock`], not the VCS `Clock`, because only it can
//! sleep until a deadline, and a manual one resolves that sleep when a test moves time.
use crate::{
    codex_runtime::{ActorLifetime, Work, commit, projection},
    persistence::Store,
    scheduled_tasks::{SchedulerClock, SystemClock},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc;

/// `DEFAULT_IDLE_TIMEOUT_MS`.
pub const IDLE_TIMEOUT_MS: i64 = 30 * 60 * 1000;
/// `DEFAULT_MAX_IDLE_PIN_MS`: how long pending background work may defer a release.
pub const MAX_PIN_MS: i64 = 4 * 60 * 60 * 1000;

#[derive(Clone)]
struct Settings {
    clock: Arc<dyn SchedulerClock>,
    timeout_ms: i64,
    max_pin_ms: i64,
}

/// The clock and windows idle release runs on; shared by every clone of the provider registry.
#[derive(Clone)]
pub struct IdleRelease {
    settings: Arc<RwLock<Settings>>,
    /// How many ended windows have been decided so far, so a test can wait for a decision it cannot see.
    decided: Arc<tokio::sync::watch::Sender<u64>>,
    /// The newest committed event the effect loop has taken into account.
    seen: Arc<tokio::sync::watch::Sender<u64>>,
}
impl Default for IdleRelease {
    fn default() -> Self {
        let settings = Settings { clock: Arc::new(SystemClock), timeout_ms: IDLE_TIMEOUT_MS, max_pin_ms: MAX_PIN_MS };
        Self { settings: Arc::new(RwLock::new(settings)), decided: Arc::new(tokio::sync::watch::channel(0).0), seen: Arc::new(tokio::sync::watch::channel(0).0) }
    }
}
impl IdleRelease {
    /// For tests: idle time passes when `clock` is moved.
    pub fn configure(&self, clock: Arc<dyn SchedulerClock>, timeout_ms: i64, max_pin_ms: i64) {
        *self.settings.write().unwrap() = Settings { clock, timeout_ms: timeout_ms.max(1), max_pin_ms: max_pin_ms.max(0) };
    }
    /// The number of idle windows decided so far: released, or given another window.
    pub fn decisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.decided.subscribe()
    }
    /// Resolves once the effect loop has taken every event committed so far into account. A test moves the
    /// clock only after this, or an event still on its way would start the idle window at the new time.
    pub async fn caught_up(&self, store: &Store) {
        let Ok(latest) = store.latest_sequence() else { return };
        let _ = self.seen.subscribe().wait_for(|seen| *seen >= latest).await;
    }
    pub(crate) fn saw(&self, sequence: u64) {
        self.seen.send_if_modified(|seen| if sequence > *seen { *seen = sequence; true } else { false });
    }
    fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Nothing to lose: release.
    Idle,
    /// A turn is running or queued to start, or an effect for the thread has not finished.
    Busy,
    /// Only background work (commands, tools, subagents, a provider's task roster) is pending.
    Pending,
}

/// Whether a thread's provider process may go, from its persisted state. `effects_outstanding` is an
/// effect of the thread (a turn start, a rollback, a steer) that is queued or running.
pub(crate) fn verdict(projection: &Value, effects_outstanding: bool) -> Verdict {
    if effects_outstanding {
        return Verdict::Busy;
    }
    // A held queue waits for the user, not for the process.
    let runs = projection["runs"].as_array().map_or(&[][..], Vec::as_slice);
    let busy = runs.iter().any(|run| match run["status"].as_str() {
        Some("queued") => run["queueHeld"] != true,
        Some("preparing" | "starting" | "running" | "waiting") => true,
        _ => false,
    });
    if busy {
        return Verdict::Busy;
    }
    // A pull request watch lives in the server, not in the provider's process.
    let pending = crate::background_work::derive_for(projection).iter().any(|task| !task["taskId"].as_str().is_some_and(|id| id.starts_with("pull-request-watch:")));
    if pending { Verdict::Pending } else { Verdict::Idle }
}

struct Entry {
    /// When the thread last did anything; the release window runs from here.
    since_ms: i64,
    /// When pending work first deferred a release; the pin ends `max_pin_ms` after.
    pinned_since_ms: Option<i64>,
}

/// The threads whose actor may be released, and when each is next due.
#[derive(Default)]
pub(crate) struct Tracker {
    threads: HashMap<String, Entry>,
}
impl Tracker {
    /// The thread's actor was given work or its provider reported something.
    pub(crate) fn touch(&mut self, thread: &str, now_ms: i64) {
        self.threads.entry(thread.to_owned()).and_modify(|entry| entry.since_ms = now_ms).or_insert(Entry { since_ms: now_ms, pinned_since_ms: None });
    }
    /// Activity on a thread already tracked; a thread without an actor is not tracked.
    pub(crate) fn touch_if_tracked(&mut self, thread: &str, now_ms: i64) {
        if let Some(entry) = self.threads.get_mut(thread) {
            entry.since_ms = now_ms;
        }
    }
    fn forget(&mut self, thread: &str) {
        self.threads.remove(thread);
    }
    /// The earliest moment any thread is due, for the one timer.
    pub(crate) fn next_deadline(&self, timeout_ms: i64) -> Option<i64> {
        self.threads.values().map(|entry| entry.since_ms + timeout_ms).min()
    }
    fn due(&self, now_ms: i64, timeout_ms: i64) -> Vec<String> {
        self.threads.iter().filter(|(_, entry)| entry.since_ms + timeout_ms <= now_ms).map(|(thread, _)| thread.clone()).collect()
    }
    /// What to do with a thread whose window ended: another window, or release now.
    fn decide(&mut self, thread: &str, verdict: Verdict, now_ms: i64, max_pin_ms: i64) -> bool {
        let Some(entry) = self.threads.get_mut(thread) else { return false };
        match verdict {
            Verdict::Idle => true,
            Verdict::Busy => {
                entry.since_ms = now_ms;
                entry.pinned_since_ms = None;
                false
            }
            Verdict::Pending => {
                let pinned = *entry.pinned_since_ms.get_or_insert(now_ms);
                if now_ms - pinned < max_pin_ms {
                    entry.since_ms = now_ms;
                    false
                } else {
                    tracing::warn!(thread, pinned_for_ms = now_ms - pinned, "Idle release pin expired; releasing a provider session with pending background work");
                    true
                }
            }
        }
    }
}

impl IdleRelease {
    pub(crate) fn now_ms(&self) -> i64 {
        self.settings().clock.now_ms()
    }
    /// Resolves when the earliest tracked thread's window ends on the clock; never when none is tracked.
    pub(crate) fn expiry(&self, tracker: &Tracker) -> futures_util::future::BoxFuture<'static, ()> {
        let settings = self.settings();
        match tracker.next_deadline(settings.timeout_ms) {
            Some(deadline) => settings.clock.sleep_until(deadline),
            None => Box::pin(std::future::pending()),
        }
    }
}

fn effects_outstanding(store: &Store, thread: &str) -> bool {
    store
        .read(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM rust_effect_outbox WHERE thread_id=?1 AND status IN ('pending','running')", [thread], |row| row.get::<_, i64>(0))?))
        .map_or(true, |count| count > 0)
}

/// Releases every tracked thread whose window ended and whose session is idle. Runs in the effect loop,
/// which is also what hands effects to actors, so no effect can reach an actor between the verdict and
/// the stop; one committed meanwhile starts a fresh actor.
pub(crate) async fn release_due(
    idle: &IdleRelease,
    tracker: &mut Tracker,
    store: &Store,
    actors: &mut HashMap<String, mpsc::Sender<Work>>,
    served: &mut HashMap<String, ActorLifetime>,
) {
    let settings = idle.settings();
    let now = settings.clock.now_ms();
    eprintln!("IDLEDEBUG release_due now={now} tracked={:?}", tracker.threads.iter().map(|(t, e)| (t.clone(), e.since_ms)).collect::<Vec<_>>());
    for thread in tracker.due(now, settings.timeout_ms) {
        let live = actors.get(&thread).is_some_and(|actor| !actor.is_closed());
        if !live {
            tracker.forget(&thread);
            continue;
        }
        let verdict = match projection(store, &thread) {
            Ok(view) => verdict(&view, effects_outstanding(store, &thread)),
            // A deleted thread has nothing to keep a process for.
            Err(_) => Verdict::Idle,
        };
        let release = tracker.decide(&thread, verdict, now, settings.max_pin_ms);
        if !release {
            idle.decided.send_modify(|count| *count += 1);
            continue;
        }
        tracker.forget(&thread);
        if let Some(lifetime) = served.remove(&thread) {
            lifetime.stop_and_wait().await;
        }
        actors.remove(&thread);
        if let Err(error) = expire_live_requests(store, &thread) {
            tracing::warn!(%error, thread, "Could not expire the requests of a released provider session");
        }
        idle.decided.send_modify(|count| *count += 1);
    }
}

/// A request only the released process could have taken an answer for can never be answered: it
/// expires, with its node and timeline item cancelled (`writeReleasedRuntimeRequestEvents`).
fn expire_live_requests(store: &Store, thread: &str) -> Result<(), crate::persistence::StoreError> {
    commit(store, thread, |view| {
        let now = crate::execution::at(Utc::now());
        let mut events: Vec<(&'static str, Value)> = vec![];
        for request in view["runtimeRequests"].as_array().map_or(&[][..], Vec::as_slice) {
            if request["status"] != "pending" || request["responseCapability"]["type"] != "live" {
                continue;
            }
            let mut expired = request.clone();
            expired["status"] = json!("expired");
            expired["responseCapability"] = json!({"type":"not_resumable","reason":"Provider session was closed before this runtime request was resolved."});
            expired["resolvedAt"] = json!(now);
            events.push(("runtime-request.updated", expired));
            if let Some(node) = view["nodes"].as_array().and_then(|nodes| nodes.iter().find(|node| node["id"] == request["nodeId"])) {
                let mut node = node.clone();
                node["status"] = json!("cancelled");
                node["completedAt"] = json!(now);
                events.push(("node.updated", node));
            }
            let item = view["turnItems"].as_array().and_then(|items| items.iter().find(|item| matches!(item["type"].as_str(), Some("approval_request" | "user_input_request")) && item["requestId"] == request["id"]));
            if let Some(item) = item {
                let mut item = item.clone();
                item["status"] = json!("cancelled");
                item["completedAt"] = json!(now);
                item["updatedAt"] = json!(now);
                events.push(("turn-item.updated", item));
            }
        }
        Ok(events)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(runs: Value, items: Value) -> Value {
        json!({"thread":{"id":"t","activeProviderThreadId":"pt","pullRequests":[]},"runs":runs,"turnItems":items,"providerThreads":[{"id":"pt"}]})
    }

    #[test]
    fn a_running_or_queued_turn_keeps_the_session_and_a_held_queue_does_not() {
        for status in ["preparing", "starting", "running", "waiting"] {
            assert_eq!(verdict(&thread(json!([{"id":"r","ordinal":1,"status":status}]), json!([])), false), Verdict::Busy, "{status}");
        }
        assert_eq!(verdict(&thread(json!([{"id":"r","ordinal":1,"status":"queued"}]), json!([])), false), Verdict::Busy);
        assert_eq!(verdict(&thread(json!([{"id":"r","ordinal":1,"status":"completed"},{"id":"q","ordinal":2,"status":"queued","queueHeld":true}]), json!([])), false), Verdict::Idle);
        assert_eq!(verdict(&thread(json!([{"id":"r","ordinal":1,"status":"completed"}]), json!([])), true), Verdict::Busy, "an effect of the thread is outstanding");
    }

    #[test]
    fn pending_background_work_defers_release_but_a_pull_request_watch_does_not() {
        let settled = json!([{"id":"r","ordinal":1,"status":"completed"}]);
        let running = json!([{"id":"c","runId":"r","type":"command_execution","status":"running","input":"sleep 60"}]);
        assert_eq!(verdict(&thread(settled.clone(), running), false), Verdict::Pending);
        let finished = json!([{"id":"c","runId":"r","type":"command_execution","status":"completed","input":"sleep 60"}]);
        assert_eq!(verdict(&thread(settled.clone(), finished), false), Verdict::Idle);
        let mut watched = thread(settled, json!([]));
        watched["thread"]["pullRequests"] = json!([{"host":"github.com","repository":"o/r","number":1,"watch":{"enabled":true}}]);
        assert_eq!(verdict(&watched, false), Verdict::Idle);
    }

    #[test]
    fn a_thread_is_due_a_window_after_its_last_activity_and_pending_work_defers_it_up_to_the_cap() {
        let (window, cap) = (100, 250);
        let mut tracker = Tracker::default();
        tracker.touch("a", 0);
        tracker.touch("b", 40);
        assert_eq!(tracker.next_deadline(window), Some(100));
        assert!(tracker.due(99, window).is_empty());
        assert_eq!(tracker.due(100, window), ["a"]);
        // Activity moves the window; a thread nobody tracks is not tracked by activity.
        tracker.touch_if_tracked("a", 90);
        tracker.touch_if_tracked("ghost", 90);
        assert_eq!(tracker.next_deadline(window), Some(140));
        // Pending work re-arms a window until the pin has lasted the cap, then releases.
        assert!(!tracker.decide("a", Verdict::Pending, 190, cap));
        assert_eq!(tracker.next_deadline(window), Some(140));
        assert!(!tracker.decide("a", Verdict::Pending, 290, cap));
        assert!(!tracker.decide("a", Verdict::Pending, 389, cap));
        assert!(tracker.decide("a", Verdict::Pending, 440, cap));
        // Once released the thread is forgotten; a busy verdict clears the pin so a later quiet period starts a new one.
        tracker.forget("a");
        tracker.touch("a", 500);
        assert!(!tracker.decide("a", Verdict::Pending, 600, cap));
        assert!(!tracker.decide("a", Verdict::Busy, 700, cap));
        assert!(!tracker.decide("a", Verdict::Pending, 800, cap));
        assert!(tracker.decide("a", Verdict::Idle, 900, cap));
    }
}
