//! The event log: the original's `orchestration_events` rows (application event version 2) replayed into the Rust store, and the V1
//! threads and projects the original would itself have imported when it first opened the file.
//!
//! Every row is decoded strictly with the same codecs the server uses for the events it writes (`DomainEvent`, whose payload
//! normalizers are the original's schemas), reduced into `rust_projections`, and stored under the sequence the original gave
//! it, so a client's cursor, a receipt's `result_sequence` and the order of events mean what they meant.
use crate::{
    persistence::{Event, StoredEvent, StoreError, read_projection, write_projection},
    project::reduce_project_event,
    state_import_source::{Source, columns, select_list, table_exists},
    thread::projection_after,
};
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use rusqlite::{Transaction, params};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use t3_contracts::{DomainEvent, ModelSelection};

/// Events per transaction: bounded so the journal stays small, large enough that commits are not the cost.
pub const BATCH_EVENTS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Sql(#[from] rusqlite::Error),
    /// A row that does not decode. Names the row and the kind of problem; never the content.
    #[error("Event {sequence} ({event_type}, application event version {version}, {aggregate} {stream}) cannot be imported: {detail}")]
    Row { sequence: i64, event_type: String, version: i64, aggregate: String, stream: String, detail: String },
    #[error("{0}")]
    Other(String),
}

/// Error text without the values serde puts in quotes: names of fields and variants stay, content does not.
pub fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut quoted = false;
    for character in message.chars() {
        if character == '"' {
            quoted = !quoted;
            out.push('"');
        } else if !quoted {
            out.push(character);
        } else if out.ends_with('"') {
            out.push('…');
        }
    }
    out
}

/// A timestamp as the original writes one: ISO 8601, UTC, milliseconds.
pub fn iso(text: &str) -> Result<String, String> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Ok(parsed.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true));
    }
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .map(|naive| naive.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true))
        .map_err(|_| "a timestamp does not parse".to_owned())
}

pub struct EventRow {
    pub sequence: i64,
    pub event_id: String,
    pub command_id: Option<String>,
    pub aggregate_kind: String,
    pub stream_id: String,
    pub event_type: String,
    pub occurred_at: String,
    pub payload_json: String,
    pub metadata_json: String,
    pub causation_event_id: Option<String>,
    pub correlation_id: Option<String>,
    pub version: i64,
}

/// A row of the log as a Rust event, or what is wrong with it.
pub fn decode(row: &EventRow) -> Result<Event, String> {
    let payload: Value = serde_json::from_str(&row.payload_json).map_err(|error| format!("the payload is not JSON ({})", redact(&error.to_string())))?;
    let metadata: Value = serde_json::from_str(&row.metadata_json).map_err(|error| format!("the metadata is not JSON ({})", redact(&error.to_string())))?;
    if !metadata.is_object() {
        return Err("the metadata is not an object".into());
    }
    match row.aggregate_kind.as_str() {
        "thread" => {
            let mut wire = json!({"id":row.event_id,"threadId":row.stream_id,"type":row.event_type,"occurredAt":row.occurred_at,"payload":payload});
            for key in ["runId", "nodeId", "driver", "providerInstanceId", "rawEventId"] {
                if let Some(value) = metadata.get(key) {
                    wire[key] = value.clone();
                }
            }
            let decoded: DomainEvent = serde_json::from_value(wire).map_err(|error| redact(&error.to_string()))?;
            // The payload as the original's schema canonicalizes it, which is what the Rust planners write. The typed codecs keep every
            // field of every event type (`tests/event_payload_roundtrip.rs`), so what is stored is what the original wrote.
            Ok(Event {
                event_id: row.event_id.clone(),
                aggregate_kind: "thread".into(),
                aggregate_id: row.stream_id.clone(),
                occurred_at: iso(&row.occurred_at)?,
                command_id: row.command_id.clone(),
                causation_event_id: row.causation_event_id.clone(),
                correlation_id: row.correlation_id.clone(),
                event_type: row.event_type.clone(),
                payload: decoded.payload,
                metadata,
            })
        }
        "project" => {
            project_payload(&row.event_type, &payload, &row.stream_id)?;
            Ok(Event {
                event_id: row.event_id.clone(),
                aggregate_kind: "project".into(),
                aggregate_id: row.stream_id.clone(),
                occurred_at: iso(&row.occurred_at)?,
                command_id: row.command_id.clone(),
                causation_event_id: row.causation_event_id.clone(),
                correlation_id: row.correlation_id.clone(),
                event_type: row.event_type.clone(),
                payload,
                metadata,
            })
        }
        other => Err(format!("the aggregate kind {other} is not one this server knows")),
    }
}

