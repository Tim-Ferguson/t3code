//! The original server's database as the importer sees it: located, checked, and opened without being able to change it.
//!
//! The original keeps `statev2.sqlite` (the V2 file) in its state directory and, when that file is missing, seeds it from a
//! V1 `state.sqlite` and runs its migrations over the copy. The importer does the same lookup, opens a file that has no
//! write-ahead log with `immutable=1` (no lock, no write, no `-shm`), and works on a private copy of one that has.
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// `Migrations.ts` `migrationEntries`: the schema versions this importer understands. A source recording a larger id was
/// written by a newer original and is refused.
pub const MIGRATIONS: [(i64, &str); 59] = [
    (1, "OrchestrationEvents"),
    (2, "OrchestrationCommandReceipts"),
    (3, "CheckpointDiffBlobs"),
    (4, "ProviderSessionRuntime"),
    (5, "Projections"),
    (6, "ProjectionThreadSessionRuntimeModeColumns"),
    (7, "ProjectionThreadMessageAttachments"),
    (8, "ProjectionThreadActivitySequence"),
    (9, "ProviderSessionRuntimeMode"),
    (10, "ProjectionThreadsRuntimeMode"),
    (11, "OrchestrationThreadCreatedRuntimeMode"),
    (12, "ProjectionThreadsInteractionMode"),
    (13, "ProjectionThreadProposedPlans"),
    (14, "ProjectionThreadProposedPlanImplementation"),
    (15, "ProjectionTurnsSourceProposedPlan"),
    (16, "CanonicalizeModelSelections"),
    (17, "ProjectionThreadsArchivedAt"),
    (18, "ProjectionThreadsArchivedAtIndex"),
    (19, "ProjectionSnapshotLookupIndexes"),
    (20, "AuthAccessManagement"),
    (21, "AuthSessionClientMetadata"),
    (22, "AuthSessionLastConnectedAt"),
    (23, "ProjectionThreadShellSummary"),
    (24, "BackfillProjectionThreadShellSummary"),
    (25, "CleanupInvalidProjectionPendingApprovals"),
    (26, "CanonicalizeModelSelectionOptions"),
    (27, "ProviderSessionRuntimeInstanceId"),
    (28, "ProjectionThreadSessionInstanceId"),
    (29, "ProjectionThreadDetailOrderingIndexes"),
    (30, "ProjectionThreadShellArchiveIndexes"),
    (31, "AuthAuthorizationScopes"),
    (32, "AuthPairingProofKeyThumbprint"),
    (33, "ProjectionThreadsSettled"),
    (34, "ProjectionThreadsSnoozed"),
    (35, "ProjectionThreadTitleRegeneration"),
    (36, "ProjectionThreadsPinned"),
    (37, "ProjectionTurnsKeysetIndex"),
    (38, "ProjectionThreadsPinOrderKey"),
    (39, "ProjectionProjectsDefaultThreadEnvMode"),
    (40, "ProjectionProjectFaviconPath"),
    (41, "AuthSessionClientConnection"),
    (42, "ProjectionThreadLinkedPullRequest"),
    (43, "ProjectionThreadsUnsettledAt"),
    (44, "ClearAutomaticProjectModelDefaults"),
    (45, "ProjectionProjectsAutoPull"),
    (46, "RepairAutomaticSettlementTimestamps"),
    (47, "ProjectionProjectIcon"),
    (48, "ProjectionThreadBranchPullRequest"),
    (49, "ProjectionThreadsActiveOrderKey"),
    (50, "ProjectionThreadPullRequests"),
    (51, "ProjectionThreadMessageContext"),
    (52, "ProjectionThreadTitleState"),
    (53, "PullRequestFilesViewed"),
    (54, "ProjectionThreadsAutoSettleDisabledAt"),
    (55, "OrchestrationV2"),
    (56, "RemoveRedundantProjectionIndexes"),
    (57, "ScheduledTaskWebhooks"),
    (58, "WebhookRelayDeliveries"),
    (59, "McpAppModelContext"),
];

/// The migration that creates the V2 tables. V2 previews released it as 53 or 54 (`reconcileV2PreviewMigration`).
pub const V2_MIGRATION: i64 = 55;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("There is no T3 Code database to import in {0}.")]
    Missing(String),
    #[error("The original T3 Code server appears to be running (process {pid}). Stop it, then import again.")]
    Running { pid: u32 },
    #[error("The database is damaged and was not imported: {0}")]
    Corrupt(String),
    #[error(
        "The database was written by a newer T3 Code than this server understands (schema migration {id}, {name}; this server reads migrations up to {known}). Nothing was imported."
    )]
    Newer { id: i64, name: String, known: i64 },
    #[error("The database is not a T3 Code database that can be imported: {0}")]
    Unreadable(String),
    #[error("Reading the database failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("Reading the database failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// `statev2.sqlite`: events, projections and the V2 tables.
    V2,
    /// Only `state.sqlite`: the original would copy it to V2 and import its threads when it next started.
    V1,
}

pub struct Source {
    pub kind: Kind,
    pub connection: Connection,
    /// The newest migration the file records (0 when it records none).
    pub migration: i64,
    pub migrations: Vec<(i64, String)>,
    /// Same value for the same file contents on disk; a resumed import is only continued against the source it began with.
    pub fingerprint: String,
    pub bytes: u64,
    /// Held so a private copy is removed with the source.
    _copy: Option<PathBuf>,
}
impl Drop for Source {
    fn drop(&mut self) {
        if let Some(copy) = &self._copy {
            let _ = std::fs::remove_dir_all(copy);
        }
    }
}

