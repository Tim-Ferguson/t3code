//! The contract between the background-service launcher and the server it runs
//! (`apps/server/src/cloud/serviceProtocol.ts`): the state file the launcher owns, the context it
//! hands each child through the environment, the IPC messages they exchange to switch the server to
//! a new version, and the exact-SemVer rules every version in that exchange must satisfy.
//!
//! # Who reads and writes what
//!
//! * `<home>/runtime/service-state.json` ([`ServiceState`]) is written by the launcher (and by
//!   `t3 service install`, which seeds it with the version being installed). It records the active
//!   version and the one update in flight or last finished. The server only reads it, to learn
//!   whether a replacement is coming ([`service_state_has_pending_update`]).
//! * `T3_SERVICE_LAUNCHER_CONTEXT` ([`ServiceLauncherContext`], JSON) tells a child it is launcher
//!   managed and which version the launcher meant to start; a mismatch with the child's own version
//!   is refused ([`crate::service_launcher_client`]).
//! * The child and launcher talk over the launcher's IPC channel: [`ServiceLauncherChildMessage`] up,
//!   [`ServiceLauncherParentMessage`] down. An update goes `request-update` -> `update-accepted`
//!   (the launcher persists a pending record and restarts the child on the new version) ->
//!   the new child sends `prepared` -> `committed`. The Rust server has no launcher yet; these types are the
//!   wire contract a Rust launcher (or the original TypeScript one) must keep.
//! * `.service-stopping` ([`SERVICE_STOP_MARKER_FILE`]) is written by the launcher just before an explicit
//!   stop kills its child, and `.restart-pending` ([`SERVICE_RESTART_PENDING_FILE`]) by `t3 update` when
//!   the unit was repointed but the running service was left on the old version.

use std::cmp::Ordering;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Protocol 3 requires the standalone executable layout. Bump when the runtime paths or the
/// installed runtime tree change incompatibly; launchers survive self-updates.
pub const SERVICE_LAUNCHER_PROTOCOL: u32 = 3;
pub const SERVICE_LAUNCHER_CONTEXT_ENV: &str = "T3_SERVICE_LAUNCHER_CONTEXT";
pub const SERVICE_STATE_FILE: &str = "service-state.json";
/// Written by the launcher just before an explicit stop kills its child, so the child can tell "the
/// service is going away" from "the launcher is about to start my replacement" while a pending
/// update is recorded.
pub const SERVICE_STOP_MARKER_FILE: &str = ".service-stopping";
/// Written by `t3 update` when the unit was repointed at a new version but the running service was
/// deliberately left on the old one. The launcher removes it when it starts (whoever restarted the
/// service), so while it exists the service is known to be behind its unit and status reports it
/// that way.
pub const SERVICE_RESTART_PENDING_FILE: &str = ".restart-pending";

/// `ServerSelfUpdateOutcome` status: how a finished update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceUpdateStatus {
    Committed,
    RolledBack,
    Failed,
}

/// `ServerSelfUpdateOutcome`: the terminal selection the launcher recorded for one update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSelfUpdateOutcome {
    pub id: String,
    pub from_version: String,
    pub target_version: String,
    pub status: ServiceUpdateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `PendingServiceUpdate`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingServiceUpdate {
    pub id: String,
    pub from_version: String,
    pub target_version: String,
    pub db_path: String,
}

/// `ServiceUpdateRecord`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceUpdateRecord {
    Pending(PendingServiceUpdate),
    Finished(ServerSelfUpdateOutcome),
}

impl ServiceUpdateRecord {
    pub fn id(&self) -> &str {
        match self {
            Self::Pending(p) => &p.id,
            Self::Finished(o) => &o.id,
        }
    }

    pub fn from_version(&self) -> &str {
        match self {
            Self::Pending(p) => &p.from_version,
            Self::Finished(o) => &o.from_version,
        }
    }

    pub fn target_version(&self) -> &str {
        match self {
            Self::Pending(p) => &p.target_version,
            Self::Finished(o) => &o.target_version,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Pending(p) => json!({
                "id": p.id,
                "fromVersion": p.from_version,
                "targetVersion": p.target_version,
                "dbPath": p.db_path,
                "status": "pending",
            }),
            Self::Finished(o) => serde_json::to_value(o).expect("outcome serializes"),
        }
    }
}

/// `ServiceState`: the launcher's durable record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    pub active_version: String,
    pub update: Option<ServiceUpdateRecord>,
}

impl ServiceState {
    /// The JSON text `t3 service install` writes: two-space indent and a trailing newline.
    pub fn to_json_text(&self) -> String {
        let mut object = Map::new();
        object.insert("protocol".to_owned(), json!(SERVICE_LAUNCHER_PROTOCOL));
        object.insert("activeVersion".to_owned(), json!(self.active_version));
        if let Some(update) = &self.update {
            object.insert("update".to_owned(), update.to_json());
        }
        format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(object)).expect("state serializes")
        )
    }
}

