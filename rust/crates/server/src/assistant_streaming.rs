//! The `responseStreamingMode` setting for every provider (`orchestration-v2/assistantStreaming.ts`, applied in
//! `RunExecutionService.ts` to the events of a provider's session before they are stored). Streaming assistant text and
//! reasoning is held back so unfinished markdown stays out of the read model: in `turn` mode nothing is delivered until
//! the message completes; in `paragraph` mode the text is delivered up to its last settled block (a blank line, a closing
//! code fence or a list item start outside an open fence), at most every 400 ms per message. A message that completes
//! is always delivered whole, and the node of a running assistant message is not delivered at all.
//!
//! Every adapter stores what its provider streams through `codex_runtime::commit`, so the filter sits there:
//! [`filter`] takes the events an adapter planned and returns the ones to store. State is one [`Filter`] per thread
//! (the original has one per attempt; message ids are unique, and the state is dropped when a run ends). The OpenCode
//! adapters buffer by block themselves (`opencode_projection`), and filtering their output again would cut it short at
//! a list item boundary, so they are left out here; both ways produce the original's `paragraph` result.
//! The ACP family (`acpRegistry`, Grok, Antigravity) is filtered too. Their adapter keeps the text a message has reached in the
//! projection it plans from rather than in memory, which is what the held rows are for: [`overlay_held`] lays the latest full row
//! of every withheld message, item and node over the projection the planner is handed (in place, and taken off again), so a chunk
//! is appended to the whole text and not to the part that was delivered. What a test or client of this family sees of a message
//! that is still streaming is therefore the original's: nothing until a paragraph (or the end of the message) in `paragraph`
//! mode, nothing until the message ends in `turn` mode.
//!
//! Withheld rows are not lost. Code that settles a run (an interruption, a failure, a subagent's end) finds the streaming rows it
//! closes in the projection it is handed, which has the held rows laid over it, and what it writes back (not streaming any more)
//! is delivered whole. A run interrupted mid-answer therefore keeps the answer it had.
//!
//! Cost: a delta that arrives within 400 ms of the last delivery of its message is dropped by a map lookup and a
//! subtraction (the row is moved into the held map, not copied); the text is scanned (once, linearly) only when a delivery is
//! possible. Nothing here depends on the length of the thread. A thread with no installed
//! filter source (the unit tests of an adapter) is not filtered.
use crate::persistence::Store;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

/// `ResponseStreamingMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Turn,
    Paragraph,
}

const MIN_INTERVAL_MS: i64 = 400;

