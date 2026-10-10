//! Import of the original T3 Code server's state into this server's own store.
//!
//! **Model: import once into a file of our own.** The two stores cannot be one file: the original keeps typed projection tables,
//! this server keeps a JSON projection per thread (`rust_projections`) and its own event, receipt and outbox tables, none of
//! which the original's migrations know and which its code would never read. Opening `statev2.sqlite` in place would mean
//! rewriting the store and giving up the original's ability to open it again; importing leaves the original's files exactly as
//! they were (they are opened `immutable=1`, or as a private copy when a write-ahead log is present) and the original keeps working.
//!
//! The import builds `rust-state.sqlite.importing` beside the destination, in transactions of bounded size, and renames it to
//! `rust-state.sqlite` only when every phase is done and the file passes `quick_check`. The server never opens the staging
//! file, so a crash, a refusal or a failed row leaves no state it would start on. The staging file records its progress and the
//! source's fingerprint: an interrupted import continues from the last commit when the source is unchanged, and starts clean
//! when it is not. A finished import leaves `rust_import_record` (what, from which schema version, when, how many) in the
//! store; starting the server again, or running the command again, sees it and imports nothing.
use crate::{
    persistence::{Store, StoreError, StoredEvent},
    state_import_events::{self as events, BATCH_EVENTS, EventError, Projector, canonical_thread_event, insert_event},
    state_import_source::{self as source, Source, SourceError, columns, count, select_list, table_exists},
};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

pub use crate::state_import_source::Kind;
pub const IMPORTER_VERSION: u32 = 1;
pub const DESTINATION: &str = "rust-state.sqlite";
const STAGING: &str = "rust-state.sqlite.importing";

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Event(#[from] EventError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Sql(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
    /// Only returned when `Options::stop_after` asks for it: the staging file is left as a crash would leave it.
    #[error("The import was stopped after {0}.")]
    Stopped(String),
    #[error("{0} already holds state written by this server and no import record, so nothing was imported over it.")]
    DestinationHasState(String),
    #[error("The import staging file in {0} belongs to another import and could not be removed: {1}")]
    Staging(String, String),
}

pub type Progress<'a> = &'a mut dyn FnMut(String);

pub struct Options<'a> {
    /// A T3 home, or its `userdata` directory.
    pub from: PathBuf,
    /// Where this server keeps its state; `rust-state.sqlite` is created in it.
    pub state_dir: PathBuf,
    /// Return `Stopped` after the named phase (`events`, `events:<batches>`, `projects`, `legacy-threads`, `receipts`,
    /// `scheduled-tasks`, `history`, `files`), leaving the staging file for a later run. For tests.
    pub stop_after: Option<&'a str>,
    /// Reduce each event with `thread::reduce` itself instead of the batching projector. For tests.
    pub reference_reducer: bool,
    pub batch_events: usize,
}
impl Options<'_> {
    pub fn new(from: PathBuf, state_dir: PathBuf) -> Self {
        Self { from, state_dir, stop_after: None, reference_reducer: false, batch_events: BATCH_EVENTS }
    }
}

#[derive(Debug, Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub source_kind: Option<Kind>,
    pub source_migration: i64,
    pub source_bytes: u64,
    pub destination_bytes: u64,
    pub resumed: bool,
    pub seconds: f64,
    /// Every table of the source: `[rows read, rows now in the store]` and what became of the rest.
    pub tables: BTreeMap<String, TableReport>,
    /// Rows of the event log by type: `[read, stored]`.
    pub events: BTreeMap<String, [u64; 2]>,
    /// Payload fields of events that the Rust codecs do not model and the import kept as written: `event type.field` and how many events.
    pub retained_fields: BTreeMap<String, u64>,
    pub legacy_events_not_carried: u64,
    pub legacy_threads_imported: u64,
    pub legacy_projects_imported: u64,
    pub projects_reconciled: u64,
    /// Rows in the source's projection tables against the rows now in the thread projections.
    pub projection_checks: BTreeMap<String, [u64; 2]>,
    pub files: Vec<FileReport>,
    pub credentials_to_reenter: Vec<String>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TableReport {
    pub read: u64,
    pub imported: u64,
    pub disposition: String,
}
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileReport {
    pub name: String,
    pub action: String,
    pub count: u64,
}

#[derive(Debug)]
pub enum Outcome {
    /// There was no database to import.
    NothingToImport,
    /// The destination already holds an import of this source.
    AlreadyImported,
    Imported(Box<Report>),
}

const PROJECTION_COMPARISONS: [(&str, &str); 12] = [
    ("orchestration_v2_projection_threads", "thread"),
    ("orchestration_v2_projection_runs", "runs"),
    ("orchestration_v2_projection_run_attempts", "attempts"),
    ("orchestration_v2_projection_nodes", "nodes"),
    ("orchestration_v2_projection_provider_threads", "providerThreads"),
    ("orchestration_v2_projection_provider_turns", "providerTurns"),
    ("orchestration_v2_projection_runtime_requests", "runtimeRequests"),
    ("orchestration_v2_projection_messages", "messages"),
    ("orchestration_v2_projection_plans", "plans"),
    ("orchestration_v2_projection_turn_items", "turnItems"),
    ("orchestration_v2_projection_checkpoints", "checkpoints"),
    ("orchestration_v2_projection_subagents", "subagents"),
];