/// `ServiceLauncherContext`: copied from launcher-owned state when a child is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLauncherContext {
    pub child_version: String,
    pub update: Option<ServiceUpdateRecord>,
}

/// `ServiceLauncherChildMessage`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum ServiceLauncherChildMessage {
    RequestUpdate {
        target_version: String,
        db_path: String,
    },
    Prepared {
        update_id: String,
    },
}

/// `ServiceLauncherParentMessage`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum ServiceLauncherParentMessage {
    UpdateAccepted { update_id: String },
    UpdateRejected { reason: String },
    Committed { update_id: String },
}

const SEMVER_NUMBER: &str = "(?:0|[1-9][0-9]*)";

fn exact_service_version() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        let prerelease = format!("(?:{SEMVER_NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)");
        Regex::new(&format!(
            "^{SEMVER_NUMBER}\\.{SEMVER_NUMBER}\\.{SEMVER_NUMBER}(?:-{prerelease}(?:\\.{prerelease})*)?(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$"
        ))
        .expect("exact service version pattern")
    })
}

/// Accepts exact SemVer only: never dist-tags or ranges passed to npm or filesystem paths.
pub fn is_exact_service_version(version: &str) -> bool {
    exact_service_version().is_match(version)
}

/// Compares two non-negative integers written as digit strings of any length (a `BigInt`
/// comparison); anything that is not digits counts as 0, as the empty string does for `BigInt`.
fn compare_digits(left: &str, right: &str) -> Ordering {
    let clean = |text: &str| -> String {
        if text.bytes().all(|b| b.is_ascii_digit()) {
            text.trim_start_matches('0').to_owned()
        } else {
            String::new()
        }
    };
    let (left, right) = (clean(left), clean(right));
    left.len().cmp(&right.len()).then_with(|| left.cmp(&right))
}

/// SemVer precedence for exact versions. Build metadata is ignored. Both arguments must satisfy
/// [`is_exact_service_version`]; the original throws on anything else.
pub fn compare_exact_service_versions(left: &str, right: &str) -> Ordering {
    struct Parsed<'a> {
        core: [&'a str; 3],
        prerelease: Vec<&'a str>,
    }
    fn parse(version: &str) -> Parsed<'_> {
        let without_build = version.split('+').next().unwrap_or(version);
        let (core, prerelease) = match without_build.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (without_build, None),
        };
        let mut parts = core.split('.');
        Parsed {
            core: [
                parts.next().unwrap_or("0"),
                parts.next().unwrap_or("0"),
                parts.next().unwrap_or("0"),
            ],
            prerelease: prerelease.map(|p| p.split('.').collect()).unwrap_or_default(),
        }
    }
    let (a, b) = (parse(left), parse(right));
    for index in 0..3 {
        let order = compare_digits(a.core[index], b.core[index]);
        if order != Ordering::Equal {
            return order;
        }
    }
    if a.prerelease.is_empty() || b.prerelease.is_empty() {
        return match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            _ => Ordering::Less,
        };
    }
    for index in 0..a.prerelease.len().max(b.prerelease.len()) {
        let (x, y) = (a.prerelease.get(index), b.prerelease.get(index));
        let (x, y) = match (x, y) {
            (Some(x), Some(y)) => (*x, *y),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
        };
        if x == y {
            continue;
        }
        let numeric = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        return match (numeric(x), numeric(y)) {
            (true, true) => compare_digits(x, y),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.cmp(y),
        };
    }
    Ordering::Equal
}

fn non_blank_string<'a>(value: Option<&'a Value>) -> Option<&'a str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !crate::util::js_trim(s).is_empty())
}

fn exact_version_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| is_exact_service_version(v))
        .map(str::to_owned)
}

/// `decodeServiceUpdate`
fn decode_service_update(value: &Value) -> Option<ServiceUpdateRecord> {
    let object = value.as_object()?;
    let id = non_blank_string(object.get("id"))?.to_owned();
    let from_version = exact_version_field(object, "fromVersion")?;
    let target_version = exact_version_field(object, "targetVersion")?;
    match object.get("status").and_then(Value::as_str)? {
        "pending" => {
            let db_path = non_blank_string(object.get("dbPath"))?.to_owned();
            Some(ServiceUpdateRecord::Pending(PendingServiceUpdate {
                id,
                from_version,
                target_version,
                db_path,
            }))
        }
        status @ ("committed" | "rolled-back" | "failed") => {
            let reason = match object.get("reason") {
                None => None,
                Some(_) => Some(non_blank_string(object.get("reason"))?.to_owned()),
            };
            Some(ServiceUpdateRecord::Finished(ServerSelfUpdateOutcome {
                id,
                from_version,
                target_version,
                status: match status {
                    "committed" => ServiceUpdateStatus::Committed,
                    "rolled-back" => ServiceUpdateStatus::RolledBack,
                    _ => ServiceUpdateStatus::Failed,
                },
                reason,
            }))
        }
        _ => None,
    }
}