/// `ApplicationProject*Payload`: the fields each project event must carry.
fn project_payload(event_type: &str, payload: &Value, stream: &str) -> Result<(), String> {
    let object = payload.as_object().ok_or("the payload is not an object")?;
    if object.get("projectId").and_then(Value::as_str) != Some(stream) {
        return Err("the payload names a different project than its stream".into());
    }
    let text = |key: &str| object.get(key).and_then(Value::as_str).filter(|value| !value.trim().is_empty()).is_some();
    let stamp = |key: &str| object.get(key).and_then(Value::as_str).is_some_and(|value| iso(value).is_ok());
    let ok = match event_type {
        "project.created" => text("title") && text("workspaceRoot") && stamp("createdAt") && stamp("updatedAt") && object.get("scripts").is_some_and(Value::is_array),
        "project.meta-updated" => stamp("updatedAt") && object.get("title").is_none_or(|_| text("title")) && object.get("workspaceRoot").is_none_or(|_| text("workspaceRoot")),
        "project.deleted" => stamp("deletedAt"),
        _ => return Err("the project event type is not one this server knows".into()),
    };
    if ok { Ok(()) } else { Err(format!("the {event_type} payload is missing or malformed in a required field")) }
}

/// One event through `thread::reduce` itself: the slow path the tests hold the projector to.
pub fn apply_reference(transaction: &Transaction<'_>, stored: &StoredEvent) -> Result<(), StoreError> {
    if stored.event.aggregate_kind == "project" {
        return reduce_project_event(transaction, stored);
    }
    crate::thread::reduce(transaction, stored)
}

