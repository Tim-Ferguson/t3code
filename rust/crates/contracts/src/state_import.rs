//! RUST-PORT EXTENSION. Nothing in this file exists in the original's contracts (`packages/contracts`): the
//! original never imports another server's state, so it has nothing to tell a client about one. This server does,
//! once, on its first start beside the original's data (`t3-server import`, rust/README.md), and a client that
//! opens the app afterwards can show what came across, what did not, and what the user has to enter again.
//!
//! The block travels as the optional `stateImport` key of the server config (`server.getConfig` and the snapshot
//! of `subscribeServerConfig`). It is absent when no import happened, and once the user has acknowledged it with
//! `server.acknowledgeStateImport`. A server whose import failed does not run: it serves the same block, alone,
//! at `GET /api/rust/state-import` as `{"stateImport": ...}`, with the web assets and the environment descriptor.
use crate::UtcDateTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StateImportOutcome {
    Imported,
    Failed,
}

/// Which database the import read, and how big it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateImportSource {
    /// "v2" (the original's `statev2.sqlite`) or "v1" (only the older `state.sqlite`).
    pub kind: StateImportSourceKind,
    /// The original's schema migration the database was at.
    pub migration: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StateImportSourceKind {
    V2,
    V1,
}

/// What came across, counted in the store as it now is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateImportCounts {
    pub projects: u64,
    pub threads: u64,
    /// Application events replayed.
    pub events: u64,
    pub command_receipts: u64,
    pub scheduled_tasks: u64,
}

/// Something of the original's that was read and deliberately not carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateImportSkipped {
    /// The original's table.
    pub item: String,
    pub rows: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateImportStatus {
    pub status: StateImportOutcome,
    /// When the import finished, or failed.
    pub at: UtcDateTime,
    /// Why it failed; only on `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Only on `imported`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<StateImportSource>,
    /// Only on `imported`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<StateImportCounts>,
    #[serde(default)]
    pub skipped: Vec<StateImportSkipped>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// What the user has to enter or do again, each a sentence.
    #[serde(default)]
    pub reenter: Vec<String>,
}

/// `server.acknowledgeStateImport`: no input, answers whether there was a block to clear.
pub const ACKNOWLEDGE_STATE_IMPORT_METHOD: &str = "server.acknowledgeStateImport";