/// `value.protocol === SERVICE_LAUNCHER_PROTOCOL` (JSON `3.0` equals `3` in JavaScript).
fn has_current_protocol(object: &Map<String, Value>) -> bool {
    object
        .get("protocol")
        .and_then(Value::as_f64)
        .is_some_and(|p| p == f64::from(SERVICE_LAUNCHER_PROTOCOL))
}

/// `decodeServiceState`: valid only when the update record agrees with the active version (a pending
/// update starts from it, a committed one ended on it, a rolled-back or failed one started from it,
/// and every update moves forward).
pub fn decode_service_state(value: &Value) -> Option<ServiceState> {
    let object = value.as_object()?;
    let update = match object.get("update") {
        None => None,
        Some(raw) => Some(decode_service_update(raw)?),
    };
    if !has_current_protocol(object) {
        return None;
    }
    let active_version = exact_version_field(object, "activeVersion")?;
    if let Some(update) = &update {
        if compare_exact_service_versions(update.target_version(), update.from_version())
            != Ordering::Greater
        {
            return None;
        }
        match update {
            ServiceUpdateRecord::Pending(p) if p.from_version != active_version => return None,
            ServiceUpdateRecord::Finished(o) => {
                let consistent = match o.status {
                    ServiceUpdateStatus::Committed => o.target_version == active_version,
                    ServiceUpdateStatus::RolledBack | ServiceUpdateStatus::Failed => {
                        o.from_version == active_version
                    }
                };
                if !consistent {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(ServiceState {
        active_version,
        update,
    })
}

/// `parseServiceState`
pub fn parse_service_state(text: &str) -> Option<ServiceState> {
    decode_service_state(&serde_json::from_str::<Value>(text).ok()?)
}

/// Detects an in-flight update across launcher protocol versions before replacing its state.
pub fn service_state_has_pending_update(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| {
            v.get("update")?
                .as_object()?
                .get("status")
                .map(|s| s == "pending")
        })
        .unwrap_or(false)
}

/// Reads the active version across launcher protocol revisions for downgrade protection.
pub fn service_state_active_version(text: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let version = value.as_object()?.get("activeVersion")?.as_str()?;
    is_exact_service_version(version).then(|| version.to_owned())
}

/// `decodeServiceLauncherContext`: the context a child finds in `T3_SERVICE_LAUNCHER_CONTEXT`; its
/// child version must be the one the update record selects (the target of a pending or committed
/// update, the origin of a rolled-back or failed one).
pub fn decode_service_launcher_context(text: &str) -> Option<ServiceLauncherContext> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let object = value.as_object()?;
    if !has_current_protocol(object) {
        return None;
    }
    let child_version = exact_version_field(object, "childVersion")?;
    let update = match object.get("update") {
        None => None,
        Some(raw) => Some(decode_service_update(raw)?),
    };
    let selected = match &update {
        Some(ServiceUpdateRecord::Pending(p)) => p.target_version.as_str(),
        Some(ServiceUpdateRecord::Finished(o)) if o.status == ServiceUpdateStatus::Committed => {
            o.target_version.as_str()
        }
        Some(ServiceUpdateRecord::Finished(o)) => o.from_version.as_str(),
        None => child_version.as_str(),
    };
    if child_version != selected {
        return None;
    }
    Some(ServiceLauncherContext {
        child_version,
        update,
    })
}

/// `decodeServiceLauncherChildMessage`
pub fn decode_service_launcher_child_message(value: &Value) -> Option<ServiceLauncherChildMessage> {
    let object = value.as_object()?;
    let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
    match object.get("type")?.as_str()? {
        "request-update" => Some(ServiceLauncherChildMessage::RequestUpdate {
            target_version: string("targetVersion")?,
            db_path: string("dbPath")?,
        }),
        "prepared" => Some(ServiceLauncherChildMessage::Prepared {
            update_id: string("updateId")?,
        }),
        _ => None,
    }
}

/// `decodeServiceLauncherParentMessage`
pub fn decode_service_launcher_parent_message(
    value: &Value,
) -> Option<ServiceLauncherParentMessage> {
    let object = value.as_object()?;
    let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
    match object.get("type")?.as_str()? {
        "update-rejected" => Some(ServiceLauncherParentMessage::UpdateRejected {
            reason: string("reason")?,
        }),
        "update-accepted" => Some(ServiceLauncherParentMessage::UpdateAccepted {
            update_id: string("updateId")?,
        }),
        "committed" => Some(ServiceLauncherParentMessage::Committed {
            update_id: string("updateId")?,
        }),
        _ => None,
    }
}
