//! What a client is told of the one-time import of the original's data (RUST-PORT EXTENSION; the original has no
//! such state and the contract is `t3_contracts::state_import`). The block is derived from `rust-import-report.json`,
//! which the import writes beside the store, and stays until `server.acknowledgeStateImport` records that it was seen
//! (`rust-import-acknowledged`, holding the time of the import it covers, so a later import shows again).
//!
//! A server whose import failed does not open its store. `failed_router` is all it serves: the environment descriptor,
//! the status block at `/api/rust/state-import`, and the web assets, so a client can show what went wrong.
use crate::{persistence::Store, transport::serve_asset};
use axum::{
    Json, Router,
    extract::State,
    http::Uri,
    response::Response,
    routing::get,
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_contracts::{
    StateImportCounts, StateImportOutcome, StateImportSkipped, StateImportSource, StateImportSourceKind, StateImportStatus, UtcDateTime,
};

pub const REPORT_FILE: &str = "rust-import-report.json";
pub const ACKNOWLEDGED_FILE: &str = "rust-import-acknowledged";

/// The block for a state directory that was imported into, or none if it was not or the user has seen it.
pub fn status(state_dir: &Path) -> Option<StateImportStatus> {
    let report: Value = serde_json::from_slice(&std::fs::read(state_dir.join(REPORT_FILE)).ok()?).ok()?;
    let block = from_report(&report, || std::fs::metadata(state_dir.join(REPORT_FILE)).and_then(|meta| meta.modified()).ok().map(chrono::DateTime::<Utc>::from))?;
    let acknowledged = std::fs::read_to_string(state_dir.join(ACKNOWLEDGED_FILE)).ok();
    (acknowledged.as_deref().map(str::trim) != Some(stamp(&block).as_str())).then_some(block)
}

fn stamp(block: &StateImportStatus) -> String {
    block.at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Records that the block was seen. Answers whether there was one to clear.
pub fn acknowledge(state_dir: &Path) -> std::io::Result<bool> {
    let Some(block) = status(state_dir) else { return Ok(false) };
    let temporary = state_dir.join(format!("{ACKNOWLEDGED_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, stamp(&block))?;
    std::fs::rename(&temporary, state_dir.join(ACKNOWLEDGED_FILE))?;
    Ok(true)
}

/// Puts the block into a config document, as the config services do for their own keys.
pub fn overlay_config(store: &Store, config: &mut Value) {
    let block = store.state_dir().and_then(status);
    match block {
        Some(block) => config["stateImport"] = serde_json::to_value(block).expect("status block"),
        None => {
            if let Some(object) = config.as_object_mut() {
                object.remove("stateImport");
            }
        }
    }
}

/// The block for an import that did not finish.
pub fn failed(error: &str) -> StateImportStatus {
    StateImportStatus {
        status: StateImportOutcome::Failed,
        at: Utc::now().into(),
        error: Some(error.to_owned()),
        source: None,
        counts: None,
        skipped: Vec::new(),
        warnings: Vec::new(),
        reenter: Vec::new(),
    }
}

/// `rust-import-report.json` as a block. `fallback_time` stands in for the time of a report written before it carried one.
pub fn from_report(report: &Value, fallback_time: impl FnOnce() -> Option<chrono::DateTime<Utc>>) -> Option<StateImportStatus> {
    let at = report["importedAt"].as_str().and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).map(|time| time.with_timezone(&Utc)).or_else(fallback_time)?;
    let number = |value: &Value| value.as_u64().unwrap_or(0);
    let kind = match report["sourceKind"].as_str()? {
        "v2" => StateImportSourceKind::V2,
        "v1" => StateImportSourceKind::V1,
        _ => return None,
    };
    let tables = report["tables"].as_object();
    let table = |name: &str, field: &str| tables.and_then(|tables| tables.get(name)).map_or(0, |table| number(&table[field]));
    let skipped = tables
        .into_iter()
        .flatten()
        .filter(|(_, table)| number(&table["read"]) > 0 && table["disposition"].as_str().is_some_and(|text| text.contains("not carried")))
        .map(|(name, table)| StateImportSkipped { item: name.clone(), rows: number(&table["read"]), reason: table["disposition"].as_str().unwrap_or_default().to_owned() })
        .collect();
    let strings = |value: &Value| value.as_array().into_iter().flatten().filter_map(|text| text.as_str().map(str::to_owned)).collect::<Vec<_>>();
    Some(StateImportStatus {
        status: StateImportOutcome::Imported,
        at: UtcDateTime::from(at),
        error: None,
        source: Some(StateImportSource { kind, migration: number(&report["sourceMigration"]) as u32, bytes: number(&report["sourceBytes"]) }),
        counts: Some(StateImportCounts {
            projects: number(&report["projectsImported"]),
            threads: number(&report["threadsImported"]),
            events: report["events"].as_object().into_iter().flatten().map(|(_, row)| number(&row[1])).sum(),
            command_receipts: table("orchestration_command_receipts", "imported"),
            scheduled_tasks: table("scheduled_tasks", "imported"),
        }),
        skipped,
        warnings: strings(&report["warnings"]),
        reenter: strings(&report["credentialsToReenter"]),
    })
}

/// What a server whose import failed serves.
pub struct FailedMode {
    /// The environment descriptor, with no capability a client could act on.
    pub environment: Value,
    pub assets: Option<PathBuf>,
    pub status: StateImportStatus,
}

pub fn failed_router(mode: FailedMode) -> Router {
    let state = std::sync::Arc::new(mode);
    Router::new()
        .route("/.well-known/t3/environment", get(|State(mode): State<std::sync::Arc<FailedMode>>| async move { Json(mode.environment.clone()) }))
        .route("/api/rust/state-import", get(|State(mode): State<std::sync::Arc<FailedMode>>| async move { Json(json!({"stateImport":mode.status})) }))
        .fallback(get(|State(mode): State<std::sync::Arc<FailedMode>>, uri: Uri| async move { serve_asset(mode.assets.clone(), uri).await }))
        .layer(axum::middleware::map_response(|mut response: Response| async move {
            response.headers_mut().insert(axum::http::header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }))
        .with_state(state)
}

/// The descriptor of a server that is not running: the identity the original had (so a client that saved this
/// environment recognises it) or a throwaway one, and no capability.
pub fn failed_environment(state_dir: &Path, version: &str) -> Value {
    let read = |name: &str| std::fs::read_to_string(state_dir.join(name)).ok().map(|text| text.trim().to_owned()).filter(|text| uuid::Uuid::parse_str(text).is_ok());
    let environment_id = read("rust-environment-id").or_else(|| read("environment-id")).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    json!({"environmentId":environment_id,"label":"T3 Code Rust (state import failed)","platform":{"os":os,"arch":arch},"serverVersion":version,"orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":false}})
}
