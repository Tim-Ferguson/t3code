//! The live end of a thread stream keeps only the newest running update of each tool call within a short window
//! (`ThreadLiveEventCoalescer.ts`). A command that prints continuously, or a search that reports its hits, would otherwise send a
//! frame per update, each carrying the whole item, to a client that draws the last one. Any other event closes the window at once,
//! so a client never sees an update after the event that follows it, and a terminal update is never dropped.
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

/// `COALESCE_WINDOW`: how long running updates wait, counted from the first of them.
pub(crate) const WINDOW: Duration = Duration::from_millis(50);
/// `MAX_PENDING_UPDATES`: the count that flushes the window early.
pub(crate) const MAX_PENDING: usize = 512;

/// `isToolUpdated`: a tool call still running. The item types are those that report progress while they run.
fn is_tool_updated(item: &Value) -> bool {
    let event = &item["event"];
    event["type"] == "turn-item.updated"
        && event["payload"]["status"] == "running"
        && matches!(event["payload"]["type"].as_str(), Some("command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool"))
}

/// `stableToolCallIdentity`: parallel calls with one label are told apart by item id, within a thread and run.
fn identity(item: &Value) -> Option<String> {
    let event = &item["event"];
    (event["type"] == "turn-item.updated").then(|| format!("{}\0{}\0{}", event["threadId"].as_str().unwrap_or_default(), event["runId"].as_str().unwrap_or_default(), event["payload"]["id"].as_str().unwrap_or_default()))
}

/// `coalesceLiveToolUpdatedEvents`: of each run of running updates (between two other events) the newest of each tool call
/// survives, in the order it arrived. Everything else is kept as it is.
pub(crate) fn coalesce(events: Vec<Value>) -> Vec<Value> {
    fn flush(updates: &mut Vec<Value>, survivors: &mut Vec<Value>) {
        let mut seen = std::collections::HashSet::new();
        let mut latest: Vec<Value> = updates
            .drain(..)
            .rev()
            .filter(|update| match identity(update) {
                Some(key) => seen.insert(key),
                None => true,
            })
            .collect();
        latest.reverse();
        survivors.append(&mut latest);
    }
    let (mut survivors, mut updates) = (Vec::new(), Vec::new());
    for event in events {
        if is_tool_updated(&event) {
            updates.push(event);
            continue;
        }
        flush(&mut updates, &mut survivors);
        survivors.push(event);
    }
    flush(&mut updates, &mut survivors);
    survivors
}