/// What becomes of each table the original can have. A table not listed here is reported as unknown.
fn disposition(table: &str) -> &'static str {
    match table {
        "orchestration_events" => "application events (version 2) replayed into rust_application_events and the thread and project projections; V1 events are not carried",
        "orchestration_command_receipts" => "copied to rust_command_receipts",
        "scheduled_tasks" => "copied to rust_scheduled_tasks",
        "scheduled_task_webhook_deliveries" => "copied to rust_scheduled_task_webhook_deliveries",
        "scheduled_task_webhook_relay_deliveries" => "copied to rust_scheduled_task_webhook_relay_deliveries",
        "pull_request_files_viewed" => "folded into the pullRequestFilesViewed projections",
        "mcp_app_model_context" => "folded into the mcp_app_context projections",
        "projection_projects" => "V1 projects without a project event are baselined as the original's migration 55 does; project columns that no event carries are reconciled; deleted projects stay deleted",
        "projection_threads" | "projection_thread_messages" | "projection_thread_pull_requests" => "V1 threads without a V2 thread.created are imported with their messages as the original's legacy importer does",
        "projection_thread_sessions" | "provider_session_runtime" => "provider names kept for the settings migration that restores the providers a user has used",
        "effect_sql_migrations" => "recorded in rust_import_record",
        "orchestration_v2_effect_outbox" => "not carried: pending or running effects would run provider turns and side effects again",
        "orchestration_v2_thread_launch_workflows" => "not carried: in-flight launches are not resumed",
        "orchestration_v2_projection_provider_session_bindings" | "orchestration_v2_projection_metadata" | "orchestration_v2_turn_item_positions" | "orchestration_v2_legacy_imports" => "derived or bookkeeping rows of the original's store; not carried",
        "auth_sessions" | "auth_pairing_links" => "not carried: sessions are signed with a key this server does not share, devices pair again",
        "checkpoint_diff_blobs" | "projection_thread_activities" | "projection_turns" | "projection_pending_approvals" | "projection_state" | "projection_thread_proposed_plans" => "V1 projection detail the original's own V2 code does not read; not carried",
        "orchestration_v2_events" | "orchestration_v2_command_receipts" => "pre-release V2 tables that migration 55 already folded into the log; not carried",
        name if name.starts_with("orchestration_v2_projection_") => "derived from the events by the original; the same projections are rebuilt from the events",
        _ => "unknown table; not carried",
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The staging file.
// ---------------------------------------------------------------------------------------------------------------------

fn progress_get(store: &Store, key: &str) -> Result<Option<String>, StoreError> {
    store.read(|connection| Ok(connection.query_row("SELECT value FROM rust_import_progress WHERE key = ?1", [key], |row| row.get(0)).optional()?))
}
fn progress_set(transaction: &Transaction<'_>, key: &str, value: &str) -> Result<(), StoreError> {
    transaction.execute("INSERT INTO rust_import_progress(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    Ok(())
}

fn open_staging(state_dir: &Path, fingerprint: &str, report: &mut Report) -> Result<Store, ImportError> {
    let staging = state_dir.join(STAGING);
    if staging.exists() {
        let store = Store::open(&staging)?;
        let usable = store
            .read(|connection| {
                let present = table_exists(connection, "rust_import_progress")?;
                Ok(present
                    && connection
                        .query_row("SELECT value FROM rust_import_progress WHERE key = 'fingerprint'", [], |row| row.get::<_, String>(0))
                        .optional()?
                        .is_some_and(|stored| stored == format!("{IMPORTER_VERSION}:{fingerprint}")))
            })
            .unwrap_or(false);
        if usable {
            report.resumed = true;
            return Ok(store);
        }
        drop(store);
        remove_database(&staging).map_err(|error| ImportError::Staging(state_dir.display().to_string(), error.to_string()))?;
    }
    let store = Store::open(&staging)?;
    store.transaction(|transaction| {
        transaction.execute_batch(
            "CREATE TABLE rust_import_progress(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE rust_import_record(id INTEGER PRIMARY KEY CHECK (id = 1), record_json TEXT NOT NULL);",
        )?;
        progress_set(transaction, "fingerprint", &format!("{IMPORTER_VERSION}:{fingerprint}"))
    })?;
    Ok(store)
}

fn remove_database(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::remove_file(PathBuf::from(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn stop(options: &Options<'_>, phase: &str) -> Result<(), ImportError> {
    if options.stop_after == Some(phase) {
        return Err(ImportError::Stopped(phase.to_owned()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// The import.
// ---------------------------------------------------------------------------------------------------------------------

/// The destination already holds an import: `Some(record)`; holds this server's own state with none: an error.
fn existing_record(path: &Path) -> Result<Option<Value>, ImportError> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if table_exists(&connection, "rust_import_record")? {
        let json: Option<String> = connection.query_row("SELECT record_json FROM rust_import_record WHERE id = 1", [], |row| row.get(0)).optional()?;
        return Ok(json.and_then(|raw| serde_json::from_str(&raw).ok()));
    }
    Ok(None)
}

/// Whether an import is due in `state_dir`: no store yet, and a database of the original beside it or at `from`.
pub fn due(state_dir: &Path) -> bool {
    !state_dir.join(DESTINATION).exists() && (state_dir.join("statev2.sqlite").exists() || state_dir.join("state.sqlite").exists())
}

/// Runs the import; a failure other than a requested stop removes the staging file, so a retry begins from the source again.
pub fn import(options: Options<'_>, progress: Progress<'_>) -> Result<Outcome, ImportError> {
    let staging = options.state_dir.join(STAGING);
    let result = import_into_staging(&options, progress);
    if matches!(&result, Err(error) if !matches!(error, ImportError::Stopped(_))) {
        let _ = remove_database(&staging);
    }
    result
}

fn import_into_staging(options: &Options<'_>, progress: Progress<'_>) -> Result<Outcome, ImportError> {
    let started = Instant::now();
    std::fs::create_dir_all(&options.state_dir)?;
    let state_dir = std::fs::canonicalize(&options.state_dir)?;
    let destination = state_dir.join(DESTINATION);
    let source = match source::open(&options.from, &state_dir) {
        Ok(source) => source,
        Err(SourceError::Missing(_)) => {
            // Nothing to import is only an answer when the destination is not already imported over.
            return Ok(Outcome::NothingToImport);
        }
        Err(error) => return Err(error.into()),
    };
    if destination.exists() {
        return match existing_record(&destination)? {
            Some(_) => Ok(Outcome::AlreadyImported),
            None => Err(ImportError::DestinationHasState(state_dir.display().to_string())),
        };
    }
    let mut report = Report { source_kind: Some(source.kind), source_migration: source.migration, source_bytes: source.bytes, ..Default::default() };
    progress(format!("importing {:?} database ({} bytes, schema migration {}) into {}", source.kind, source.bytes, source.migration, state_dir.display()));
    let store = open_staging(&state_dir, &source.fingerprint, &mut report)?;
    if report.resumed {
        progress("continuing an interrupted import of the same database".to_owned());
    }
    // Every table of the source and the rows it holds, before anything is read.
    {
        let mut statement = source.connection.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
        let names: Vec<String> = statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
        for name in names {
            let rows = count(&source.connection, &name)?;
            report.tables.insert(name.clone(), TableReport { read: rows, imported: 0, disposition: disposition(&name).to_owned() });
        }
    }
    let phase_done = |name: &str| progress_get(&store, &format!("done:{name}")).map(|value| value.is_some());
    let mark = |name: &str| store.transaction(|transaction| progress_set(transaction, &format!("done:{name}"), "1"));

    // 1. Projects and threads: the event log, then the V1 rows the original would import itself.
    if !phase_done("events")? {
        replay_events(options, &source, &store, &mut report, progress)?;
        mark("events")?;
    }
    count_events(&source, &store, &mut report)?;
    stop(options, "events")?;
    if !phase_done("projects")? {
        project_phase(&source, &store, &mut report)?;
        mark("projects")?;
    }
    stop(options, "projects")?;
    if !phase_done("legacy-threads")? {
        legacy_threads(options, &source, &store, &mut report, progress)?;
        mark("legacy-threads")?;
    }
    stop(options, "legacy-threads")?;
    // 2. Rows beside the log.
    if !phase_done("receipts")? {
        receipts(&source, &store, &mut report)?;
        mark("receipts")?;
    }
    stop(options, "receipts")?;
    if !phase_done("scheduled-tasks")? {
        scheduled_tasks(&source, &store, &mut report)?;
        mark("scheduled-tasks")?;
    }
    stop(options, "scheduled-tasks")?;
    if !phase_done("history")? {
        history(&source, &store, &mut report)?;
        mark("history")?;
    }
    stop(options, "history")?;
    // 3. Files.
    let userdata = source::userdata_of(&options.from);
    if !phase_done("files")? {
        crate::state_import_files::copy(&userdata, &state_dir, &mut report)?;
        mark("files")?;
    }
    stop(options, "files")?;
    // 4. What is outstanding in the source and left behind on purpose.
    leftovers(&source, &mut report)?;
    compare_projections(&source, &store, &mut report)?;

    // Publish: a record, then the staging file becomes the store in one rename.
    report.seconds = started.elapsed().as_secs_f64();
    let record = json!({
        "importerVersion": IMPORTER_VERSION,
        "sourceKind": source.kind,
        "sourceFingerprint": source.fingerprint,
        "sourceSchemaMigration": source.migration,
        "sourceMigrations": source.migrations.iter().map(|(id, name)| json!({"id":id,"name":name})).collect::<Vec<_>>(),
        "importedAt": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "report": &report,
    });
    store.transaction(|transaction| {
        transaction.execute("INSERT OR REPLACE INTO rust_import_record(id,record_json) VALUES(1,?1)", [serde_json::to_string(&record)?])?;
        transaction.execute("DROP TABLE rust_import_progress", [])?;
        Ok(())
    })?;
    drop(store);
    let staging = state_dir.join(STAGING);
    {
        let check = Connection::open(&staging)?;
        let verdict: String = check.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        if verdict != "ok" {
            return Err(ImportError::Other(format!("the imported store failed its own check: {verdict}")));
        }
        check.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    }
    for suffix in ["-wal", "-shm"] {
        let mut name = staging.as_os_str().to_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
    report.destination_bytes = std::fs::metadata(&staging)?.len();
    std::fs::rename(&staging, &destination)?;
    if let Ok(directory) = std::fs::File::open(&state_dir) {
        let _ = directory.sync_all();
    }
    let _ = std::fs::write(state_dir.join("rust-import-report.json"), serde_json::to_vec_pretty(&report).unwrap_or_default());
    progress(format!("import complete in {:.1}s", started.elapsed().as_secs_f64()));
    Ok(Outcome::Imported(Box::new(report)))
}

// ---------------------------------------------------------------------------------------------------------------------
// Phases.
// ---------------------------------------------------------------------------------------------------------------------

fn event_cursor(store: &Store) -> Result<i64, ImportError> {
    Ok(progress_get(store, "events_cursor")?.and_then(|value| value.parse().ok()).unwrap_or(0))
}

fn count_events(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    if events::v2_events_present(source)? {
        let mut statement = source.connection.prepare("SELECT event_type, COUNT(*) FROM orchestration_events WHERE application_event_version = 2 GROUP BY event_type")?;
        for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)))? {
            let (kind, read) = row?;
            report.events.insert(kind, [read, 0]);
        }
    }
    let stored: Vec<(String, u64)> = store.read(|connection| {
        let mut statement = connection.prepare("SELECT event_type, COUNT(*) FROM rust_application_events WHERE event_id NOT LIKE 'migration:%' GROUP BY event_type")?;
        Ok(statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?)
    })?;
    for (kind, stored) in stored {
        report.events.entry(kind).or_insert([0, 0])[1] = stored;
    }
    Ok(())
}

fn replay_events(options: &Options<'_>, source: &Source, store: &Store, report: &mut Report, progress: Progress<'_>) -> Result<(), ImportError> {
    report.legacy_events_not_carried = events::legacy_event_count(source)?;
    if !events::v2_events_present(source)? {
        return Ok(());
    }
    let total: u64 = source.connection.query_row("SELECT COUNT(*) FROM orchestration_events WHERE application_event_version = 2", [], |row| row.get(0))?;
    let mut cursor = event_cursor(store)?;
    let mut done: u64 = store.read(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM rust_application_events WHERE event_id NOT LIKE 'migration:%'", [], |row| row.get(0))?))?;
    let started = Instant::now();
    let mut batches = 0usize;
    let mut next_report = Instant::now();
    loop {
        let rows = events::read_rows(source, cursor, options.batch_events)?;
        if rows.is_empty() {
            break;
        }
        // Decode first, outside the transaction: a row that cannot be imported fails before anything of the batch is written.
        let mut decoded = Vec::with_capacity(rows.len());
        for row in &rows {
            let (event, retained) = events::decode(row).map_err(|detail| EventError::Row {
                sequence: row.sequence,
                event_type: row.event_type.clone(),
                version: row.version,
                aggregate: row.aggregate_kind.clone(),
                stream: row.stream_id.clone(),
                detail,
            })?;
            for key in retained {
                *report.retained_fields.entry(key).or_default() += 1;
            }
            decoded.push((row.sequence, event));
        }
        let last = rows.last().map_or(cursor, |row| row.sequence);
        let reference = options.reference_reducer;
        let count = decoded.len() as u64;
        store.transaction(|transaction| {
            let mut projector = Projector::default();
            for (sequence, event) in &decoded {
                insert_event(transaction, *sequence, event)?;
                let stored = StoredEvent { sequence: *sequence as u64, event: event.clone() };
                if reference {
                    events::apply_reference(transaction, &stored)?;
                } else {
                    projector.apply(transaction, &stored)?;
                }
            }
            projector.flush(transaction)?;
            progress_set(transaction, "events_cursor", &last.to_string())
        })?;
        cursor = last;
        done += count;
        batches += 1;
        if next_report.elapsed().as_secs() >= 2 || done == total {
            progress(format!("events: {done}/{total} ({:.0}%), {:.1}s", 100.0 * done as f64 / total.max(1) as f64, started.elapsed().as_secs_f64()));
            next_report = Instant::now();
        }
        if options.stop_after.is_some_and(|stop| stop == format!("events:{batches}")) {
            return Err(ImportError::Stopped(format!("events:{batches}")));
        }
    }
    // Rows the log holds that were not carried because of their version.
    if done != total {
        return Err(ImportError::Other(format!("{done} of {total} events were stored; the import was not completed")));
    }
    Ok(())
}

fn project_phase(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let existing: HashSet<String> = store.read(|connection| {
        let mut statement = connection.prepare("SELECT DISTINCT aggregate_id FROM rust_application_events WHERE aggregate_kind = 'project'")?;
        Ok(statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?)
    })?;
    // V1 projects: baseline events, exactly as migration 55 wrote them for the projects that existed.
    let baseline = events::v1_project_events(source, &existing)?;
    report.legacy_projects_imported = baseline.iter().filter(|event| event.event_type == "project.created").count() as u64;
    let next = next_sequence(source, store)?;
    store.transaction(|transaction| {
        let mut sequence = next;
        for event in &baseline {
            insert_event(transaction, sequence, event)?;
            events_apply_project(transaction, sequence, event)?;
            sequence += 1;
        }
        Ok(())
    })?;
    // The columns the original reads from its project table that no event of a V1-origin project carries.
    let (patches, checked) = store.read(|connection| events::project_reconciliation(source, connection, &now).map_err(|error| StoreError::InvalidCommand(error.to_string())))?;
    report.projects_reconciled = patches.len() as u64;
    let next = next_sequence(source, store)?;
    store.transaction(|transaction| {
        let mut sequence = next;
        for event in &patches {
            insert_event(transaction, sequence, event)?;
            events_apply_project(transaction, sequence, event)?;
            sequence += 1;
        }
        Ok(())
    })?;
    let _ = checked;
    Ok(())
}

fn events_apply_project(transaction: &Transaction<'_>, sequence: i64, event: &crate::persistence::Event) -> Result<(), StoreError> {
    crate::project::reduce_project_event(transaction, &StoredEvent { sequence: sequence as u64, event: event.clone() })
}

/// Sequences continue after everything the source ever numbered, V1 events included, and after what is already stored.
fn next_sequence(source: &Source, store: &Store) -> Result<i64, ImportError> {
    let source_max: i64 = if table_exists(&source.connection, "orchestration_events")? {
        source.connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM orchestration_events", [], |row| row.get(0))?
    } else {
        0
    };
    let stored: i64 = store.read(|connection| Ok(connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM rust_application_events", [], |row| row.get(0))?))?;
    Ok(source_max.max(stored) + 1)
}

fn legacy_threads(options: &Options<'_>, source: &Source, store: &Store, report: &mut Report, progress: Progress<'_>) -> Result<(), ImportError> {
    let have: HashSet<String> = store.read(|connection| {
        let mut statement = connection.prepare("SELECT DISTINCT aggregate_id FROM rust_application_events WHERE aggregate_kind = 'thread' AND event_type = 'thread.created'")?;
        Ok(statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?)
    })?;
    let legacy = events::v1_thread_ids(source, &have)?;
    let total = legacy.threads.len();
    for (index, (id, _)) in legacy.threads.iter().enumerate() {
        let mut thread_events = events::v1_thread_events(source, id)?;
        for event in &mut thread_events {
            *event = canonical_thread_event(event.clone()).map_err(|detail| EventError::Other(format!("legacy thread {id}: {detail}")))?;
        }
        let first = next_sequence(source, store)?;
        store.transaction(|transaction| {
            let mut projector = Projector::default();
            for (offset, event) in thread_events.iter().enumerate() {
                let sequence = first + offset as i64;
                insert_event(transaction, sequence, event)?;
                projector.apply(transaction, &StoredEvent { sequence: sequence as u64, event: event.clone() })?;
            }
            projector.flush(transaction)
        })?;
        report.legacy_threads_imported += 1;
        if (index + 1) % 50 == 0 || index + 1 == total {
            progress(format!("legacy V1 threads: {}/{total}", index + 1));
        }
        if options.stop_after.is_some_and(|stop| stop == format!("legacy-threads:{}", index + 1)) {
            return Err(ImportError::Stopped(format!("legacy-threads:{}", index + 1)));
        }
    }
    Ok(())
}

/// Receipts keep the command ids the original answered, so a client that retries one is told what happened the first time.
fn receipts(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    let connection = &source.connection;
    if !table_exists(connection, "orchestration_command_receipts")? {
        return Ok(());
    }
    let available = columns(connection, "orchestration_command_receipts")?;
    let list = select_list(&available, &["command_id", "aggregate_kind", "aggregate_id", "command_type", "accepted_at", "result_sequence", "status", "error"]);
    let mut statement = connection.prepare(&format!("SELECT {list} FROM orchestration_command_receipts ORDER BY result_sequence, command_id"))?;
    let mut rows = statement.query([])?;
    let mut imported = 0u64;
    store.transaction(|transaction| {
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let id: String = row.get(0).map_err(StoreError::from)?;
            let error: Option<String> = row.get(7).map_err(StoreError::from)?;
            let accepted: String = row.get(4).map_err(StoreError::from)?;
            let receipt = json!({
                "commandId": id,
                "aggregateKind": row.get::<_, String>(1).map_err(StoreError::from)?,
                "aggregateId": row.get::<_, String>(2).map_err(StoreError::from)?,
                "commandType": row.get::<_, Option<String>>(3).map_err(StoreError::from)?.unwrap_or_else(|| "legacy".into()),
                "acceptedAt": events::iso(&accepted).unwrap_or(accepted),
                "resultSequence": row.get::<_, i64>(5).map_err(StoreError::from)?,
                "status": row.get::<_, String>(6).map_err(StoreError::from)?,
                "error": error.map(|text| serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))),
            });
            transaction.execute("INSERT OR REPLACE INTO rust_command_receipts(command_id,receipt_json) VALUES(?1,?2)", params![id, serde_json::to_string(&receipt)?])?;
            imported += 1;
        }
        Ok(())
    })?;
    if let Some(table) = report.tables.get_mut("orchestration_command_receipts") {
        table.imported = imported;
    }
    Ok(())
}

const TASK_DDL: &str = "
CREATE TABLE IF NOT EXISTS rust_scheduled_tasks (
  task_id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, prompt TEXT NOT NULL, enabled INTEGER NOT NULL,
  schedule_json TEXT NOT NULL, project_id TEXT NOT NULL, thread_id TEXT, workspace_strategy_json TEXT NOT NULL,
  model_selection_json TEXT NOT NULL, runtime_mode TEXT NOT NULL, interaction_mode TEXT NOT NULL,
  created_by TEXT NOT NULL, creation_source TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  next_run_at TEXT, last_run_at TEXT, last_run_status TEXT NOT NULL, last_run_error TEXT, run_count INTEGER NOT NULL,
  webhook_token TEXT, webhook_secret TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_tasks_due ON rust_scheduled_tasks(enabled, next_run_at) WHERE enabled = 1 AND next_run_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS rust_scheduled_tasks_project ON rust_scheduled_tasks(project_id, updated_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_runs (
  run_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, trigger TEXT NOT NULL, started_at TEXT NOT NULL,
  completed_at TEXT, status TEXT NOT NULL, command_id TEXT NOT NULL, message_id TEXT NOT NULL,
  thread_id TEXT, delivery_id TEXT, error TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_runs_task ON rust_scheduled_task_runs(task_id, started_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_webhook_deliveries (
  delivery_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, received_at TEXT NOT NULL, method TEXT NOT NULL,
  query TEXT NOT NULL, headers_json TEXT NOT NULL, body TEXT NOT NULL, body_bytes INTEGER NOT NULL,
  body_truncated INTEGER NOT NULL, outcome TEXT NOT NULL, signature_verified INTEGER NOT NULL,
  missing_fields_json TEXT NOT NULL, rendered_prompt TEXT, error TEXT);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_webhook_deliveries_task ON rust_scheduled_task_webhook_deliveries(task_id, received_at);
CREATE TABLE IF NOT EXISTS rust_scheduled_task_webhook_relay_deliveries (
  relay_delivery_id TEXT PRIMARY KEY NOT NULL, task_id TEXT NOT NULL, seen_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS rust_scheduled_task_webhook_relay_deliveries_seen ON rust_scheduled_task_webhook_relay_deliveries(seen_at);";

/// Scheduled tasks are the same rows under another table name. A task that was mid-run stays `running`: the scheduler's own
/// startup recovery settles it (a recorded launch is a succeeded run, otherwise failed) and advances it, as after a crash; the
/// import never starts one.
fn scheduled_tasks(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    store.transaction(|transaction| Ok(transaction.execute_batch(TASK_DDL)?))?;
    for (from, to, wanted) in [
        ("scheduled_tasks", "rust_scheduled_tasks", vec!["task_id", "title", "prompt", "enabled", "schedule_json", "project_id", "thread_id", "workspace_strategy_json", "model_selection_json", "runtime_mode", "interaction_mode", "created_by", "creation_source", "created_at", "updated_at", "next_run_at", "last_run_at", "last_run_status", "last_run_error", "run_count", "webhook_token", "webhook_secret"]),
        ("scheduled_task_webhook_deliveries", "rust_scheduled_task_webhook_deliveries", vec!["delivery_id", "task_id", "received_at", "method", "query", "headers_json", "body", "body_bytes", "body_truncated", "outcome", "signature_verified", "missing_fields_json", "rendered_prompt", "error"]),
        ("scheduled_task_webhook_relay_deliveries", "rust_scheduled_task_webhook_relay_deliveries", vec!["relay_delivery_id", "task_id", "seen_at"]),
    ] {
        if !table_exists(&source.connection, from)? {
            continue;
        }
        let available = columns(&source.connection, from)?;
        let list = select_list(&available, &wanted);
        let mut statement = source.connection.prepare(&format!("SELECT {list} FROM {from}"))?;
        let mut rows = statement.query([])?;
        let marks = (1..=wanted.len()).map(|index| format!("?{index}")).collect::<Vec<_>>().join(",");
        let insert = format!("INSERT OR REPLACE INTO {to}({}) VALUES({marks})", wanted.join(","));
        let mut imported = 0u64;
        store.transaction(|transaction| {
            let mut write = transaction.prepare(&insert)?;
            while let Some(row) = rows.next().map_err(StoreError::from)? {
                let values: Vec<rusqlite::types::Value> = (0..wanted.len()).map(|index| row.get(index)).collect::<Result<_, _>>().map_err(StoreError::from)?;
                write.execute(rusqlite::params_from_iter(values))?;
                imported += 1;
            }
            Ok(())
        })?;
        if let Some(table) = report.tables.get_mut(from) {
            table.imported = imported;
        }
    }
    Ok(())
}

/// Provider names the settings migration reads to switch back on the providers a user has used, the viewed-file marks and the
/// MCP app context, which this server keeps as projections.
fn history(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    let connection = &source.connection;
    store.transaction(|transaction| {
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS projection_thread_sessions(provider_name TEXT, provider_instance_id TEXT); CREATE TABLE IF NOT EXISTS provider_session_runtime(provider_name TEXT, provider_instance_id TEXT);")?;
        Ok(())
    })?;
    for table in ["projection_thread_sessions", "provider_session_runtime"] {
        if !table_exists(connection, table)? {
            continue;
        }
        let available = columns(connection, table)?;
        let list = select_list(&available, &["provider_name", "provider_instance_id"]);
        let mut statement = connection.prepare(&format!("SELECT DISTINCT {list} FROM {table} WHERE provider_name IS NOT NULL"))?;
        let rows: Vec<(String, Option<String>)> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
        store.transaction(|transaction| {
            for (name, instance) in &rows {
                transaction.execute(&format!("INSERT INTO {table}(provider_name, provider_instance_id) VALUES(?1,?2)"), params![name, instance])?;
            }
            Ok(())
        })?;
        if let Some(entry) = report.tables.get_mut(table) {
            entry.imported = rows.len() as u64;
        }
    }
    if table_exists(connection, "pull_request_files_viewed")? {
        let mut statement = connection.prepare("SELECT provider,host,repository,number,viewer,path,revision,viewed_at FROM pull_request_files_viewed ORDER BY provider,host,repository,number,viewer,path")?;
        let rows: Vec<(String, String, String, i64, String, String, Option<String>, String)> =
            statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)))?.collect::<Result<_, _>>()?;
        store.transaction(|transaction| {
            for (provider, host, repository, number, viewer, path, revision, viewed_at) in &rows {
                let scope = json!([provider, host, repository, number, viewer]).to_string();
                let mut state = crate::persistence::read_projection(transaction, "pullRequestFilesViewed", &scope)?.unwrap_or_else(|| json!({"marks": {}}));
                state["marks"][path] = json!({"revision":revision,"viewedAt":events::iso(viewed_at).unwrap_or_else(|_| viewed_at.clone())});
                crate::persistence::write_projection(transaction, "pullRequestFilesViewed", &scope, &state)?;
            }
            Ok(())
        })?;
        if let Some(entry) = report.tables.get_mut("pull_request_files_viewed") {
            entry.imported = rows.len() as u64;
        }
    }
    if table_exists(connection, "mcp_app_model_context")? {
        let mut statement = connection.prepare("SELECT thread_id,item_id,server,tool,text,updated_at FROM mcp_app_model_context ORDER BY thread_id,item_id")?;
        let rows: Vec<(String, String, String, String, String, String)> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?.collect::<Result<_, _>>()?;
        store.transaction(|transaction| {
            for (thread, item, server, tool, body, updated) in &rows {
                let mut state = crate::persistence::read_projection(transaction, "mcp_app_context", thread)?.unwrap_or_else(|| json!({"entries": {}}));
                state["entries"][item] = json!({"itemId":item,"server":server,"tool":tool,"text":body,"updatedAt":events::iso(updated).unwrap_or_else(|_| updated.clone())});
                crate::persistence::write_projection(transaction, "mcp_app_context", thread, &state)?;
            }
            Ok(())
        })?;
        if let Some(entry) = report.tables.get_mut("mcp_app_model_context") {
            entry.imported = rows.len() as u64;
        }
    }
    Ok(())
}

/// What the source holds that is not carried, counted so the report says what was left, never re-run.
fn leftovers(source: &Source, report: &mut Report) -> Result<(), ImportError> {
    let connection = &source.connection;
    if table_exists(connection, "orchestration_v2_effect_outbox")? {
        let mut statement = connection.prepare("SELECT status, effect_type, COUNT(*) FROM orchestration_v2_effect_outbox GROUP BY status, effect_type ORDER BY status, effect_type")?;
        let rows: Vec<(String, String, u64)> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect::<Result<_, _>>()?;
        let open: u64 = rows.iter().filter(|row| row.0 == "pending" || row.0 == "running").map(|row| row.2).sum();
        if open > 0 {
            let kinds: Vec<String> = rows.iter().filter(|row| row.0 == "pending" || row.0 == "running").map(|row| format!("{} {} {}", row.2, row.0, row.1)).collect();
            report.warnings.push(format!("{open} effects were pending or running in the original and were not carried, so nothing is run again: {}", kinds.join(", ")));
        }
    }
    if table_exists(connection, "orchestration_v2_thread_launch_workflows")? {
        let open: u64 = connection.query_row("SELECT COUNT(*) FROM orchestration_v2_thread_launch_workflows WHERE status NOT IN ('completed','failed','cancelled')", [], |row| row.get(0)).unwrap_or(0);
        if open > 0 {
            report.warnings.push(format!("{open} thread launches were still in progress in the original and are not resumed"));
        }
    }
    let sessions = count(connection, "auth_sessions")?;
    if sessions > 0 {
        report.credentials_to_reenter.push(format!("{sessions} signed-in browser or device sessions: pair each device again (sessions are signed with a key this server does not have)"));
    }
    Ok(())
}

fn compare_projections(source: &Source, store: &Store, report: &mut Report) -> Result<(), ImportError> {
    let connection = &source.connection;
    for (table, field) in PROJECTION_COMPARISONS {
        if !table_exists(connection, table)? {
            continue;
        }
        let read = count(connection, table)?;
        let have: u64 = store.read(|connection| {
            Ok(if field == "thread" {
                connection.query_row("SELECT COUNT(*) FROM rust_projections WHERE aggregate_kind = 'thread'", [], |row| row.get(0))?
            } else {
                connection.query_row(&format!("SELECT COALESCE(SUM(json_array_length(state_json, '$.{field}')),0) FROM rust_projections WHERE aggregate_kind = 'thread'"), [], |row| row.get(0))?
            })
        })?;
        report.projection_checks.insert(field.to_owned(), [read, have]);
        if let Some(entry) = report.tables.get_mut(table) {
            entry.imported = have;
        }
    }
    for (field, [read, have]) in report.projection_checks.clone() {
        if read != have {
            report.warnings.push(format!("the original's {field} projection has {read} rows; the rebuilt projections hold {have}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// The command.
// ---------------------------------------------------------------------------------------------------------------------

pub fn print_report(report: &Report) {
    eprintln!("Imported a {:?} database (schema migration {}, {} bytes) in {:.1}s; the store is {} bytes.", report.source_kind, report.source_migration, report.source_bytes, report.seconds, report.destination_bytes);
    let stored: u64 = report.events.values().map(|row| row[1]).sum();
    eprintln!("  events replayed: {stored}; V1 threads imported: {}; V1 projects imported: {}; project fields reconciled: {}", report.legacy_threads_imported, report.legacy_projects_imported, report.projects_reconciled);
    for (name, table) in &report.tables {
        if table.read > 0 {
            eprintln!("  {name}: {} rows read, {} now in the store -- {}", table.read, table.imported, table.disposition);
        }
    }
    for file in &report.files {
        eprintln!("  files: {} {} ({})", file.action, file.name, file.count);
    }
    for credential in &report.credentials_to_reenter {
        eprintln!("  to do: {credential}");
    }
    for warning in &report.warnings {
        eprintln!("  warning: {warning}");
    }
}

/// `t3-server import --from HOME --state-dir DIR`: the exit code. Explains every refusal in one line.
pub fn run_command(from: PathBuf, state_dir: PathBuf) -> i32 {
    let mut progress = |line: String| eprintln!("{line}");
    match import(Options::new(from, state_dir), &mut progress) {
        Ok(Outcome::Imported(report)) => {
            print_report(&report);
            0
        }
        Ok(Outcome::AlreadyImported) => {
            eprintln!("This state directory already holds an import (see rust_import_record in {DESTINATION}); nothing was imported again.");
            0
        }
        Ok(Outcome::NothingToImport) => {
            eprintln!("There is no T3 Code database to import there; nothing was imported.");
            0
        }
        Err(error) => {
            eprintln!("Import failed, and nothing was changed that the server will start on: {error}");
            1
        }
    }
}

/// The import `serve` runs on first start in a state directory that holds the original's database and no store of ours yet.
pub fn automatic(state_dir: &Path) -> Result<Option<Report>, ImportError> {
    if !due(state_dir) {
        return Ok(None);
    }
    let mut progress = |line: String| tracing::info!("{line}");
    match import(Options::new(state_dir.to_path_buf(), state_dir.to_path_buf()), &mut progress)? {
        Outcome::Imported(report) => Ok(Some(*report)),
        _ => Ok(None),
    }
}
