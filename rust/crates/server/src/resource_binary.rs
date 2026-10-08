//! Resolve the packaged or independently built original Rust telemetry sidecar.
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, thiserror::Error, serde::Serialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum TelemetryError {
    #[serde(rename = "ResourceMonitorBinaryUnsupported")]
    #[error("Resource monitoring is unsupported on {platform}/{architecture}.")]
    Unsupported {
        platform: String,
        architecture: String,
    },
    #[serde(rename = "ResourceMonitorBinaryNotFound")]
    #[error("Resource monitor binary was not found for {platform}/{architecture}.")]
    NotFound {
        platform: String,
        architecture: String,
        candidates: Vec<PathBuf>,
    },
    #[serde(rename = "ResourceMonitorBinaryNotExecutable")]
    #[error("Resource monitor binary at '{path}' is not executable.")]
    NotExecutable { path: String, mode: u32 },
    #[serde(rename = "NativeTelemetrySpawnFailed")]
    #[error("Failed to start resource monitor '{path}'.")]
    SpawnFailed {
        path: String,
        cause: serde_json::Value,
    },
    #[serde(rename = "NativeTelemetryHandshakeTimedOut")]
    #[error("Resource monitor handshake timed out after {timeout_ms}ms.")]
    HandshakeTimedOut { timeout_ms: u64 },
    #[serde(rename = "NativeTelemetryRequestTimedOut")]
    #[error("Resource monitor '{operation}' request timed out after {timeout_ms}ms.")]
    RequestTimedOut { operation: String, timeout_ms: u64 },
    #[serde(rename = "NativeTelemetryProtocolMismatch")]
    #[error(
        "Resource monitor protocol {received_version} is incompatible with expected protocol {expected_version}."
    )]
    ProtocolMismatch {
        expected_version: u32,
        received_version: serde_json::Number,
    },
    #[serde(rename = "NativeTelemetryDecodeFailed")]
    #[error("Failed to decode resource monitor output.")]
    DecodeFailed { cause: serde_json::Value },
    #[serde(rename = "NativeTelemetryCommandFailed")]
    #[error("Resource monitor command '{operation}' failed.")]
    CommandFailed {
        operation: String,
        cause: serde_json::Value,
    },
    #[serde(rename = "NativeTelemetryExited")]
    #[error("Resource monitor exited with code {exit_code}.")]
    Exited { exit_code: i32 },
    #[serde(rename = "NativeTelemetryStreamClosed")]
    #[error("Resource monitor event stream closed unexpectedly.")]
    StreamClosed,
    #[serde(rename = "NativeTelemetryUnavailable")]
    #[error("Resource monitor is unavailable: {reason}")]
    Unavailable { reason: String },
}
#[derive(Clone, Debug)]
pub struct ResourceMonitorBinary {
    pub platform: String,
    pub architecture: String,
    pub linux_gnu: bool,
    pub overrides: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
}
impl ResourceMonitorBinary {
    pub fn host(configured: Option<PathBuf>) -> Self {
        let platform = if cfg!(windows) {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            std::env::consts::OS
        };
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        let overrides = std::env::var_os("T3CODE_RESOURCE_MONITOR_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .into_iter()
            .chain(configured)
            .collect();
        let executable = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_owned))
            .unwrap_or_default();
        let rust_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Self {
            platform: platform.into(),
            architecture: architecture.into(),
            linux_gnu: cfg!(target_env = "gnu"),
            overrides,
            directories: vec![
                executable
                    .join("resource-monitor")
                    .join(format!("{platform}-{architecture}")),
                executable.join("resource-monitor"),
                executable,
                rust_root.join("target/resource-monitor/release"),
                rust_root.join("target/resource-monitor/debug"),
                rust_root.join("native/resource-monitor/target/release"),
                rust_root.join("native/resource-monitor/target/debug"),
            ],
        }
    }
    pub fn resolve(&self) -> Result<PathBuf, TelemetryError> {
        let supported = matches!(self.platform.as_str(), "darwin" | "win32")
            || (self.platform == "linux" && self.linux_gnu);
        let supported = supported && matches!(self.architecture.as_str(), "arm64" | "x64");
        if !supported && self.overrides.is_empty() {
            return Err(TelemetryError::Unsupported {
                platform: self.platform.clone(),
                architecture: self.architecture.clone(),
            });
        }
        let name = if self.platform == "win32" {
            "t3-resource-monitor.exe"
        } else {
            "t3-resource-monitor"
        };
        let candidates: Vec<_> = self
            .overrides
            .iter()
            .cloned()
            .chain(
                self.directories
                    .iter()
                    .filter(|_| supported)
                    .map(|directory| directory.join(name)),
            )
            .collect();
        for candidate in &candidates {
            if !candidate.exists() {
                continue;
            }
            #[cfg(unix)]
            if self.platform != "win32" {
                use std::os::unix::fs::MetadataExt;
                if let Ok(metadata) = candidate.metadata() {
                    if metadata.mode() & 0o111 == 0 {
                        return Err(TelemetryError::NotExecutable {
                            path: candidate.to_string_lossy().into_owned(),
                            mode: metadata.mode(),
                        });
                    }
                }
            }
            return Ok(candidate.clone());
        }
        Err(TelemetryError::NotFound {
            platform: self.platform.clone(),
            architecture: self.architecture.clone(),
            candidates,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn binary(platform: &str, architecture: &str, gnu: bool) -> ResourceMonitorBinary {
        ResourceMonitorBinary {
            platform: platform.into(),
            architecture: architecture.into(),
            linux_gnu: gnu,
            overrides: vec![],
            directories: vec![],
        }
    }
    #[test]
    fn override_precedence_executable_guard_and_unsupported_hosts_match_source_tests() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        for path in [&first, &second] {
            std::fs::write(path, "fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut resolver = binary("darwin", "arm64", false);
        resolver.overrides = vec![missing.clone(), first.clone(), second];
        assert_eq!(resolver.resolve().unwrap(), first);
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            matches!(resolver.resolve(),Err(TelemetryError::NotExecutable{path,..}) if path==first.to_string_lossy())
        );
        resolver = binary("freebsd", "ia32", false);
        assert!(matches!(
            resolver.resolve(),
            Err(TelemetryError::Unsupported { .. })
        ));
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
        resolver.overrides = vec![first.clone()];
        assert_eq!(
            resolver.resolve().unwrap(),
            first,
            "explicit override works even on unsupported platform"
        );
        resolver = binary("linux", "x64", false);
        assert!(
            matches!(resolver.resolve(), Err(TelemetryError::Unsupported { .. })),
            "musl cannot load bundled glibc binaries"
        );
        resolver.overrides = vec![missing.clone()];
        assert!(
            matches!(resolver.resolve(),Err(TelemetryError::NotFound{candidates,..}) if candidates==vec![missing])
        );
    }
}
