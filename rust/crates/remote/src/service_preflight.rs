//! The staged-runtime preflight (`apps/server/src/cloud/servicePreflight.ts`). Before a server
//! switches itself to a downloaded version, it runs that version's hidden `__service-preflight
//! --database-path <db> --launcher-protocol <n>` and reads one JSON line: `ready`, or `blocked` with a
//! reason a human can act on (the new release needs a newer launcher). [`crate::self_update`] is the
//! caller; the Rust server binary prints [`run_service_preflight`]'s result for that subcommand.

use serde_json::{Value, json};

use crate::service_protocol::SERVICE_LAUNCHER_PROTOCOL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServicePreflightResult {
    Ready { version: String },
    Blocked { version: String, reason: String },
}

impl ServicePreflightResult {
    pub fn version(&self) -> &str {
        match self {
            Self::Ready { version } | Self::Blocked { version, .. } => version,
        }
    }

    /// The JSON line the subcommand prints.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Ready { version } => json!({
                "status": "ready",
                "version": version,
                "launcherProtocol": SERVICE_LAUNCHER_PROTOCOL,
            }),
            Self::Blocked { version, reason } => json!({
                "status": "blocked",
                "version": version,
                "reason": reason,
            }),
        }
    }
}

/// `runServicePreflight`: `launcher_protocol` is the one the running launcher speaks; `version` is
/// this build's version. `database_path` is accepted for compatibility (older servers always pass it).
pub fn run_service_preflight(
    _database_path: &str,
    launcher_protocol: u32,
    version: &str,
) -> ServicePreflightResult {
    if launcher_protocol != SERVICE_LAUNCHER_PROTOCOL {
        return ServicePreflightResult::Blocked {
            version: version.to_owned(),
            reason:
                "This release requires a newer T3 Code service launcher. Update it on the server machine."
                    .to_owned(),
        };
    }
    ServicePreflightResult::Ready {
        version: version.to_owned(),
    }
}

/// `decodeServicePreflightResult`
pub fn decode_service_preflight_result(value: &Value) -> Option<ServicePreflightResult> {
    let record = value.as_object()?;
    let version = record.get("version").and_then(Value::as_str)?.to_owned();
    match record.get("status").and_then(Value::as_str)? {
        "ready"
            if record
                .get("launcherProtocol")
                .and_then(Value::as_f64)
                .is_some_and(|p| p == f64::from(SERVICE_LAUNCHER_PROTOCOL)) =>
        {
            Some(ServicePreflightResult::Ready { version })
        }
        "blocked" => Some(ServicePreflightResult::Blocked {
            version,
            reason: record.get("reason").and_then(Value::as_str)?.to_owned(),
        }),
        _ => None,
    }
}
