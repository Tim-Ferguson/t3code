//! Importing the original server's state directory (`state_import*.rs`), against state the unchanged original wrote
//! (`rust/tools/generate_import_fixtures.mjs`) and what the original then served for it (`reference.json.gz`).
//!
//! Every case imports into a temporary directory, starts the real server on the imported store and asks it over its WebSocket.
//! Replies are validated against the original's schemas as in every other flow (the `outbound-check` feature).
mod rpc_support;
use chrono::Utc;
use flate2::read::GzDecoder;
use futures_util::SinkExt;
use rpc_support::*;
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    config::NativeConfig,
    persistence::Store,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    state_import::{Options, Outcome, import},
    transport::{ApiState, router},
};
use tokio_tungstenite::tungstenite::Message;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-import")
}
fn gunzip(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_end(&mut bytes).unwrap();
    bytes
}
/// A copy of fixture `name` as a T3 home: `<home>/userdata` with the database unpacked.
fn home(name: &str, base: &Path) -> PathBuf {
    let home = base.join(format!("home-{name}"));
    let userdata = home.join("userdata");
    copy_tree(&fixtures().join(name).join("userdata"), &userdata);
    for file in std::fs::read_dir(&userdata).unwrap().flatten().map(|entry| entry.path()).collect::<Vec<_>>() {
        if file.extension().is_some_and(|extension| extension == "gz") {
            std::fs::write(file.with_extension(""), gunzip(&file)).unwrap();
            std::fs::remove_file(&file).unwrap();
        }
    }
    home
}
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
fn reference(name: &str) -> Value {
    serde_json::from_slice(&gunzip(&fixtures().join(name).join("reference.json.gz"))).unwrap()
}
/// Digest of every file under `root`: path, size and bytes. Equal before and after means untouched.
fn digest(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut rows = Vec::new();
    fn walk(root: &Path, at: &Path, rows: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(at).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), rows);
            } else {
                rows.push((entry.path().strip_prefix(root).unwrap().display().to_string(), std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    walk(root, root, &mut rows);
    rows.sort();
    rows
}

fn run(from: &Path, state: &Path) -> Result<Outcome, t3_server::state_import::ImportError> {
    import(Options::new(from.to_path_buf(), state.to_path_buf()), &mut |_| {})
}
fn imported(from: &Path, state: &Path) -> Box<t3_server::state_import::Report> {
    match run(from, state).unwrap_or_else(|error| panic!("{error}")) {
        Outcome::Imported(report) => report,
        other => panic!("expected an import, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// A real server on an imported store.
// ---------------------------------------------------------------------------------------------------------------------

struct Served {
    address: std::net::SocketAddr,
    token: String,
    store: Store,
    settings: SettingsService,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}
async fn serve(state_dir: &Path) -> Served {
    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_import".into(), "loopback-browser".into()).unwrap();
    let token = auth
        .issue_session(
            "fixture",
            "bearer-access-token",
            vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite],
            json!({"deviceType":"unknown"}),
            Utc::now(),
            chrono::Duration::hours(1),
        )
        .unwrap()
        .1;
    let environment = json!({"environmentId":"import-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    // The settings service starts as the server starts it: with the store as the history its load-time migrations read.
    let mut options = SettingsOptions::file(state_dir.join("settings.json"), ServerSecretStore::open(state_dir.join("secrets")).unwrap());
    options.watch = false;
    options.history = Some(store.clone());
    let settings = SettingsService::start(options).await.unwrap();
    let native = NativeConfig::from_settings(settings.snapshot().await.unwrap(), state_dir, state_dir, &environment, &auth.descriptor()).await.unwrap();
    let state = ApiState {
        store: store.clone(),
        auth,
        environment,
        config: Some(native.snapshot.clone()),
        settings: Some(settings.clone()),
        cors_origins: None,
        assets: None,
        providers: Some(native.providers.clone()),
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    Served { address, token, store, settings, stop: Some(stop), server: Some(server) }
}
impl Served {
    async fn connect(&self) -> Socket {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {}", self.token).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    async fn finish(mut self) {
        self.settings.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }
}

/// The first snapshot of `orchestration.subscribeShell`, read as a client does: chunks acknowledged until it says it is in sync.
async fn shell_snapshot(socket: &mut Socket) -> Value {
    socket
        .send(Message::Text(json!({"_tag":"Request","id":900,"tag":"orchestration.subscribeShell","payload":{"requestCompletionMarker":true},"headers":[]}).to_string().into()))
        .await
        .unwrap();
    let mut snapshot = Value::Null;
    loop {
        let message = next(socket).await;
        if message["_tag"] == "Chunk" {
            socket.send(Message::Text(json!({"_tag":"Ack","requestId":900}).to_string().into())).await.unwrap();
            for value in message["values"].as_array().unwrap() {
                if value["kind"] == "snapshot" {
                    snapshot = value["snapshot"].clone();
                }
                if value["kind"] == "synchronized" {
                    return snapshot;
                }
            }
        }
        assert_ne!(message["_tag"], "Exit", "the shell stream ended: {message}");
    }
}

/// Differences between two JSON values as `path: left != right`, for the paths `ignore` does not name.
fn differences(left: &Value, right: &Value, at: &str, ignore: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
    if ignore(at) {
        return;
    }
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                differences(a.get(key).unwrap_or(&Value::Null), b.get(key).unwrap_or(&Value::Null), &format!("{at}.{key}"), ignore, out);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{at}: {} items != {} items", a.len(), b.len()));
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differences(x, y, &format!("{at}[{index}]"), ignore, out);
            }
        }
        _ if left == right => {}
        _ => out.push(format!("{at}: {} != {}", short(left), short(right))),
    }
}
fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 90 { format!("{}...", text.chars().take(90).collect::<String>()) } else { text }
}

fn by_id(rows: &Value) -> std::collections::BTreeMap<String, Value> {
    rows.as_array().map(|rows| rows.iter().map(|row| (row["id"].as_str().unwrap_or_default().to_owned(), row.clone())).collect()).unwrap_or_default()
}

/// Paths where this server answers differently from the original for reasons that are not the import: shell summary fields its
/// shell derivation does not compute (`thread::shell` leaves them out or empty; they are read from the same projections the
/// import fills, and the thread projection below is compared whole), and values computed when asked.
fn computed(path: &str) -> bool {
    // A V1 thread's `updatedAt` is the time V1 recorded. The original, importing a transcript lazily, appends the older messages
    // after the thread's shell event, so its thread ends up stamped with the time of the last message it hydrated; the import writes
    // the whole transcript first and the shell last.
    if LEGACY.with(|legacy| legacy.get()) && (path.ends_with("].updatedAt") || path.ends_with(".thread.updatedAt")) {
        return true;
    }
    const SHELL_GAPS: [&str; 9] = [
        ".latestRunRequestedAt", ".latestRunStartedAt", ".latestRunCompletedAt", ".latestUserAuthoredMessageAt", ".latestVisibleMessage", ".providerInstanceHistory",
        ".pullRequests", ".itemCount", ".visibleItemCount",
    ];
    path.contains(".repositoryIdentity")
        || path.ends_with(".snapshotSequence")
        || path.contains("resolvedRepositoryIdentityRoots")
        || (path.starts_with("shell.threads") && SHELL_GAPS.iter().any(|gap| path.ends_with(gap)))
}

/// The lists of a served projection that the comparison with the original's served rows is made on, as a subset: every row the
/// original served must be served here, equal. This server also serves rows the original's read leaves out (the runs a rollback
/// undid and what hangs off them, a wake's notification message), and leaves out provider threads of child threads that the
/// original joins in; neither is a matter of what was imported (`imported_projections_equal_the_originals_rows` compares the
/// imported rows with the original's own).
fn served_subset(have: &Value, want: &Value, thread: &str, problems: &mut Vec<String>) {
    for (field, wanted) in want.as_object().unwrap() {
        if matches!(field.as_str(), "providerThreads" | "providerSessions" | "visibleTurnItems" | "updatedAt") {
            continue;
        }
        match (wanted.as_array(), have[field].as_array()) {
            (Some(wanted), Some(have)) => {
                let by = |rows: &[Value]| rows.iter().map(|row| (row["id"].as_str().map_or_else(|| row.to_string(), str::to_owned), row.clone())).collect::<std::collections::BTreeMap<_, _>>();
                let have = by(have);
                for (id, row) in by(wanted) {
                    match have.get(&id) {
                        Some(found) => differences(found, &row, &format!("thread[{thread}].{field}[{id}]"), &computed, problems),
                        None => problems.push(format!("thread[{thread}].{field}[{id}]: not served")),
                    }
                }
            }
            _ => differences(&have[field], wanted, &format!("thread[{thread}].{field}"), &computed, problems),
        }
    }
}

/// A thread projection with every list ordered by the identity of its rows: the original lists rows in the order its queries
/// give them and this server in the order events wrote them; the rows themselves must be equal.
fn ordered(mut projection: Value) -> Value {
    if let Some(object) = projection.as_object_mut() {
        for (key, rows) in object.iter_mut() {
            if let Some(rows) = rows.as_array_mut() {
                let identity = |row: &Value| row["id"].as_str().or_else(|| row["item"]["id"].as_str()).map(str::to_owned).unwrap_or_else(|| row.to_string());
                rows.sort_by_key(|row| identity(row));
                if key == "visibleTurnItems" {
                    for row in rows.iter_mut() {
                        row["position"] = Value::Null;
                    }
                }
            }
        }
    }
    projection
}

thread_local! { static LEGACY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
async fn compare_with_reference(served: &Served, name: &str) {
    LEGACY.with(|legacy| legacy.set(name == "v1" || name == "oldest"));
    // A server task that died would leave this waiting forever; a timeout turns that into a failure.
    tokio::time::timeout(std::time::Duration::from_secs(120), compare_inner(served, name)).await.unwrap_or_else(|_| panic!("{name}: the server did not answer within 120s"));
}
async fn compare_inner(served: &Served, name: &str) {
    let expected = reference(name);
    let mut socket = served.connect().await;
    let mut problems = Vec::new();
    // The shell: threads and projects, row for row.
    let shell = shell_snapshot(&mut socket).await;
    let wanted = &expected["shell"]["snapshot"];
    for kind in ["threads", "projects"] {
        let (have, want) = (by_id(&shell[kind]), by_id(&wanted[kind]));
        assert_eq!(have.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>(), "{name}: the shell's {kind}");
        for (id, row) in &want {
            differences(&have[id], row, &format!("shell.{kind}[{id}]"), &computed, &mut problems);
        }
    }
    let archived = success(&rpc(&mut socket, 1, "orchestration.getArchivedShellSnapshot", json!({})).await).clone();
    assert_eq!(by_id(&archived["threads"]).keys().collect::<Vec<_>>(), by_id(&expected["archived"]["threads"]).keys().collect::<Vec<_>>(), "{name}: archived threads");
    // Each thread's projection.
    let mut compared = 0;
    for (id, want) in expected["threads"].as_object().unwrap() {
        let exit = rpc(&mut socket, 100 + compared, "orchestration.getThreadProjection", json!({"threadId":id})).await;
        let have = success(&exit);
        served_subset(have, want, id, &mut problems);
        compared += 1;
    }
    // Settings, keybindings, scheduled tasks.
    let settings = success(&rpc(&mut socket, 2, "server.getSettings", json!({})).await).clone();
    differences(&settings, &expected["settings"], "settings", &computed, &mut problems);
    let config = success(&rpc(&mut socket, 3, "server.getConfig", json!({})).await).clone();
    differences(&config["keybindings"], &expected["config"]["keybindings"], "config.keybindings", &computed, &mut problems);
    let tasks = success(&rpc(&mut socket, 4, "scheduledTasks.list", json!({})).await).clone();
    differences(&tasks, &expected["scheduledTasks"], "scheduledTasks", &computed, &mut problems);
    assert!(compared > 0 || name == "v1" || name == "oldest", "{name}: no threads compared");
    assert!(problems.is_empty(), "{name}: {} differences from what the original served:\n{}", problems.len(), summary(&problems));
}

/// The differences grouped by the shape of their path (ids and positions removed), with a count and one example each.
fn summary(problems: &[String]) -> String {
    let mut groups: std::collections::BTreeMap<String, (usize, String)> = Default::default();
    for problem in problems {
        let (path, rest) = problem.split_once(": ").unwrap_or((problem, ""));
        let mut shape = String::new();
        let mut depth = 0;
        for character in path.chars() {
            match character {
                '[' => { depth += 1; shape.push('['); }
                ']' => { depth -= 1; shape.push(']'); }
                _ if depth == 0 => shape.push(character),
                _ => {}
            }
        }
        let entry = groups.entry(shape).or_insert((0, rest.to_owned()));
        entry.0 += 1;
    }
    groups.iter().map(|(shape, (count, example))| format!("{count:>5}  {shape}: {example}")).collect::<Vec<_>>().join("\n")
}

fn sqlite_count(state: &Path, sql: &str) -> i64 {
    rusqlite::Connection::open_with_flags(state.join("rust-state.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap().query_row(sql, [], |row| row.get(0)).unwrap()
}

// ---------------------------------------------------------------------------------------------------------------------
// Cases.
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_home_is_served_as_the_original_served_it() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let before = digest(&from);
    let state = base.path().join("state");
    let report = imported(&from, &state);
    assert_eq!(digest(&from), before, "the original's files are untouched");
    assert!(!state.join("rust-state.sqlite.importing").exists());
    let events: u64 = report.events.values().map(|row| row[1]).sum();
    let in_log: i64 = rusqlite::Connection::open_with_flags(from.join("userdata/statev2.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap().query_row("SELECT COUNT(*) FROM orchestration_events WHERE application_event_version = 2", [], |row| row.get(0)).unwrap();
    assert_eq!(events as i64, in_log, "every application event of the log was replayed");
    assert!(report.events.values().all(|row| row[0] == row[1]), "{:?}", report.events);
    assert_eq!(sqlite_count(&state, "SELECT COUNT(*) FROM rust_import_record"), 1);
    let served = serve(&state).await;
    compare_with_reference(&served, "full").await;
    served.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn homes_from_v1_and_from_the_oldest_schema_are_served_as_the_original_served_them() {
    for name in ["v1", "oldest"] {
        let base = tempfile::tempdir().unwrap();
        let from = home(name, base.path());
        let before = digest(&from);
        let state = base.path().join("state");
        let report = imported(&from, &state);
        assert_eq!(digest(&from), before, "{name}: the original's files are untouched");
        assert_eq!(report.legacy_threads_imported, 4, "{name}");
        assert_eq!(report.legacy_projects_imported, 2, "{name}");
        assert_eq!(report.source_kind, Some(t3_server::state_import::Kind::V1));
        let served = serve(&state).await;
        compare_with_reference(&served, name).await;
        served.finish().await;
    }
}

#[test]
fn an_empty_home_imports_nothing_and_leaves_no_trace() {
    let base = tempfile::tempdir().unwrap();
    let from = base.path().join("home");
    std::fs::create_dir_all(from.join("userdata")).unwrap();
    let state = base.path().join("state");
    assert!(matches!(run(&from, &state).unwrap(), Outcome::NothingToImport));
    assert!(!t3_server::state_import::due(&state));
    assert!(std::fs::read_dir(&state).unwrap().next().is_none());
    // A home that does not exist at all is the same.
    assert!(matches!(run(&base.path().join("nowhere"), &state).unwrap(), Outcome::NothingToImport));
}

#[test]
fn a_second_import_does_nothing_and_a_foreign_store_is_not_written_over() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let state = base.path().join("state");
    imported(&from, &state);
    let events = sqlite_count(&state, "SELECT COUNT(*) FROM rust_application_events");
    let record = sqlite_count(&state, "SELECT LENGTH(record_json) FROM rust_import_record");
    assert!(matches!(run(&from, &state).unwrap(), Outcome::AlreadyImported));
    assert!(!t3_server::state_import::due(&state), "serve would not import on its next start either");
    assert_eq!(sqlite_count(&state, "SELECT COUNT(*) FROM rust_application_events"), events);
    assert_eq!(sqlite_count(&state, "SELECT LENGTH(record_json) FROM rust_import_record"), record);
    // A store this server made itself, with no import behind it, is not imported over.
    let own = base.path().join("own");
    std::fs::create_dir_all(&own).unwrap();
    Store::open(own.join("rust-state.sqlite")).unwrap();
    let error = run(&from, &own).unwrap_err().to_string();
    assert!(error.contains("already holds state"), "{error}");
}

#[test]
fn the_applied_events_equal_the_events_run_through_the_servers_own_reducer() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let (fast, slow) = (base.path().join("fast"), base.path().join("slow"));
    imported(&from, &fast);
    let mut options = Options::new(from.clone(), slow.clone());
    options.reference_reducer = true;
    assert!(matches!(import(options, &mut |_| {}).unwrap(), Outcome::Imported(_)));
    let rows = |state: &Path| -> Vec<(String, String, String)> {
        let connection = rusqlite::Connection::open_with_flags(state.join("rust-state.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut statement = connection.prepare("SELECT aggregate_kind, aggregate_id, state_json FROM rust_projections ORDER BY 1, 2").unwrap();
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().map(Result::unwrap).collect()
    };
    let (fast_rows, slow_rows) = (rows(&fast), rows(&slow));
    assert!(fast_rows.len() > 20);
    assert_eq!(fast_rows, slow_rows);
}

fn mutate_source(from: &Path, sql: &str) {
    let connection = rusqlite::Connection::open(from.join("userdata/statev2.sqlite")).unwrap();
    connection.execute_batch(sql).unwrap();
}

#[test]
fn damaged_newer_and_undecodable_sources_are_refused_with_nothing_left_to_start_on() {
    let base = tempfile::tempdir().unwrap();
    // A file that is not a database, and a database cut short.
    for (name, damage) in [("garbage", 0usize), ("truncated", 1)] {
        let from = home("full", base.path());
        let path = from.join("userdata/statev2.sqlite");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, if damage == 0 { b"this is not a database at all".to_vec() } else { bytes[..bytes.len() / 3].to_vec() }).unwrap();
        let state = base.path().join(format!("state-{name}"));
        let error = run(&from, &state).unwrap_err().to_string();
        assert!(error.contains("damaged") || error.contains("not a T3 Code database") || error.contains("Reading the database failed") || error.contains("not a database"), "{name}: {error}");
        assert!(!state.join("rust-state.sqlite").exists() && !state.join("rust-state.sqlite.importing").exists(), "{name}");
        std::fs::remove_dir_all(&from).unwrap();
    }
    // Written by a newer original.
    let from = home("full", base.path());
    mutate_source(&from, "INSERT INTO effect_sql_migrations(migration_id, name) VALUES (60, 'SomethingNewer')");
    let state = base.path().join("state-newer");
    let error = run(&from, &state).unwrap_err().to_string();
    assert!(error.contains("newer T3 Code") && error.contains("60") && error.contains("SomethingNewer"), "{error}");
    assert!(!state.join("rust-state.sqlite").exists() && !state.join("rust-state.sqlite.importing").exists());
    // A row that does not decode names the event and its kind, never its content; the staging file does not outlive the failure.
    let from2 = home("full", base.path());
    mutate_source(&from2, "UPDATE orchestration_events SET payload_json = '{\"secretText\":\"do-not-print-this\"}' WHERE sequence = (SELECT MIN(sequence) FROM orchestration_events WHERE event_type = 'run.created')");
    let state = base.path().join("state-undecodable");
    let error = run(&from2, &state).unwrap_err().to_string();
    assert!(error.contains("run.created") && error.contains("application event version 2") && error.contains("cannot be imported"), "{error}");
    assert!(!error.contains("do-not-print-this"), "{error}");
    assert!(!state.join("rust-state.sqlite").exists() && !state.join("rust-state.sqlite.importing").exists());
    // An event type this server does not know is refused the same way.
    let from3 = home("full", base.path());
    mutate_source(&from3, "UPDATE orchestration_events SET event_type = 'thread.from-the-future' WHERE sequence = (SELECT MAX(sequence) FROM orchestration_events WHERE aggregate_kind = 'thread')");
    let error = run(&from3, &base.path().join("state-future")).unwrap_err().to_string();
    assert!(error.contains("thread.from-the-future"), "{error}");
}

#[test]
fn an_original_that_is_still_running_is_not_imported_from() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    // The parent process is alive and is not this one.
    std::fs::write(from.join("userdata/server-runtime.json"), json!({"pid": std::os::unix::process::parent_id()}).to_string()).unwrap();
    let error = run(&from, &base.path().join("state")).unwrap_err().to_string();
    assert!(error.contains("appears to be running"), "{error}");
    // A stale descriptor (no such process) is not a reason to refuse.
    std::fs::write(from.join("userdata/server-runtime.json"), json!({"pid": 2_000_000_000u64}).to_string()).unwrap();
    imported(&from, &base.path().join("state2"));
}

#[test]
fn a_write_ahead_log_is_read_from_a_private_copy_and_the_original_is_not_touched() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let path = from.join("userdata/statev2.sqlite");
    // Committed work that lives only in the log: a scheduled task written after the last checkpoint, connection left open.
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
    writer
        .execute_batch("INSERT INTO scheduled_tasks(task_id,title,prompt,enabled,schedule_json,project_id,workspace_strategy_json,model_selection_json,runtime_mode,interaction_mode,created_by,creation_source,created_at,updated_at,last_run_status,run_count) SELECT 'task-only-in-the-log','In the log','Prompt',0,schedule_json,project_id,workspace_strategy_json,model_selection_json,runtime_mode,interaction_mode,created_by,creation_source,created_at,updated_at,'never',0 FROM scheduled_tasks LIMIT 1")
        .unwrap();
    assert!(std::fs::metadata(path.with_extension("sqlite-wal")).unwrap().len() > 0);
    let before = digest(&from);
    let state = base.path().join("state");
    imported(&from, &state);
    assert_eq!(digest(&from), before, "neither the database nor its log was touched");
    assert_eq!(sqlite_count(&state, "SELECT COUNT(*) FROM rust_scheduled_tasks WHERE task_id = 'task-only-in-the-log'"), 1);
    drop(writer);
}

#[test]
fn an_import_interrupted_between_phases_or_mid_log_continues_or_starts_clean() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let whole = base.path().join("whole");
    imported(&from, &whole);
    let rows = |state: &Path| -> Vec<(String, String)> {
        let connection = rusqlite::Connection::open_with_flags(state.join("rust-state.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut statement = connection.prepare("SELECT aggregate_id, state_json FROM rust_projections UNION ALL SELECT command_id, receipt_json FROM rust_command_receipts UNION ALL SELECT task_id, title FROM rust_scheduled_tasks ORDER BY 1, 2").unwrap();
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    let expected = rows(&whole);
    // Stopped after batch 2 of the log, after the log, and after each later phase: each resumes to the same store.
    for stop in ["events:2", "events", "projects", "legacy-threads", "receipts", "scheduled-tasks", "history"] {
        let state = base.path().join(format!("state-{}", stop.replace(':', "-")));
        let mut options = Options::new(from.clone(), state.clone());
        options.stop_after = Some(stop);
        options.batch_events = 300;
        assert!(matches!(import(options, &mut |_| {}), Err(t3_server::state_import::ImportError::Stopped(_))), "{stop}");
        assert!(!state.join("rust-state.sqlite").exists(), "{stop}: nothing the server could start on");
        assert!(state.join("rust-state.sqlite.importing").exists(), "{stop}");
        let report = imported(&from, &state);
        assert!(report.resumed, "{stop}: the same source continues where it stopped");
        assert_eq!(rows(&state), expected, "{stop}");
        assert!(!state.join("rust-state.sqlite.importing").exists());
    }
    // The source changed while the import was stopped: the staging file is discarded and the import starts clean.
    let state = base.path().join("state-changed");
    let mut options = Options::new(from.clone(), state.clone());
    options.stop_after = Some("events:1");
    options.batch_events = 300;
    assert!(import(options, &mut |_| {}).is_err());
    mutate_source(&from, "UPDATE scheduled_tasks SET title = title || ' (edited)' WHERE task_id = (SELECT MIN(task_id) FROM scheduled_tasks)");
    let report = imported(&from, &state);
    assert!(!report.resumed);
    assert_eq!(sqlite_count(&state, "SELECT COUNT(*) FROM rust_scheduled_tasks WHERE title LIKE '% (edited)'"), 1);
}

#[test]
fn files_are_copied_to_another_directory_without_replacing_anything_and_secrets_stay_private() {
    use std::os::unix::fs::PermissionsExt;
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    // A secret the settings service reads by name, and another the importer must not carry.
    std::fs::write(from.join("userdata/secrets/provider-env-Y29kZXg-VE9LRU4.bin"), b"token-bytes").unwrap();
    let before = digest(&from);
    let state = base.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("keybindings.json"), "[]").unwrap();
    let report = imported(&from, &state);
    assert_eq!(digest(&from), before);
    assert_eq!(std::fs::read(state.join("keybindings.json")).unwrap(), b"[]", "a file the destination has is kept");
    assert_eq!(std::fs::read(state.join("settings.json")).unwrap(), std::fs::read(from.join("userdata/settings.json")).unwrap());
    assert_eq!(std::fs::read(state.join("attachments/fixture-image.png")).unwrap(), std::fs::read(from.join("userdata/attachments/fixture-image.png")).unwrap());
    let carried = state.join("secrets/provider-env-Y29kZXg-VE9LRU4.bin");
    assert_eq!(std::fs::read(&carried).unwrap(), b"token-bytes");
    assert_eq!(std::fs::metadata(&carried).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!state.join("secrets/server-signing-key.bin").exists(), "session signing keys are not carried");
    assert!(report.credentials_to_reenter.iter().any(|line| line.contains("T3 Connect")));
}

/// A source whose runs were mid-turn: nothing is launched again because of the import, and the server's own startup recovery
/// settles what was open, as after a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_in_flight_comes_across_for_ordinary_recovery_and_nothing_is_run_again() {
    let base = tempfile::tempdir().unwrap();
    let from = home("inflight", base.path());
    // The original had a pending effect and a queued launch when it stopped.
    mutate_source(
        &from,
        "INSERT INTO orchestration_v2_effect_outbox(effect_id,command_id,thread_id,effect_type,payload_json,status,attempt_count,available_at,created_at,updated_at) SELECT 'pending-effect','command-x',thread_id,'provider-turn.start','{}','pending',0,'2000-01-01T00:00:00.000Z','2000-01-01T00:00:00.000Z','2000-01-01T00:00:00.000Z' FROM orchestration_v2_projection_threads LIMIT 1",
    );
    let state = base.path().join("state");
    let report = imported(&from, &state);
    assert!(report.warnings.iter().any(|line| line.contains("pending") && line.contains("provider-turn.start")), "{:?}", report.warnings);
    assert_eq!(sqlite_count(&state, "SELECT COUNT(*) FROM rust_effect_outbox"), 0, "no effect is carried, so none can run");
    let running = sqlite_count(&state, "SELECT COUNT(*) FROM rust_projections, json_each(state_json, '$.runs') WHERE aggregate_kind = 'thread' AND json_extract(value, '$.status') = 'running'");
    assert!(running >= 2, "the open runs came across as they were: {running}");
    let events_before = sqlite_count(&state, "SELECT COUNT(*) FROM rust_application_events WHERE event_type = 'run.created'");
    // The shell is valid as imported.
    let served = serve(&state).await;
    {
        let mut socket = served.connect().await;
        let shell = tokio::time::timeout(std::time::Duration::from_secs(60), shell_snapshot(&mut socket)).await.unwrap();
        assert!(shell["threads"].as_array().unwrap().len() >= 3);
    }
    // Startup recovery, as `main` runs it, over the imported store.
    let lease = served.store.acquire_runtime_lease().unwrap();
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}})).unwrap();
    let providers = t3_server::provider_registry::ProviderRegistry::discover_with_catalog(&settings, base.path(), None).await.unwrap();
    let execution = t3_server::execution::ExecutionService::try_start_with_lease(served.store.clone(), providers, lease).unwrap();
    let open: i64 = served
        .store
        .read(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM rust_projections, json_each(state_json, '$.runs') WHERE aggregate_kind = 'thread' AND json_extract(value, '$.status') IN ('running','waiting','starting','preparing')", [], |row| row.get(0))?))
        .unwrap();
    let left: Vec<Value> = served.store.projections("thread").unwrap().iter().flat_map(|thread| thread["runs"].as_array().unwrap().iter().filter(|run| matches!(run["status"].as_str(), Some("running" | "waiting" | "starting" | "preparing"))).map(|run| json!([thread["thread"]["title"], run["status"], run["id"]])).collect::<Vec<_>>()).collect();
    assert_eq!(open, 0, "recovery settled every open run, left: {left:?}");
    let started: Vec<String> = served.store.events(0, None, None, None, 10_000).unwrap().iter().filter(|stored| stored.event.event_type == "run.created").map(|stored| format!("{} {} {}", stored.sequence, stored.event.payload["status"], stored.event.payload["restartContinuationOfRunId"])).collect();
    assert_eq!(started.len() as i64, events_before, "no run was started: {started:?}");
    let effects: Vec<String> = served.store.read(|connection| {
        let mut statement = connection.prepare("SELECT status || ' ' || json_extract(request_json, '$.type') FROM rust_effect_outbox")?;
        Ok(statement.query_map([], |row| row.get(0))?.map(Result::unwrap).collect())
    }).unwrap();
    // What the server's own recovery queues for the runs it settled is a checkpoint capture, as after a crash; never a provider turn.
    assert!(effects.iter().all(|effect| effect.ends_with("checkpoint.capture") || effect.starts_with("cancelled")), "effects after recovery: {effects:?}");
    assert!(!effects.iter().any(|effect| effect.contains("provider-turn") || effect.contains("provider-runtime")), "{effects:?}");
    execution.shutdown().await;
    served.finish().await;
}

/// The imported projections are the original's own: each list of each thread equals the rows the original's projection tables
/// hold for the thread (`payload_json`), row for row. The events replayed here are the same ones the original reduced.
#[test]
fn imported_projections_equal_the_originals_rows() {
    const TABLES: [(&str, &str); 13] = [
        ("orchestration_v2_projection_runs", "runs"),
        ("orchestration_v2_projection_run_attempts", "attempts"),
        ("orchestration_v2_projection_nodes", "nodes"),
        ("orchestration_v2_projection_subagents", "subagents"),
        ("orchestration_v2_projection_provider_threads", "providerThreads"),
        ("orchestration_v2_projection_provider_turns", "providerTurns"),
        ("orchestration_v2_projection_runtime_requests", "runtimeRequests"),
        ("orchestration_v2_projection_messages", "messages"),
        ("orchestration_v2_projection_plans", "plans"),
        ("orchestration_v2_projection_turn_items", "turnItems"),
        ("orchestration_v2_projection_checkpoint_scopes", "checkpointScopes"),
        ("orchestration_v2_projection_checkpoints", "checkpoints"),
        ("orchestration_v2_projection_context_handoffs", "contextHandoffs"),
    ];
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let state = base.path().join("state");
    imported(&from, &state);
    let original = rusqlite::Connection::open_with_flags(from.join("userdata/statev2.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let ours = rusqlite::Connection::open_with_flags(state.join("rust-state.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut projections = std::collections::BTreeMap::new();
    let mut statement = ours.prepare("SELECT aggregate_id, state_json FROM rust_projections WHERE aggregate_kind = 'thread'").unwrap();
    for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap() {
        let (id, json) = row.unwrap();
        projections.insert(id, serde_json::from_str::<Value>(&json).unwrap());
    }
    let mut problems = Vec::new();
    let mut compared = 0;
    // The thread record.
    let mut statement = original.prepare("SELECT thread_id, payload_json FROM orchestration_v2_projection_threads").unwrap();
    for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap() {
        let (id, json) = row.unwrap();
        differences(&projections[&id]["thread"], &serde_json::from_str::<Value>(&json).unwrap(), &format!("thread[{id}].thread"), &|_| false, &mut problems);
        compared += 1;
    }
    for (table, field) in TABLES {
        let mut statement = original.prepare(&format!("SELECT thread_id, payload_json FROM {table}")).unwrap();
        for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap() {
            let (thread, json) = row.unwrap();
            let want: Value = serde_json::from_str(&json).unwrap();
            let rows = projections[&thread][field].as_array().unwrap();
            match rows.iter().find(|row| row["id"] == want["id"]) {
                Some(found) => differences(found, &want, &format!("{field}[{}]", want["id"]), &|_| false, &mut problems),
                None => problems.push(format!("{field}[{}]: missing from thread {thread}", want["id"])),
            }
            compared += 1;
        }
    }
    assert!(compared > 300, "{compared}");
    assert!(problems.is_empty(), "{} differences from the original's projection rows ({compared} rows compared):\n{}", problems.len(), summary(&problems));
}

#[test]
fn serve_imports_in_place_on_first_start_only_and_leaves_the_originals_files_where_they_are() {
    let base = tempfile::tempdir().unwrap();
    let from = home("full", base.path());
    let userdata = from.join("userdata");
    let before = digest(&userdata);
    assert!(t3_server::state_import::due(&userdata));
    let report = t3_server::state_import::automatic(&userdata).unwrap().expect("the first start imports");
    assert!(report.files.iter().any(|file| file.action.contains("in place")), "{:?}", report.files);
    assert!(userdata.join("rust-state.sqlite").exists());
    assert!(!t3_server::state_import::due(&userdata));
    assert!(t3_server::state_import::automatic(&userdata).unwrap().is_none(), "the second start imports nothing");
    // Only the files this server adds are new: every file of the original is byte for byte what it was.
    let after = digest(&userdata);
    for (name, bytes) in &before {
        assert_eq!(after.iter().find(|(other, _)| other == name).map(|(_, bytes)| bytes), Some(bytes), "{name}");
    }
}