/// The window of one thread stream. The stream offers each event it would send and sends what comes back; when `deadline` passes it
/// sends what `flush` returns. Time is tokio's, so a test that pauses the clock controls it.
#[derive(Default)]
pub(crate) struct ThreadLiveCoalescer {
    pending: Vec<Value>,
    deadline: Option<Instant>,
}
impl ThreadLiveCoalescer {
    /// The frames to send now: none while a running update waits in the window, and the window's survivors followed by the event
    /// when the event is anything else.
    pub(crate) fn offer(&mut self, item: Value) -> Vec<Value> {
        let update = is_tool_updated(&item);
        self.pending.push(item);
        if !update {
            return self.flush();
        }
        if self.pending.len() == 1 {
            self.deadline = Some(Instant::now() + WINDOW);
        }
        if self.pending.len() >= MAX_PENDING { self.flush() } else { Vec::new() }
    }
    /// When the window closes, if one is open.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    /// Closes the window: the newest update of each tool call it holds.
    pub(crate) fn flush(&mut self) -> Vec<Value> {
        self.deadline = None;
        coalesce(std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(sequence: u64, kind: &str, status: &str, item: &str) -> Value {
        json!({"kind":"event","sequence":sequence,"event":{"type":"turn-item.updated","threadId":"t","runId":"r","payload":{"id":item,"type":kind,"status":status,"output":format!("{sequence}")}}})
    }
    fn lifecycle(sequence: u64) -> Value {
        json!({"kind":"event","sequence":sequence,"event":{"type":"run.updated","threadId":"t","runId":"r","payload":{"id":"r"}}})
    }
    fn sequences(items: &[Value]) -> Vec<u64> {
        items.iter().map(|item| item["sequence"].as_u64().unwrap()).collect()
    }

    #[test]
    fn the_newest_running_update_of_each_call_survives_in_order() {
        let events = vec![
            event(1, "command_execution", "running", "a"),
            event(2, "command_execution", "running", "b"),
            event(3, "command_execution", "running", "a"),
            event(4, "web_search", "running", "a"),
            event(5, "command_execution", "running", "b"),
        ];
        // A call is its item id, whatever kind of item reports it: the web search with id "a" replaces the command with id "a".
        assert_eq!(sequences(&coalesce(events)), vec![4, 5]);
        // A terminal update, a message and a lifecycle event are never dropped, and close the run of updates before them.
        let events = vec![
            event(1, "command_execution", "running", "a"),
            event(2, "command_execution", "running", "a"),
            event(3, "command_execution", "completed", "a"),
            event(4, "assistant_message", "running", "m"),
            event(5, "assistant_message", "running", "m"),
            lifecycle(6),
            event(7, "file_change", "running", "f"),
            event(8, "file_change", "running", "f"),
        ];
        assert_eq!(sequences(&coalesce(events)), vec![2, 3, 4, 5, 6, 8]);
        // The same item id in another run or thread is another call.
        let mut other_run = event(2, "command_execution", "running", "a");
        other_run["event"]["runId"] = json!("r2");
        let mut other_thread = event(3, "command_execution", "running", "a");
        other_thread["event"]["threadId"] = json!("t2");
        assert_eq!(sequences(&coalesce(vec![event(1, "command_execution", "running", "a"), other_run, other_thread])), vec![1, 2, 3]);
    }

    /// What the unchanged `coalesceLiveToolUpdatedEvents` keeps of seeded sequences (`tools/generate_thread_live_coalescer_fixtures.mjs`).
    #[test]
    fn the_survivors_are_the_originals() {
        let rows: Vec<Value> = include_str!("../tests/fixtures/thread-live-coalescing.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(rows.len() >= 400);
        let mut dropped = 0;
        for row in &rows {
            let events = row["events"].as_array().unwrap().clone();
            let kept = coalesce(events.clone());
            assert_eq!(json!(sequences(&kept)), row["survivors"], "{}", row["events"]);
            dropped += events.len() - kept.len();
            // The window gives the same survivors as the pure function over the whole run, when no event closes it early.
            let mut window = ThreadLiveCoalescer::default();
            let mut sent: Vec<Value> = events.iter().flat_map(|event| window.offer(event.clone())).collect();
            sent.extend(window.flush());
            assert_eq!(json!(sequences(&sent)), row["survivors"], "{}", row["events"]);
        }
        assert!(dropped > 100, "the sequences must exercise dropping ({dropped})");
    }

    #[tokio::test(start_paused = true)]
    async fn updates_wait_for_the_window_and_anything_else_closes_it() {
        let mut window = ThreadLiveCoalescer::default();
        assert!(window.deadline().is_none());
        assert!(window.offer(event(1, "command_execution", "running", "a")).is_empty());
        let opened = window.deadline().expect("the first update opens the window");
        assert_eq!(opened, Instant::now() + WINDOW);
        // Later updates join the window; they do not move its end.
        tokio::time::advance(Duration::from_millis(30)).await;
        assert!(window.offer(event(2, "command_execution", "running", "a")).is_empty());
        assert!(window.offer(event(3, "command_execution", "running", "b")).is_empty());
        assert_eq!(window.deadline(), Some(opened));
        // The deadline is the stream's to wait for; reaching it, the stream flushes.
        tokio::time::advance(Duration::from_millis(20)).await;
        assert_eq!(Instant::now(), opened);
        assert_eq!(sequences(&window.flush()), vec![2, 3]);
        assert!(window.deadline().is_none() && window.flush().is_empty());
        // A window opens again with the next update.
        assert!(window.offer(event(4, "command_execution", "running", "a")).is_empty());
        assert!(window.deadline().is_some_and(|at| at > opened));
        // Another event ends it at once, and goes out after what the window held.
        assert_eq!(sequences(&window.offer(event(5, "command_execution", "completed", "a"))), vec![4, 5]);
        assert!(window.deadline().is_none());
        // With nothing waiting, another event goes straight out.
        assert_eq!(sequences(&window.offer(lifecycle(6))), vec![6]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_window_is_flushed_early() {
        let mut window = ThreadLiveCoalescer::default();
        for index in 1..MAX_PENDING as u64 {
            assert!(window.offer(event(index, "command_execution", "running", &format!("call-{index}"))).is_empty());
        }
        let flushed = window.offer(event(MAX_PENDING as u64, "command_execution", "running", "last"));
        assert_eq!(flushed.len(), MAX_PENDING, "every call is a different item, so none is dropped");
        assert!(window.deadline().is_none());
        // Repeated updates of one call fill it too, and what goes out is that call's newest alone.
        for index in 1..MAX_PENDING as u64 {
            assert!(window.offer(event(index, "command_execution", "running", "same")).is_empty());
        }
        assert_eq!(sequences(&window.offer(event(MAX_PENDING as u64, "command_execution", "running", "same"))), vec![MAX_PENDING as u64]);
    }
}