/// The directory of a T3 home's state: `<home>/userdata`, or the directory itself when it already holds the database.
pub fn userdata_of(from: &Path) -> PathBuf {
    if from.join("statev2.sqlite").exists() || from.join("state.sqlite").exists() {
        from.to_path_buf()
    } else {
        from.join("userdata")
    }
}

fn process_alive(pid: u32) -> bool {
    // Signal 0 checks existence; EPERM means the process exists and belongs to someone else.
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `server-runtime.json` holds the running server's process id; a live one means the files are being written.
fn refuse_when_running(userdata: &Path) -> Result<(), SourceError> {
    let Ok(text) = std::fs::read_to_string(userdata.join("server-runtime.json")) else { return Ok(()) };
    let pid = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|value| value["pid"].as_u64());
    match pid {
        Some(pid) if pid as u32 != std::process::id() && process_alive(pid as u32) => Err(SourceError::Running { pid: pid as u32 }),
        _ => Ok(()),
    }
}

pub fn table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |row| row.get(0))
}
pub fn columns(connection: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    statement.query_map([table], |row| row.get::<_, String>(0))?.collect()
}
pub fn count(connection: &Connection, table: &str) -> rusqlite::Result<u64> {
    if !table_exists(connection, table)? {
        return Ok(0);
    }
    connection.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| row.get(0))
}

/// `SELECT` list for a table whose columns depend on the schema version: a column the file lacks reads as `NULL`.
pub fn select_list(available: &HashSet<String>, wanted: &[&str]) -> String {
    wanted.iter().map(|name| if available.contains(*name) { format!("\"{name}\"") } else { format!("NULL AS \"{name}\"") }).collect::<Vec<_>>().join(",")
}

pub fn open(from: &Path, scratch: &Path) -> Result<Source, SourceError> {
    let userdata = userdata_of(from);
    refuse_when_running(&userdata)?;
    let (kind, path) = if userdata.join("statev2.sqlite").exists() {
        (Kind::V2, userdata.join("statev2.sqlite"))
    } else if userdata.join("state.sqlite").exists() {
        (Kind::V1, userdata.join("state.sqlite"))
    } else {
        return Err(SourceError::Missing(userdata.display().to_string()));
    };
    let metadata = std::fs::metadata(&path)?;
    let wal = {
        let mut name = path.as_os_str().to_os_string();
        name.push("-wal");
        PathBuf::from(name)
    };
    let wal_bytes = std::fs::metadata(&wal).map(|metadata| metadata.len()).unwrap_or(0);
    let mut copy = None;
    let connection = if wal_bytes == 0 {
        // No write-ahead log: nothing to recover, so the file is read as it lies, without locks and without any possible write.
        let uri = format!("file:{}?mode=ro&immutable=1", percent(&path));
        Connection::open_with_flags(uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?
    } else {
        // A log holds committed pages the file does not: recovery needs a writable copy, never the original.
        let directory = scratch.join(format!(".import-source-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        copy = Some(directory.clone());
        let target = directory.join("source.sqlite");
        std::fs::copy(&path, &target)?;
        let mut copied_wal = target.as_os_str().to_os_string();
        copied_wal.push("-wal");
        std::fs::copy(&wal, PathBuf::from(copied_wal))?;
        Connection::open(&target)?
    };
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let check: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| SourceError::Corrupt(error.to_string()))?;
    if check != "ok" {
        return Err(SourceError::Corrupt(check));
    }
    if !table_exists(&connection, "effect_sql_migrations")? {
        return Err(SourceError::Unreadable("it records no schema migrations".into()));
    }
    let migrations: Vec<(i64, String)> = {
        let mut statement = connection.prepare("SELECT migration_id, name FROM effect_sql_migrations ORDER BY migration_id")?;
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?
    };
    let newest = MIGRATIONS[MIGRATIONS.len() - 1].0;
    for (id, name) in &migrations {
        if *id > newest {
            return Err(SourceError::Newer { id: *id, name: name.clone(), known: newest });
        }
    }
    let migration = migrations.last().map_or(0, |row| row.0);
    // A V2 file must hold what migration 55 creates (or the preview ids that created the same tables).
    let has_v2 = migrations.iter().any(|(id, name)| *id >= V2_MIGRATION || name == "OrchestrationV2");
    if kind == Kind::V2 && !has_v2 {
        return Err(SourceError::Unreadable("statev2.sqlite records no V2 migration".into()));
    }
    if has_v2 && !table_exists(&connection, "orchestration_events")? {
        return Err(SourceError::Corrupt("the event log table is missing".into()));
    }
    let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |elapsed| elapsed.as_nanos());
    let latest: i64 = if table_exists(&connection, "orchestration_events")? {
        connection.query_row("SELECT COALESCE(MAX(sequence),0) FROM orchestration_events", [], |row| row.get(0))?
    } else {
        0
    };
    let fingerprint = format!("{:?}:{}:{}:{}:{}", kind, metadata.len(), modified, migration, latest);
    Ok(Source { kind, connection, migration, migrations, fingerprint, bytes: metadata.len(), _copy: copy })
}

/// `immutable=1` URIs need the path percent-encoded for the characters URIs reserve.
fn percent(path: &Path) -> String {
    let mut out = String::new();
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
