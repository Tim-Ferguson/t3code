//! How fast the reducer runs: the events of the `full` import fixture, repeated for 40 threads (69,800 events), through the reducer
//! this server had before `thread::projection_after` took on the original's rules and the shell counters, and through the reducer
//! it has. Run on demand, not by the suite:
//!   cargo test -p t3-server --lib reducer_bench -- --ignored --nocapture
use crate::persistence::{Event, StoreError};
use crate::state_import_events::{Projector, decode, read_rows};
use crate::state_import_source::open;
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::{collections::HashMap, io::Read};

/// `thread::projection_after` as it was before (verbatim), for the comparison.
fn legacy_projection_after(
    projection: Option<Value>,
    event: &Event,
) -> Result<Value, StoreError> {
    let mut projection = if event.event_type == "thread.created" {
        let mut projection = json!({"thread":event.payload,"updatedAt":event.occurred_at});
        for field in [
            "runs",
            "attempts",
            "nodes",
            "subagents",
            "providerSessions",
            "providerThreads",
            "providerTurns",
            "runtimeRequests",
            "messages",
            "plans",
            "turnItems",
            "checkpointScopes",
            "checkpoints",
            "contextHandoffs",
            "contextTransfers",
            "visibleTurnItems",
        ] {
            projection[field] = json!([]);
        }
        projection
    } else {
        projection.ok_or_else(|| StoreError::InvalidProjection {
            kind: "thread".into(),
            id: event.aggregate_id.clone(),
            detail: "missing thread".into(),
        })?
    };
    let field = match event.event_type.as_str() {
        "run.created" | "run.updated" => Some("runs"),
        "run-attempt.created" | "run-attempt.updated" => Some("attempts"),
        "node.updated" => Some("nodes"),
        "provider-session.attached" | "provider-session.updated" => Some("providerSessions"),
        "provider-thread.updated" => Some("providerThreads"),
        "provider-turn.updated" => Some("providerTurns"),
        "runtime-request.updated" => Some("runtimeRequests"),
        "message.updated" => Some("messages"),
        "plan.updated" => Some("plans"),
        "checkpoint-scope.created" | "checkpoint-scope.updated" => Some("checkpointScopes"),
        "checkpoint.captured" => Some("checkpoints"),
        "turn-item.updated" => Some("turnItems"),
        "subagent.updated" => Some("subagents"),
        "context-transfer.created" | "context-transfer.updated" => Some("contextTransfers"),
        "context-handoff.updated" => Some("contextHandoffs"),
        _ => None,
    };
    if event.event_type == "provider-session.detached" {
        projection["providerSessions"]
            .as_array_mut()
            .ok_or_else(|| {
                StoreError::InvalidCommand("Provider sessions are not an array.".into())
            })?
            .retain(|session| session["id"] != event.payload["providerSessionId"]);
    } else if let Some(field) = field {
        let rows =
            projection[field]
                .as_array_mut()
                .ok_or_else(|| StoreError::InvalidProjection {
                    kind: "thread".into(),
                    id: event.aggregate_id.clone(),
                    detail: format!("{field} is not an array"),
                })?;
        if let Some(existing) = rows.iter_mut().find(|row| row["id"] == event.payload["id"]) {
            let mut next = event.payload.clone();
            // A stale snapshot must not erase what another event recorded: a run's delegated-completion
            // cohort and a task's completion delivery are kept when the update carries none.
            for (row_field, kept) in [("runs", "delegatedCompletion"), ("subagents", "completionDelivery")] {
                if field == row_field && next.get(kept).is_none() && existing.get(kept).is_some() {
                    next[kept] = existing[kept].clone();
                }
            }
            *existing = next;
        } else {
            rows.push(event.payload.clone());
        }
        if field == "providerThreads"
            && event.payload["appThreadId"] == event.aggregate_id
            && !event.payload["nativeThreadRef"].is_null()
        {
            projection["thread"]["activeProviderThreadId"] = event.payload["id"].clone();
        }
        if matches!(field, "turnItems" | "runs" | "attempts") {
            // Local histories keep first-write order, less the runs a rollback undid and the queued
            // messages of cancelled runs; a fork's inherited rows are put in front by `reduce`.
            projection["visibleTurnItems"] = Value::Array(crate::thread_fork::local_visible_rows(&projection));
        }
    } else if event.event_type == "checkpoint.rollback-requested" {
        // A request only: the rollback effect's own events change the projection.
    } else if event.event_type.starts_with("thread.") {
        projection["thread"] = event.payload.clone();
    } else {
        return Err(StoreError::InvalidCommand(format!(
            "Unsupported projection event {}",
            event.event_type
        )));
    }
    if !matches!(
        event.event_type.as_str(),
        "thread.visited" | "thread.marked-unread"
    ) {
        projection["updatedAt"] = json!(event.occurred_at);
    }
    Ok(projection)
}