/// Thread projections held across one transaction's events, so a thread's whole batch is parsed once and written once.
/// `apply` is `thread::reduce` with the projection kept in memory; the tests replay the same events through both.
#[derive(Default)]
pub struct Projector {
    threads: HashMap<String, Value>,
    dirty: HashSet<String>,
}
impl Projector {
    fn load(&mut self, transaction: &Transaction<'_>, id: &str) -> Result<Option<&Value>, StoreError> {
        if !self.threads.contains_key(id) {
            if let Some(value) = read_projection(transaction, "thread", id)? {
                self.threads.insert(id.to_owned(), value);
            }
        }
        Ok(self.threads.get(id))
    }
    pub fn apply(&mut self, transaction: &Transaction<'_>, stored: &StoredEvent) -> Result<(), StoreError> {
        let event = &stored.event;
        match event.aggregate_kind.as_str() {
            "project" => return reduce_project_event(transaction, stored),
            "thread" => {}
            other => return Err(StoreError::InvalidCommand(format!("no reducer for {other}"))),
        }
        self.load(transaction, &event.aggregate_id)?;
        crate::shared_rows::apply(transaction, event)?;
        let current = self.threads.remove(&event.aggregate_id);
        let mut projection = projection_after(current, event)?;
        if matches!(event.event_type.as_str(), "thread.created" | "turn-item.updated" | "run.updated" | "run-attempt.updated") && projection["thread"]["forkedFrom"]["type"] == "run" {
            let source_id = projection["thread"]["forkedFrom"]["threadId"].as_str().unwrap_or("").to_owned();
            if let Some(source) = self.load(transaction, &source_id)? {
                let local = projection["visibleTurnItems"].as_array().cloned().unwrap_or_default();
                projection["visibleTurnItems"] = Value::Array(crate::thread_fork::visible_with_inherited(&projection, source, local));
                crate::shell_state::inherit(&mut projection, source);
            }
        }
        self.threads.insert(event.aggregate_id.clone(), projection);
        self.dirty.insert(event.aggregate_id.clone());
        if matches!(event.event_type.as_str(), "context-transfer.created" | "context-transfer.updated") && matches!(event.payload["type"].as_str(), Some("fork" | "merge_back")) {
            let other = if event.payload["sourceThreadId"] == event.aggregate_id { &event.payload["targetThreadId"] } else { &event.payload["sourceThreadId"] };
            if let Some(other) = other.as_str().filter(|other| *other != event.aggregate_id) {
                self.load(transaction, other)?;
                if let Some(listed) = self.threads.get_mut(other) {
                    if let Some(transfers) = listed["contextTransfers"].as_array_mut() {
                        match transfers.iter_mut().find(|row| row["id"] == event.payload["id"]) {
                            Some(row) => *row = event.payload.clone(),
                            None => transfers.push(event.payload.clone()),
                        }
                        self.dirty.insert(other.to_owned());
                    }
                }
            }
        }
        Ok(())
    }
    pub fn flush(&mut self, transaction: &Transaction<'_>) -> Result<(), StoreError> {
        for id in self.dirty.drain() {
            if let Some(value) = self.threads.get(&id) {
                write_projection(transaction, "thread", &id, value)?;
            }
        }
        self.threads.clear();
        Ok(())
    }
}

pub fn insert_event(transaction: &Transaction<'_>, sequence: i64, event: &Event) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO rust_application_events(sequence,event_id,aggregate_kind,aggregate_id,command_id,event_type,occurred_at,event_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![sequence, event.event_id, event.aggregate_kind, event.aggregate_id, event.command_id, event.event_type, event.occurred_at, serde_json::to_string(event)?],
    )?;
    Ok(())
}

pub const EVENT_COLUMNS: &str = "sequence,event_id,command_id,aggregate_kind,stream_id,event_type,occurred_at,payload_json,metadata_json,causation_event_id,correlation_id,application_event_version";
pub fn read_rows(source: &Source, after: i64, limit: usize) -> Result<Vec<EventRow>, rusqlite::Error> {
    let mut statement = source.connection.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM orchestration_events WHERE application_event_version = 2 AND sequence > ?1 ORDER BY sequence LIMIT ?2"
    ))?;
    statement
        .query_map(params![after, limit as i64], |row| {
            Ok(EventRow {
                sequence: row.get(0)?,
                event_id: row.get(1)?,
                command_id: row.get(2)?,
                aggregate_kind: row.get(3)?,
                stream_id: row.get(4)?,
                event_type: row.get(5)?,
                occurred_at: row.get(6)?,
                payload_json: row.get(7)?,
                metadata_json: row.get(8)?,
                causation_event_id: row.get(9)?,
                correlation_id: row.get(10)?,
                version: row.get(11)?,
            })
        })?
        .collect()
}

/// Events of the log whose application event version is not 2: V1 events, which the original's V2 code never reads.
pub fn legacy_event_count(source: &Source) -> Result<u64, rusqlite::Error> {
    if !table_exists(&source.connection, "orchestration_events")? || !columns(&source.connection, "orchestration_events")?.contains("application_event_version") {
        return Ok(if table_exists(&source.connection, "orchestration_events")? { source.connection.query_row("SELECT COUNT(*) FROM orchestration_events", [], |row| row.get(0))? } else { 0 });
    }
    source.connection.query_row("SELECT COUNT(*) FROM orchestration_events WHERE application_event_version <> 2", [], |row| row.get(0))
}