/// `makeAssistantStreamingFilter`'s state: what was last delivered for each streaming message.
#[derive(Default)]
pub struct Filter {
    delivered: HashMap<String, (String, i64)>,
    /// The latest full row of each streaming message, item and assistant node, whether or not it was delivered.
    held: HashMap<String, (&'static str, Value)>,
}

impl Filter {
    /// The event to store in place of `payload`, or `None` to store nothing.
    pub fn apply(&mut self, mode: Mode, kind: &'static str, mut payload: Value, now_ms: i64) -> Option<Value> {
        if kind == "node.updated" && payload["kind"] == "assistant_message" {
            let key = format!("{kind}:{}", payload["id"].as_str().unwrap_or_default());
            if payload["status"] == "running" {
                self.held.insert(key, (kind, payload));
                return None;
            }
            self.held.remove(&key);
        }
        let streamed_text = match kind {
            "message.updated" => payload["role"] == "assistant",
            "turn-item.updated" => matches!(payload["type"].as_str(), Some("assistant_message" | "reasoning")),
            _ => false,
        };
        if !streamed_text {
            return Some(payload);
        }
        let key = format!("{kind}:{}", payload["id"].as_str().unwrap_or_default());
        if payload["streaming"] != true {
            self.delivered.remove(&key);
            self.held.remove(&key);
            return Some(payload);
        }
        // What is withheld is kept whole, moved in rather than copied: a delta costs the size of its message, not twice that.
        if mode == Mode::Turn {
            self.held.insert(key, (kind, payload));
            return None;
        }
        let previous = self.delivered.get(&key);
        if previous.is_some_and(|(_, at)| now_ms - at < MIN_INTERVAL_MS) {
            self.held.insert(key, (kind, payload));
            return None;
        }
        let text = payload["text"].as_str().unwrap_or_default();
        let ready = &text[..crate::opencode_projection::split_buffered(text)];
        if ready.is_empty() || previous.is_some_and(|(delivered, _)| delivered == ready) {
            self.held.insert(key, (kind, payload));
            return None;
        }
        let ready = ready.to_owned();
        let mut delivery = payload.clone();
        delivery["text"] = Value::String(ready.clone());
        self.held.insert(key.clone(), (kind, payload));
        self.delivered.insert(key, (ready, now_ms));
        Some(delivery)
    }
}

/// Where a thread's mode comes from: the settings of the process, given the thread's projection. `None` leaves the thread
/// unfiltered.
pub type ModeSource = Arc<dyn Fn(&Value) -> Option<Mode> + Send + Sync>;

#[derive(Default)]
struct Filters {
    source: RwLock<Option<ModeSource>>,
    threads: Mutex<HashMap<String, Filter>>,
}

/// Installs the source of the modes for the threads of `store`: the project's own `responseStreamingMode`, else the
/// environment's, for the threads of the Codex, Claude and Pi adapters.
pub fn install(store: &Store, providers: &crate::provider_registry::ProviderRegistry) {
    let providers = providers.clone();
    let source: ModeSource = Arc::new(move |projection| {
        let thread = &projection["thread"];
        // Only adapters that hold their stream state in memory can have events withheld (see the module comment).
        let instance = thread["modelSelection"]["instanceId"].as_str()?;
        if !providers.driver(instance).is_ok_and(|driver| matches!(driver, "codex" | "claudeAgent" | "pi" | "acpRegistry" | "grok" | "antigravity")) {
            return None;
        }
        let project: Option<t3_contracts::ProjectId> = thread["projectId"].as_str().and_then(|id| id.parse().ok());
        let mode = providers.with_settings(|settings| project.and_then(|project| settings.project_settings_overrides.get(&project).and_then(|entry| entry.response_streaming_mode)).unwrap_or(settings.response_streaming_mode));
        Some(match mode {
            t3_contracts::ResponseStreamingMode::Turn => Mode::Turn,
            t3_contracts::ResponseStreamingMode::Paragraph => Mode::Paragraph,
        })
    });
    install_source(store, source);
}

pub(crate) fn install_source(store: &Store, source: ModeSource) {
    *store.extension(Filters::default).source.write().unwrap() = Some(source);
}

fn streams_text(kind: &str) -> bool {
    matches!(kind, "message.updated" | "turn-item.updated" | "node.updated")
}

/// The rows the filter holds for the thread, laid over `projection` in place so that code settling a run finds each streaming
/// message, item and node with the text it had reached; [`Overlay::undo`] puts the projection back as it was. Costs the held rows
/// (searched from the end, where streaming rows are) and nothing in the length of the thread.
pub(crate) fn overlay_held(store: &Store, thread_id: &str, projection: &mut Value) -> Overlay {
    let filters = store.extension(Filters::default);
    let threads = filters.threads.lock().unwrap();
    let mut changes = Vec::new();
    let Some(filter) = threads.get(thread_id).filter(|filter| !filter.held.is_empty()) else { return Overlay(changes) };
    for (kind, row) in filter.held.values() {
        let list = match *kind {
            "message.updated" => "messages",
            "turn-item.updated" => "turnItems",
            _ => "nodes",
        };
        let Some(rows) = projection[list].as_array_mut() else { continue };
        match rows.iter().rposition(|existing| existing["id"] == row["id"]) {
            Some(at) => changes.push((list, at, Some(std::mem::replace(&mut rows[at], row.clone())))),
            None => {
                changes.push((list, rows.len(), None));
                rows.push(row.clone());
            }
        }
    }
    Overlay(changes)
}

/// What [`overlay_held`] changed: the row each place held before, or none for a row it appended.
pub(crate) struct Overlay(Vec<(&'static str, usize, Option<Value>)>);

impl Overlay {
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Overlay(Vec::new())
    }
    pub(crate) fn undo(self, projection: &mut Value) {
        for (list, at, prior) in self.0.into_iter().rev() {
            let Some(rows) = projection[list].as_array_mut() else { continue };
            match prior {
                Some(row) => rows[at] = row,
                None => {
                    rows.pop();
                }
            }
        }
    }
}

/// The events of one adapter write that reach the store (`filterAssistantEvent` over each).
pub(crate) fn filter(store: &Store, thread_id: &str, projection: &Value, events: Vec<(&'static str, Value)>, now_ms: i64) -> Vec<(&'static str, Value)> {
    if !events.iter().any(|(kind, _)| streams_text(kind) || *kind == "run.updated") {
        return events;
    }
    let filters = store.extension(Filters::default);
    let Some(source) = filters.source.read().unwrap().clone() else { return events };
    let ended = events.iter().any(|(kind, payload)| *kind == "run.updated" && matches!(payload["status"].as_str(), Some("completed" | "failed" | "interrupted" | "cancelled")));
    let kept = if events.iter().any(|(kind, _)| streams_text(kind)) {
        match source(projection) {
            Some(mode) => {
                let mut threads = filters.threads.lock().unwrap();
                let filter = threads.entry(thread_id.to_owned()).or_default();
                events.into_iter().filter_map(|(kind, payload)| filter.apply(mode, kind, payload, now_ms).map(|payload| (kind, payload))).collect()
            }
            None => events,
        }
    } else {
        events
    };
    if ended {
        filters.threads.lock().unwrap().remove(thread_id);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(id: &str, text: &str, streaming: bool) -> Value {
        json!({"id": id, "role": "assistant", "text": text, "streaming": streaming})
    }

    /// The unchanged original over scripted event sequences (`tools/generate_assistant_streaming_fixtures.mjs`).
    #[test]
    fn the_filter_delivers_what_the_original_delivers() {
        let (mut sequences, mut splits) = (0, 0);
        for line in include_str!("../tests/fixtures/assistant-streaming.jsonl").lines().filter(|line| !line.is_empty()) {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "split" => {
                    let text = row["text"].as_str().unwrap();
                    let at = crate::opencode_projection::split_buffered(text);
                    assert_eq!(json!({"ready": &text[..at], "rest": &text[at..]}), row["result"], "{row}");
                    splits += 1;
                }
                "sequence" => {
                    let mode = if row["mode"] == "turn" { Mode::Turn } else { Mode::Paragraph };
                    let mut filter = Filter::default();
                    let delivered: Vec<Value> = row["steps"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|step| filter.apply(mode, ["message.updated", "turn-item.updated", "node.updated"].into_iter().find(|kind| *kind == step["kind"].as_str().unwrap()).unwrap(), step["payload"].clone(), step["now"].as_i64().unwrap()).unwrap_or(Value::Null))
                        .collect();
                    assert_eq!(Value::Array(delivered), row["delivered"], "{row}");
                    sequences += 1;
                }
                other => panic!("unknown row {other}"),
            }
        }
        assert!(splits >= 40 && sequences >= 20, "{splits} splits, {sequences} sequences");
    }

    #[test]
    fn the_sink_withholds_per_thread_forgets_a_thread_when_its_run_ends_and_leaves_unfiltered_threads_alone() {
        let store = Store::memory().unwrap();
        let events = || vec![("node.updated", json!({"kind": "assistant_message", "status": "running"})), ("message.updated", message("a", "partial\n\nnext", true))];
        let projection = |driver: &str| json!({"thread": {"id": "t", "modelSelection": {"instanceId": driver}}});
        // No source installed: nothing is filtered.
        assert_eq!(filter(&store, "t", &projection("codex"), events(), 0).len(), 2);
        install_source(&store, Arc::new(|projection| match projection["thread"]["modelSelection"]["instanceId"].as_str() {
            Some("codex") => Some(Mode::Paragraph),
            Some("claude") => Some(Mode::Turn),
            _ => None,
        }));
        let kept = filter(&store, "t", &projection("codex"), events(), 1_000);
        assert_eq!(kept.len(), 1, "the running node is withheld, the message is delivered up to its settled block");
        assert_eq!(kept[0].1["text"], "partial\n\n");
        // Inside the window the next delta is withheld; another thread has its own window.
        assert!(filter(&store, "t", &projection("codex"), vec![("message.updated", message("a", "partial\n\nnext\n\n", true))], 1_100).is_empty());
        assert_eq!(filter(&store, "other", &projection("codex"), vec![("message.updated", message("a", "partial\n\nx", true))], 1_100).len(), 1);
        // Turn mode delivers only the completed message; a thread the source does not cover is untouched.
        assert!(filter(&store, "c", &projection("claude"), vec![("message.updated", message("a", "text\n\n", true))], 0).is_empty());
        assert_eq!(filter(&store, "c", &projection("claude"), vec![("message.updated", message("a", "text", false))], 0).len(), 1);
        assert_eq!(filter(&store, "g", &projection("grok"), vec![("message.updated", message("a", "text", true))], 0).len(), 1);
        // The end of a run drops the thread's state: the same message id starts afresh.
        let ended = vec![("run.updated", json!({"status": "completed"}))];
        assert_eq!(filter(&store, "t", &projection("codex"), ended, 1_200).len(), 1);
        assert_eq!(filter(&store, "t", &projection("codex"), vec![("message.updated", message("a", "partial\n\nnext", true))], 1_300).len(), 1, "delivered again: the earlier delivery was forgotten");
    }

    #[test]
    fn a_settling_write_finds_the_withheld_row_in_full_and_its_closing_event_is_delivered_whole() {
        let store = Store::memory().unwrap();
        install_source(&store, Arc::new(|_| Some(Mode::Turn)));
        let projection = json!({"thread": {"id": "t"}, "messages": [], "turnItems": [{"id": "other", "type": "command_execution"}], "nodes": []});
        let withheld = filter(&store, "t", &projection, vec![("node.updated", json!({"id": "n", "kind": "assistant_message", "status": "running"})), ("message.updated", message("a", "half an answer", true))], 0);
        assert!(withheld.is_empty(), "turn mode delivers nothing while the message streams");
        // What an interruption sees: the thread as stored, plus the rows held back, with the text they had reached.
        let mut view = projection.clone();
        let overlay = overlay_held(&store, "t", &mut view);
        assert_eq!(view["messages"][0]["text"], "half an answer");
        assert_eq!(view["nodes"][0]["status"], "running");
        assert_eq!(view["turnItems"].as_array().unwrap().len(), 1, "unrelated rows are untouched");
        overlay.undo(&mut view);
        assert_eq!(view, projection, "the projection is as it was once the overlay is undone");
        let mut view = projection.clone();
        overlay_held(&store, "t", &mut view);
        // Closing them, as the run settling does, delivers the whole message and the node.
        let mut closing = view["messages"][0].clone();
        closing["streaming"] = json!(false);
        let delivered = filter(&store, "t", &projection, vec![("message.updated", closing), ("node.updated", json!({"id": "n", "kind": "assistant_message", "status": "interrupted"}))], 5);
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].1["text"], "half an answer");
        let mut after = projection.clone();
        assert!(overlay_held(&store, "t", &mut after).0.is_empty() && after == projection, "nothing is held after the close");
        assert!(overlay_held(&store, "never-seen", &mut after).0.is_empty());
    }

    #[test]
    fn a_running_assistant_node_and_other_events_are_handled_as_the_original_does() {
        let mut filter = Filter::default();
        assert!(filter.apply(Mode::Paragraph, "node.updated", json!({"kind": "assistant_message", "status": "running"}), 0).is_none());
        assert!(filter.apply(Mode::Paragraph, "node.updated", json!({"kind": "assistant_message", "status": "completed"}), 0).is_some());
        assert!(filter.apply(Mode::Turn, "message.updated", json!({"id": "u", "role": "user", "text": "hi", "streaming": true}), 0).is_some(), "only assistant messages are held");
        assert!(filter.apply(Mode::Turn, "turn-item.updated", json!({"id": "t", "type": "command_execution", "streaming": true}), 0).is_some());
        assert!(filter.apply(Mode::Turn, "message.updated", message("a", "partial\n\n", true), 0).is_none());
        let done = filter.apply(Mode::Turn, "message.updated", message("a", "partial\n\nwhole", false), 0).unwrap();
        assert_eq!(done["text"], "partial\n\nwhole");
    }

    /// The per-delta cost: the common case (a delta inside the 400 ms window) and the delivering case on a long message.
    #[test]
    fn the_cost_of_a_delta_is_small_inside_the_window_and_linear_when_delivering() {
        let mut filter = Filter::default();
        let long = "paragraph of text.\n\n".repeat(2000);
        assert!(filter.apply(Mode::Paragraph, "message.updated", message("a", &long, true), 0).is_some());
        let started = std::time::Instant::now();
        for step in 0..10_000 {
            assert!(filter.apply(Mode::Paragraph, "message.updated", message("a", &long, true), 1 + step % 300).is_none());
        }
        let inside = started.elapsed() / 10_000;
        let started = std::time::Instant::now();
        for round in 0..200 {
            let text = format!("{long}extra {round}\n\n");
            assert!(filter.apply(Mode::Paragraph, "message.updated", message("a", &text, true), 1000 * (round + 1)).is_some());
        }
        let delivering = started.elapsed() / 200;
        eprintln!("assistant streaming filter, 40 KB message: {inside:?} per delta inside the window (includes building the payload), {delivering:?} per delivering delta");
    }
}