fn events() -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let userdata = directory.path().join("userdata");
    std::fs::create_dir_all(&userdata).unwrap();
    let gz = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-import/full/userdata/statev2.sqlite.gz");
    let mut bytes = Vec::new();
    GzDecoder::new(std::fs::File::open(gz).unwrap()).read_to_end(&mut bytes).unwrap();
    std::fs::write(userdata.join("statev2.sqlite"), bytes).unwrap();
    let source = open(directory.path(), directory.path()).unwrap();
    let rows = read_rows(&source, 0, 1_000_000).unwrap();
    let originals: Vec<Event> = rows.iter().filter(|row| row.aggregate_kind == "thread").map(|row| decode(row).unwrap()).collect();
    let mut out = Vec::new();
    for copy in 0..40 {
        for event in &originals {
            // A copy is its own thread: the thread's id, wherever the event names it, takes a suffix.
            let text = serde_json::to_string(event).unwrap().replace(&event.aggregate_id, &format!("{}~{copy}", event.aggregate_id));
            let mut renamed: Event = serde_json::from_str(&text).unwrap();
            renamed.event_id = format!("{}~{copy}", event.event_id);
            out.push(renamed);
        }
    }
    out
}

fn run(events: &[Event], apply: impl Fn(Option<Value>, &Event) -> Value) -> f64 {
    let mut projections: HashMap<String, Value> = HashMap::new();
    let started = std::time::Instant::now();
    for event in events {
        let next = apply(projections.remove(&event.aggregate_id), event);
        projections.insert(event.aggregate_id.clone(), next);
    }
    started.elapsed().as_secs_f64()
}

#[test]
#[ignore]
fn reducer_throughput_on_a_synthetic_source() {
    let events = events();
    let count = events.len();
    // Events of a thread that no longer exists (a child's, never created in a copy) are skipped by both.
    let (mut before, mut after) = (f64::MAX, f64::MAX);
    for _ in 0..3 {
        before = before.min(run(&events, |current, event| legacy_projection_after(current, event).unwrap_or(Value::Null)));
        after = after.min(run(&events, |current, event| crate::thread::projection_after(current, event).unwrap_or(Value::Null)));
    }
    eprintln!("reducer, {count} events: before {:.0} events/s ({before:.2}s), after {:.0} events/s ({after:.2}s)", count as f64 / before, count as f64 / after);
    // The importer's batching projector over a real store: what an import of this many events costs end to end.
    let store = crate::persistence::Store::memory().unwrap();
    let started = std::time::Instant::now();
    for batch in events.chunks(500) {
        store
            .transaction(|transaction| {
                let mut projector = Projector::default();
                for (index, event) in batch.iter().enumerate() {
                    projector.apply(transaction, &crate::persistence::StoredEvent { sequence: index as u64 + 1, event: event.clone() })?;
                }
                projector.flush(transaction)
            })
            .unwrap();
    }
    let seconds = started.elapsed().as_secs_f64();
    eprintln!("importer projector over a store, {count} events: {:.0} events/s ({seconds:.2}s)", count as f64 / seconds);
}