pub fn v2_events_present(source: &Source) -> Result<bool, rusqlite::Error> {
    Ok(table_exists(&source.connection, "orchestration_events")? && columns(&source.connection, "orchestration_events")?.contains("application_event_version"))
}

// ---------------------------------------------------------------------------------------------------------------------
// V1: what the original's `LegacyV1ThreadImporter` and migration 55's project baseline make of the V1 projection tables.
// ---------------------------------------------------------------------------------------------------------------------

fn text(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => Ok(Some(String::from_utf8_lossy(bytes).into_owned())),
        rusqlite::types::ValueRef::Integer(value) => Ok(Some(value.to_string())),
        rusqlite::types::ValueRef::Real(value) => Ok(Some(value.to_string())),
    }
}

fn parse(json_text: &Option<String>) -> Option<Value> {
    json_text.as_deref().and_then(|raw| serde_json::from_str(raw).ok())
}

/// A stored model selection in the current shape (`instanceId`, `options` as a list), or the default the original uses.
fn model_selection(stored: &Option<String>, legacy_model: &Option<String>, provider: &Option<String>) -> Value {
    let canonical = |value: &Value| serde_json::from_value::<ModelSelection>(value.clone()).ok().and_then(|selection| serde_json::to_value(selection).ok());
    if let Some(value) = parse(stored).as_ref().and_then(canonical) {
        return value;
    }
    if let (None, Some(model)) = (stored, legacy_model) {
        // Before migration 16 the thread kept a bare model name; the migration derived the driver as this does.
        let driver = provider.clone().unwrap_or_else(|| if model.to_lowercase().contains("claude") { "claudeAgent".into() } else { "codex".into() });
        if let Some(value) = canonical(&json!({"provider":driver,"model":model})) {
            return value;
        }
    }
    json!({"instanceId":"codex","model":"gpt-6-astra"})
}

/// A synthesized thread event with the payload the original's schema would store for it; the same strictness as a row of the log.
pub fn canonical_thread_event(mut event: Event) -> Result<Event, String> {
    let wire = json!({"id":event.event_id,"threadId":event.aggregate_id,"type":event.event_type,"occurredAt":event.occurred_at,"payload":event.payload});
    let decoded: DomainEvent = serde_json::from_value(wire).map_err(|error| redact(&error.to_string()))?;
    event.payload = decoded.payload;
    Ok(event)
}

/// One `project.created` (and `project.deleted`) per V1 project row that has no project event, as migration 55 baselines them.
pub fn v1_project_events(source: &Source, existing: &HashSet<String>) -> Result<Vec<Event>, EventError> {
    let connection = &source.connection;
    if !table_exists(connection, "projection_projects")? {
        return Ok(Vec::new());
    }
    let available = columns(connection, "projection_projects")?;
    let list = select_list(&available, &["project_id", "title", "workspace_root", "default_model_selection_json", "default_model", "scripts_json", "created_at", "updated_at", "deleted_at"]);
    let mut statement = connection.prepare(&format!("SELECT {list} FROM projection_projects ORDER BY created_at, project_id"))?;
    let mut rows = statement.query([])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        let id = text(row, 0)?.unwrap_or_default();
        if existing.contains(&id) {
            continue;
        }
        let fail = |detail: &str| EventError::Other(format!("project row {id} cannot be imported: {detail}"));
        let (title, root) = (text(row, 1)?.unwrap_or_default(), text(row, 2)?.unwrap_or_default());
        let stored = text(row, 3)?;
        let legacy_model = text(row, 4)?;
        let selection = if stored.is_none() && legacy_model.is_none() { Value::Null } else { model_selection(&stored, &legacy_model, &None) };
        let scripts: Value = text(row, 5)?.and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or(json!([]));
        let created = iso(&text(row, 6)?.unwrap_or_default()).map_err(|detail| fail(&detail))?;
        let updated = iso(&text(row, 7)?.unwrap_or_default()).map_err(|detail| fail(&detail))?;
        let baseline = format!("migration:39:project:{id}:baseline");
        let metadata = json!({"applicationEventVersion":2,"migration":39});
        events.push(Event {
            event_id: baseline.clone(),
            aggregate_kind: "project".into(),
            aggregate_id: id.clone(),
            occurred_at: updated.clone(),
            command_id: None,
            causation_event_id: None,
            correlation_id: None,
            event_type: "project.created".into(),
            payload: json!({"projectId":id,"title":title,"workspaceRoot":root,"defaultModelSelection":selection,"scripts":scripts,"createdAt":created,"updatedAt":updated}),
            metadata: metadata.clone(),
        });
        if let Some(deleted) = text(row, 8)? {
            let deleted = iso(&deleted).map_err(|detail| fail(&detail))?;
            events.push(Event {
                event_id: format!("migration:39:project:{id}:deleted"),
                aggregate_kind: "project".into(),
                aggregate_id: id.clone(),
                occurred_at: deleted.clone(),
                command_id: None,
                causation_event_id: Some(baseline),
                correlation_id: None,
                event_type: "project.deleted".into(),
                payload: json!({"projectId":id,"deletedAt":deleted}),
                metadata,
            });
        }
    }
    Ok(events)
}

