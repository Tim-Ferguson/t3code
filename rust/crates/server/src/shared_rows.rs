//! Rows several threads read that one thread's event writes: provider sessions and provider threads.
//!
//! The original keeps them in tables of their own, one row per id, whichever thread's stream wrote the event
//! (`orchestration_v2_projection_provider_sessions` and `_provider_threads`, with a binding table of which threads a session serves).
//! A thread reads the rows bound to it, and for provider threads also those a node of its own owns or one of its subagents holds, so
//! a session that serves several threads is the same row for each, and a child thread's provider threads are listed with its parent.
//! This server keeps each thread's projection as one document, which holds what its own stream wrote; these tables are what the
//! read path (`projection_read`) joins with to serve the rest.
//!
//! A store written by an earlier build has no such tables: they are created at open and filled from the stored projections, so no
//! rebuild is needed.
use crate::persistence::{Event, StoreError};
use rusqlite::{Connection, Transaction, params};

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS rust_provider_threads (id TEXT PRIMARY KEY, app_thread_id TEXT, owner_node_id TEXT, json TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS rust_provider_threads_owner ON rust_provider_threads(owner_node_id);
CREATE TABLE IF NOT EXISTS rust_provider_sessions (id TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS rust_provider_session_bindings (session_id TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY(session_id, thread_id));
CREATE INDEX IF NOT EXISTS rust_provider_session_bindings_thread ON rust_provider_session_bindings(thread_id);";

/// Creates the tables; when they are new, fills them from the thread projections already stored.
pub(crate) fn ensure(connection: &Connection) -> Result<(), rusqlite::Error> {
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='rust_provider_threads')", [], |row| row.get(0))?;
    connection.execute_batch(TABLES)?;
    if !exists {
        connection.execute_batch(
            "INSERT OR REPLACE INTO rust_provider_threads(id,app_thread_id,owner_node_id,json)
               SELECT json_extract(row.value,'$.id'), json_extract(row.value,'$.appThreadId'), json_extract(row.value,'$.ownerNodeId'), row.value
               FROM rust_projections p, json_each(p.state_json,'$.providerThreads') row WHERE p.aggregate_kind='thread';
             INSERT OR REPLACE INTO rust_provider_sessions(id,json)
               SELECT json_extract(row.value,'$.id'), row.value FROM rust_projections p, json_each(p.state_json,'$.providerSessions') row WHERE p.aggregate_kind='thread';
             INSERT OR IGNORE INTO rust_provider_session_bindings(session_id,thread_id)
               SELECT json_extract(row.value,'$.id'), p.aggregate_id FROM rust_projections p, json_each(p.state_json,'$.providerSessions') row WHERE p.aggregate_kind='thread';",
        )?;
    }
    Ok(())
}

/// What a thread event leaves in the shared tables (`ProjectionStore.apply`).
pub(crate) fn apply(transaction: &Transaction<'_>, event: &Event) -> Result<(), StoreError> {
    let payload = &event.payload;
    match event.event_type.as_str() {
        "provider-thread.updated" => {
            transaction.execute(
                "INSERT OR REPLACE INTO rust_provider_threads(id,app_thread_id,owner_node_id,json) VALUES(?1,?2,?3,?4)",
                params![payload["id"].as_str(), payload["appThreadId"].as_str(), payload["ownerNodeId"].as_str(), serde_json::to_string(payload)?],
            )?;
        }
        "provider-session.attached" | "provider-session.updated" => {
            transaction.execute("INSERT OR REPLACE INTO rust_provider_sessions(id,json) VALUES(?1,?2)", params![payload["id"].as_str(), serde_json::to_string(payload)?])?;
            if event.event_type == "provider-session.attached" {
                transaction.execute("INSERT OR IGNORE INTO rust_provider_session_bindings(session_id,thread_id) VALUES(?1,?2)", params![payload["id"].as_str(), event.aggregate_id])?;
            }
        }
        "provider-session.detached" => {
            transaction.execute("DELETE FROM rust_provider_session_bindings WHERE session_id=?1 AND thread_id=?2", params![payload["providerSessionId"].as_str(), event.aggregate_id])?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Store, StoredEvent};
    use serde_json::{Value, json};

    /// A store written by an earlier build has neither the shared tables nor the shell counters. Opening it fills the tables from the
    /// stored projections; the shell is read from the history until the thread's next event stores the counters. No rebuild.
    #[test]
    fn a_store_from_an_earlier_build_is_filled_in_when_opened_and_its_threads_when_they_next_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rust-state.sqlite");
        let store = Store::open(&path).unwrap();
        let event = |kind: &str, payload: Value, sequence: u64| StoredEvent {
            sequence,
            event: Event { event_id: format!("e{sequence}"), aggregate_kind: "thread".into(), aggregate_id: "thread:old".into(), occurred_at: "2026-01-01T00:00:00.000Z".into(), command_id: None, causation_event_id: None, correlation_id: None, event_type: kind.into(), payload, metadata: json!({}) },
        };
        let thread = json!({"createdBy":"user","creationSource":"web","id":"thread:old","projectId":"p","title":"T","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread:old"},"forkedFrom":null,"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","archivedAt":null,"settledOverride":null,"settledAt":null,"lastVisitedAt":null,"deletedAt":null});
        let session = json!({"id":"session-1","status":"ready"});
        let item = json!({"id":"i1","threadId":"thread:old","runId":null,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":1,"status":"completed","title":null,"startedAt":null,"completedAt":null,"updatedAt":"2026-01-01T00:00:00.000Z","type":"system_notice","message":"x"});
        for (sequence, (kind, payload)) in [("thread.created", thread), ("provider-session.attached", session), ("turn-item.updated", item)].into_iter().enumerate() {
            store.transaction(|transaction| crate::thread::reduce(transaction, &event(kind, payload, sequence as u64 + 1))).unwrap();
        }
        // Make it the store an earlier build wrote.
        store
            .transaction(|transaction| {
                transaction.execute_batch("DROP TABLE rust_provider_threads; DROP TABLE rust_provider_sessions; DROP TABLE rust_provider_session_bindings;")?;
                let mut projection = crate::persistence::read_projection(transaction, "thread", "thread:old")?.unwrap();
                projection.as_object_mut().unwrap().remove(crate::shell_state::KEY);
                crate::persistence::write_projection(transaction, "thread", "thread:old", &projection)
            })
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let bound: i64 = store.read(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM rust_provider_session_bindings WHERE session_id='session-1' AND thread_id='thread:old'", [], |row| row.get(0))?)).unwrap();
        assert_eq!(bound, 1);
        let projection = store.projection("thread", "thread:old").unwrap().unwrap();
        assert_eq!(crate::thread::shell(&projection)["itemCount"], 1, "read from the history");
        let reduced = crate::thread::projection_after(Some(projection), &event("run.background-work-cancelled", json!({"runId":"none","restartCancelledBackgroundWork":[]}), 4).event).unwrap();
        assert_eq!(reduced[crate::shell_state::KEY]["items"], 1, "stored with the next event");
    }
}