/// The project columns the original serves from `projection_projects` that no event of a V1-origin project carries, as a
/// `project.meta-updated` per project whose Rust projection differs. Applies to V2 files too: a project that came from V1 keeps
/// these only in the table.
pub fn project_reconciliation(source: &Source, transaction: &rusqlite::Connection, now: &str) -> Result<(Vec<Event>, u64), EventError> {
    let connection = &source.connection;
    if !table_exists(connection, "projection_projects")? {
        return Ok((Vec::new(), 0));
    }
    let available = columns(connection, "projection_projects")?;
    let list = select_list(&available, &["project_id", "default_thread_env_mode", "auto_pull", "favicon_path", "project_icon_json", "updated_at"]);
    let mut statement = connection.prepare(&format!("SELECT {list} FROM projection_projects ORDER BY created_at, project_id"))?;
    let mut rows = statement.query([])?;
    let (mut events, mut checked) = (Vec::new(), 0);
    while let Some(row) = rows.next()? {
        let id = text(row, 0)?.unwrap_or_default();
        let Some(project) = read_projection(transaction, "project", &id)? else { continue };
        checked += 1;
        let mut patch = Map::new();
        let mode = text(row, 1)?;
        let wanted_mode = mode.map_or(Value::Null, Value::String);
        if wanted_mode != project["defaultThreadEnvMode"] && !(wanted_mode.is_null() && project["defaultThreadEnvMode"].is_null()) {
            patch.insert("defaultThreadEnvMode".into(), wanted_mode);
        }
        let pull = text(row, 2)?.is_some_and(|value| value == "1" || value == "true");
        if pull != (project["autoPull"] == true) {
            patch.insert("autoPull".into(), json!(pull));
        }
        let favicon = text(row, 3)?.map_or(Value::Null, Value::String);
        if favicon != project.get("faviconPath").cloned().unwrap_or(Value::Null) {
            patch.insert("faviconPath".into(), favicon);
        }
        let icon = parse(&text(row, 4)?).unwrap_or(Value::Null);
        if icon != project.get("projectIcon").cloned().unwrap_or(Value::Null) {
            patch.insert("projectIcon".into(), icon);
        }
        if patch.is_empty() {
            continue;
        }
        let stamp = text(row, 5)?.and_then(|value| iso(&value).ok()).unwrap_or_else(|| now.to_owned());
        patch.insert("projectId".into(), json!(id));
        patch.insert("updatedAt".into(), project["updatedAt"].clone());
        let _ = stamp;
        events.push(Event {
            event_id: format!("migration:reconcile:project:{id}"),
            aggregate_kind: "project".into(),
            aggregate_id: id,
            occurred_at: now.to_owned(),
            command_id: None,
            causation_event_id: None,
            correlation_id: None,
            event_type: "project.meta-updated".into(),
            payload: Value::Object(patch),
            metadata: json!({"applicationEventVersion":2,"migration":"state-import"}),
        });
    }
    Ok((events, checked))
}

/// The V1 threads (rows of `projection_threads`) that have no V2 `thread.created` event, with their messages: the events the
/// original's importer writes (`migration:v1:` ids), every message at once instead of on first open.
pub struct LegacyThreads {
    pub threads: Vec<(String, String)>,
}
pub fn v1_thread_ids(source: &Source, with_events: &HashSet<String>) -> Result<LegacyThreads, rusqlite::Error> {
    let connection = &source.connection;
    if !table_exists(connection, "projection_threads")? {
        return Ok(LegacyThreads { threads: Vec::new() });
    }
    let mut statement = connection.prepare("SELECT thread_id, created_at FROM projection_threads ORDER BY created_at, thread_id")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut threads = Vec::new();
    for row in rows {
        let (id, created) = row?;
        if !with_events.contains(&id) {
            threads.push((id, created));
        }
    }
    Ok(LegacyThreads { threads })
}

const THREAD_COLUMNS: [&str; 24] = [
    "thread_id", "project_id", "title", "model_selection_json", "model", "runtime_mode", "interaction_mode", "branch", "worktree_path", "created_at", "updated_at", "archived_at",
    "settled_override", "settled_at", "unsettled_at", "snoozed_until", "snoozed_at", "pinned_at", "auto_settle_disabled_at", "pin_order_key", "linked_pull_request_json",
    "branch_pull_request_json", "active_order_key", "deleted_at",
];

fn nullable(value: Option<String>) -> Result<Value, String> {
    value.map_or(Ok(Value::Null), |text| iso(&text).map(Value::String))
}
fn trimmed(value: Option<String>) -> Value {
    value.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty()).map_or(Value::Null, Value::String)
}

/// `LegacyV1ThreadImporter`'s thread and message events for one V1 thread, in the order they are applied.
pub fn v1_thread_events(source: &Source, id: &str) -> Result<Vec<Event>, EventError> {
    let connection = &source.connection;
    let fail = |detail: &str| EventError::Other(format!("legacy thread {id} cannot be imported: {detail}"));
    let available = columns(connection, "projection_threads")?;
    let list = select_list(&available, &THREAD_COLUMNS);
    let mut statement = connection.prepare(&format!("SELECT {list} FROM projection_threads WHERE thread_id = ?1"))?;
    let mut rows = statement.query([id])?;
    let row = rows.next()?.ok_or_else(|| fail("the row is gone"))?;
    let get = |index: usize| text(row, index);
    let thread_id = get(0)?.unwrap_or_default();
    let project_id = get(1)?.unwrap_or_default();
    let title = get(2)?.unwrap_or_default();
    let (stored_selection, legacy_model) = (get(3)?, get(4)?);
    let provider: Option<String> = if table_exists(connection, "projection_thread_sessions")? {
        connection.query_row("SELECT provider_name FROM projection_thread_sessions WHERE thread_id = ?1", [id], |row| row.get(0)).ok().flatten()
    } else {
        None
    };
    let selection = model_selection(&stored_selection, &legacy_model, &provider);
    let runtime = get(5)?.unwrap_or_default();
    let interaction = get(6)?.unwrap_or_default();
    let (branch, worktree) = (trimmed(get(7)?), trimmed(get(8)?));
    let created = iso(&get(9)?.unwrap_or_default()).map_err(|detail| fail(&detail))?;
    let updated = iso(&get(10)?.unwrap_or_default()).map_err(|detail| fail(&detail))?;
    let archived = nullable(get(11)?).map_err(|detail| fail(&detail))?;
    let settled_override = get(12)?.filter(|value| value == "settled" || value == "active").map_or(Value::Null, Value::String);
    let settled_at = nullable(get(13)?).map_err(|detail| fail(&detail))?;
    let unsettled_at = nullable(get(14)?).map_err(|detail| fail(&detail))?;
    let snoozed_until = nullable(get(15)?).map_err(|detail| fail(&detail))?;
    let snoozed_at = nullable(get(16)?).map_err(|detail| fail(&detail))?;
    let pinned_at = nullable(get(17)?).map_err(|detail| fail(&detail))?;
    let auto_settle = nullable(get(18)?).map_err(|detail| fail(&detail))?;
    let pin_order = trimmed(get(19)?);
    let linked = parse(&get(20)?).unwrap_or(Value::Null);
    let branch_pr = parse(&get(21)?).unwrap_or(Value::Null);
    let active_order = trimmed(get(22)?);
    let deleted = nullable(get(23)?).map_err(|detail| fail(&detail))?;
    drop(rows);
    // Pull request links: the table (migration 50) and the older single link, as `threadPullRequestsOf` merges them.
    let mut links: Vec<Value> = Vec::new();
    if table_exists(connection, "projection_thread_pull_requests")? {
        let mut query = connection.prepare("SELECT host,repository,number,url,source,linked_at,snapshot_json,stack_json FROM projection_thread_pull_requests WHERE thread_id = ?1 ORDER BY rowid")?;
        let found = query.query_map([id], |row| {
            Ok(json!({"host":row.get::<_, String>(0)?,"repository":row.get::<_, String>(1)?,"number":row.get::<_, i64>(2)?,"url":row.get::<_, String>(3)?,"source":row.get::<_, String>(4)?,"linkedAt":row.get::<_, String>(5)?,
                "snapshot":row.get::<_, Option<String>>(6)?.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()),"stack":row.get::<_, Option<String>>(7)?.and_then(|raw| serde_json::from_str::<Value>(&raw).ok())}))
        })?;
        for link in found {
            let link = link?;
            links.push(link);
        }
    }
    // The older single link (`threadPullRequestsOf`): the key is read from the URL's host, and the link is a manual one made at the epoch.
    if let Some(old) = linked.as_object() {
        let url = old.get("url").and_then(Value::as_str).unwrap_or_default();
        let host = url::Url::parse(url).ok().and_then(|parsed| parsed.host_str().map(str::to_owned)).unwrap_or_default();
        let (repository, number) = (old.get("repository").cloned().unwrap_or(Value::Null), old.get("number").cloned().unwrap_or(Value::Null));
        let present = links.iter().any(|link| link["host"] == json!(host) && link["repository"] == repository && link["number"] == number);
        if !present {
            links.push(json!({"host":host,"repository":repository,"number":number,"url":url,"source":"manual","linkedAt":"1970-01-01T00:00:00.000Z","snapshot":null,"stack":null}));
        }
    }
    let runtime = match runtime.as_str() {
        "approval-required" | "auto-accept-edits" | "auto" | "full-access" => runtime,
        _ => "full-access".into(),
    };
    let thread = json!({
        "createdBy":"system","creationSource":"server","id":thread_id,"projectId":project_id,
        "title":if title.trim().is_empty() { "Untitled thread".to_owned() } else { title },
        "providerInstanceId":selection["instanceId"],"modelSelection":selection,
        "runtimeMode":runtime,"interactionMode":if interaction == "plan" { "plan" } else { "default" },
        "branch":branch,"worktreePath":worktree,"linkedPullRequest":if linked.is_object() { linked } else { Value::Null },
        "pullRequests":links,"branchPullRequest":if branch_pr.is_object() { branch_pr } else { Value::Null },
        "activeOrderKey":active_order,"activeProviderThreadId":null,"historyOrigin":"v1_import",
        "lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":thread_id},"forkedFrom":null,
        "createdAt":created,"updatedAt":updated,"archivedAt":archived,"settledOverride":settled_override,"settledAt":settled_at,
        "unsettledAt":unsettled_at,"snoozedUntil":snoozed_until,"snoozedAt":snoozed_at,"pinnedAt":pinned_at,
        "autoSettleDisabledAt":auto_settle,"pinOrderKey":pin_order,"lastVisitedAt":null,"deletedAt":deleted,
    });
    let make = |event_id: String, event_type: &str, occurred: &str, payload: Value| Event {
        event_id,
        aggregate_kind: "thread".into(),
        aggregate_id: thread_id.clone(),
        occurred_at: occurred.to_owned(),
        command_id: None,
        causation_event_id: None,
        correlation_id: None,
        event_type: event_type.into(),
        payload,
        metadata: json!({"applicationEventVersion":2,"providerInstanceId":selection["instanceId"]}),
    };
    let mut events = vec![make(format!("migration:v1:thread:{thread_id}:created"), "thread.created", &created, thread.clone())];
    if table_exists(connection, "projection_thread_messages")? {
        let available = columns(connection, "projection_thread_messages")?;
        let list = select_list(&available, &["message_id", "role", "text", "attachments_json", "context_json", "is_streaming", "created_at", "updated_at"]);
        let mut query = connection.prepare(&format!("SELECT {list} FROM projection_thread_messages WHERE thread_id = ?1 AND role IN ('user','assistant') ORDER BY created_at, message_id"))?;
        let mut rows = query.query([id])?;
        let mut ordinal = 0;
        while let Some(row) = rows.next()? {
            ordinal += 1;
            let message_id = text(row, 0)?.unwrap_or_default();
            let role = text(row, 1)?.unwrap_or_default();
            let body = text(row, 2)?.unwrap_or_default();
            let attachments = parse(&text(row, 3)?).filter(Value::is_array).unwrap_or(json!([]));
            let context = parse(&text(row, 4)?).filter(Value::is_object);
            let streaming = text(row, 5)?.is_some_and(|value| value == "1");
            let (made, changed) = (iso(&text(row, 6)?.unwrap_or_default()).map_err(|detail| fail(&detail))?, iso(&text(row, 7)?.unwrap_or_default()).map_err(|detail| fail(&detail))?);
            let user = role == "user";
            let mut message = json!({"createdBy":if user { "user" } else { "agent" },"creationSource":"server","id":message_id,"threadId":thread_id,"runId":null,"nodeId":null,"role":role,"text":body,"attachments":attachments,"streaming":false,"createdAt":made,"updatedAt":changed});
            let mut item = json!({"id":format!("migration:v1:turn-item:{message_id}"),"threadId":thread_id,"runId":null,"nodeId":null,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":ordinal,
                "status":if streaming { "interrupted" } else { "completed" },"title":null,"startedAt":made,"completedAt":changed,"updatedAt":changed,"messageId":message_id,"text":body});
            if user {
                item["createdBy"] = json!("user");
                item["creationSource"] = json!("server");
                item["type"] = json!("user_message");
                item["inputIntent"] = json!("turn_start");
                item["attachments"] = attachments;
            } else {
                item["type"] = json!("assistant_message");
                item["streaming"] = json!(false);
            }
            if let Some(context) = context {
                message["context"] = context.clone();
                item["context"] = context;
            }
            events.push(make(format!("migration:v1:message:{message_id}"), "message.updated", &changed, message));
            events.push(make(format!("migration:v1:turn-item:{message_id}"), "turn-item.updated", &changed, item));
        }
    }
    events.push(make(format!("migration:v1:thread:{thread_id}:shell"), "thread.metadata-updated", &updated, thread));
    Ok(events)
}
