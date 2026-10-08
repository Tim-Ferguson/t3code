//! Legacy diagnostics projected from the canonical resource telemetry service.
//! Signaling always requires a fresh identity and an allowed backend category.
use crate::{resource_history::History, resource_telemetry_service::ResourceTelemetry};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::*;
pub type Refresh = Arc<dyn Fn() -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
pub type Signal = Arc<dyn Fn(u64, ServerProcessSignal) -> std::io::Result<()> + Send + Sync>;
#[derive(Clone)]
pub struct ProcessDiagnostics {
    pub server_pid: u64,
    pub refresh: Refresh,
    pub latest: Arc<dyn Fn() -> Value + Send + Sync>,
    pub signal: Signal,
}
impl ProcessDiagnostics {
    pub fn new(telemetry: ResourceTelemetry, server_pid: u64) -> Self {
        let current = telemetry.clone();
        Self {
            server_pid,
            refresh: Arc::new(move || {
                let current = current.clone();
                Box::pin(async move { current.refresh().await.map_err(|error| error.to_string()) })
            }),
            latest: Arc::new(move || telemetry.latest()),
            signal: Arc::new(signal_process),
        }
    }
    pub async fn read(&self) -> Result<ServerProcessDiagnosticsResult, serde_json::Error> {
        let snapshot = (self.refresh)().await.unwrap_or_else(|_| (self.latest)());
        serde_json::from_value(project_diagnostics(&snapshot, self.server_pid))
    }
    pub async fn signal(&self, input: &ServerSignalProcessInput) -> ServerSignalProcessResult {
        let pid = input.pid.0;
        let signal = input.signal;
        let refused = |message: String| {
            serde_json::from_value(json!({"pid":pid,"signal":signal,"signaled":false,"message":{"_tag":"Some","value":message}})).unwrap()
        };
        if pid == self.server_pid {
            return refused("Refusing to signal the T3 server process.".into());
        }
        let current = match (self.refresh)().await {
            Ok(current) => current,
            Err(_) => {
                return refused(format!(
                    "Could not refresh process {pid}; refusing to signal a stale identity."
                ));
            }
        };
        let selected = current["processes"].as_array().and_then(|processes| {
            processes.iter().find(|entry| {
                entry["identity"]["pid"].as_u64() == Some(pid)
                    && entry["identity"]["startTimeMs"].as_u64() == Some(input.start_time_ms.0)
            })
        });
        let Some(selected) = selected else {
            return refused(format!(
                "Process {pid} no longer matches the selected process identity."
            ));
        };
        if !can_signal(selected["category"].as_str().unwrap_or_default()) {
            return refused(format!(
                "Process {pid} is not a signalable T3 backend descendant."
            ));
        }
        match (self.signal)(pid, signal) {
            Ok(()) => serde_json::from_value(
                json!({"pid":pid,"signal":signal,"signaled":true,"message":{"_tag":"None"}}),
            )
            .unwrap(),
            Err(_) => refused(format!(
                "Failed to signal process {pid} with {}.",
                if signal == ServerProcessSignal::Int {
                    "SIGINT"
                } else {
                    "SIGKILL"
                }
            )),
        }
    }
}
fn can_signal(category: &str) -> bool {
    matches!(category, "server-child" | "provider-root" | "terminal-root")
}
pub fn format_elapsed(run_time_ms: f64) -> String {
    let seconds = (run_time_ms / 1000.).floor().max(0.) as u64;
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
fn command(entry: &Value) -> &str {
    ["command", "name"]
        .into_iter()
        .filter_map(|field| entry[field].as_str())
        .find(|value| !value.is_empty())
        .unwrap_or("unknown")
}
fn map_error(last_error: &Value, failure: bool) -> Value {
    if last_error["_tag"] == "Some" {
        if failure {
            json!({"_tag":"Some","value":{"failureTag":"ProcessDiagnosticsQueryFailedError","message":last_error["value"]}})
        } else {
            json!({"_tag":"Some","value":{"message":last_error["value"]}})
        }
    } else {
        json!({"_tag":"None"})
    }
}
pub fn project_diagnostics(snapshot: &Value, server_pid: u64) -> Value {
    let processes:Vec<Value>=snapshot["processes"].as_array().into_iter().flatten().filter(|entry|can_signal(entry["category"].as_str().unwrap_or_default())).map(|entry|json!({
        "pid":entry["identity"]["pid"],"startTimeMs":entry["identity"]["startTimeMs"],"ppid":entry["ppid"],"pgid":{"_tag":"None"},"status":entry["status"].as_str().filter(|status|!status.is_empty()).unwrap_or("Unknown"),
        "cpuPercent":entry["cpuPercent"],"rssBytes":entry["residentBytes"],"elapsed":format_elapsed(entry["runTimeMs"].as_f64().unwrap_or(0.)),"command":command(entry),"depth":entry["depth"].as_u64().unwrap_or(0).saturating_sub(1),"childPids":entry["childPids"],
    })).collect();
    json!({"serverPid":server_pid,"readAt":snapshot["readAt"],"processCount":processes.len(),"totalRssBytes":processes.iter().map(|entry|entry["rssBytes"].as_f64().unwrap_or(0.)).sum::<f64>(),"totalCpuPercent":processes.iter().map(|entry|entry["cpuPercent"].as_f64().unwrap_or(0.)).sum::<f64>(),"processes":processes,"error":map_error(&snapshot["health"]["native"]["lastError"],false)})
}
pub fn project_history(history: &History) -> Value {
    let value = serde_json::to_value(history).unwrap();
    project_history_value(&value)
}
pub fn project_history_value(history: &Value) -> Value {
    let processes:Vec<Value>=history["topProcesses"].as_array().into_iter().flatten().filter(|entry|entry["category"]=="server"||can_signal(entry["category"].as_str().unwrap_or_default())).map(|entry|json!({
        "processKey":format!("{}:{}",entry["identity"]["pid"],entry["identity"]["startTimeMs"]),"pid":entry["identity"]["pid"],"ppid":entry["ppid"],"command":command(entry),"depth":entry["depth"],"isServerRoot":entry["category"]=="server","firstSeenAt":entry["firstSeenAt"],"lastSeenAt":entry["lastSeenAt"],"currentCpuPercent":entry["currentCpuPercent"],"avgCpuPercent":entry["avgCpuPercent"],"maxCpuPercent":entry["maxCpuPercent"],"cpuSecondsApprox":entry["cpuTimeMs"].as_f64().unwrap_or(0.)/1000.,"currentRssBytes":entry["currentRssBytes"],"maxRssBytes":entry["peakRssBytes"],"sampleCount":entry["sampleCount"],
    })).collect();
    let buckets = history
        .get("legacyBackendBuckets")
        .filter(|value| !value.is_null())
        .unwrap_or(&history["buckets"]);
    let buckets:Vec<Value>=buckets.as_array().into_iter().flatten().map(|entry|json!({"startedAt":entry["startedAt"],"endedAt":entry["endedAt"],"avgCpuPercent":entry["avgCpuPercent"],"maxCpuPercent":entry["maxCpuPercent"],"maxRssBytes":entry["maxRssBytes"],"maxProcessCount":entry["maxProcessCount"]})).collect();
    json!({"readAt":history["readAt"],"windowMs":history["windowMs"],"bucketMs":history["bucketMs"],"sampleIntervalMs":history["sampleIntervalMs"],"retainedSampleCount":history["retainedSampleCount"],"totalCpuSecondsApprox":processes.iter().map(|entry|entry["cpuSecondsApprox"].as_f64().unwrap_or(0.)).sum::<f64>(),"buckets":buckets,"topProcesses":processes,"error":map_error(&history["health"]["native"]["lastError"],true)})
}
#[derive(Clone)]
pub struct ProcessResourceMonitor(pub ResourceTelemetry);
impl ProcessResourceMonitor {
    pub async fn read_history(
        &self,
        input: &ServerProcessResourceHistoryInput,
    ) -> Result<ServerProcessResourceHistoryResult, serde_json::Error> {
        let input = ResourceTelemetryHistoryInput {
            window_ms: input.window_ms,
            bucket_ms: input.bucket_ms,
        };
        let history = self.0.read_history(&input).await;
        serde_json::from_value(project_history(&history))
    }
}
#[cfg(unix)]
fn signal_process(pid: u64, signal: ServerProcessSignal) -> std::io::Result<()> {
    let pid: i32 = pid.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "PID is outside the OS process ID range",
        )
    })?;
    if unsafe {
        libc::kill(
            pid,
            if signal == ServerProcessSignal::Int {
                libc::SIGINT
            } else {
                libc::SIGKILL
            },
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
#[cfg(windows)]
fn signal_process(pid: u64, _signal: ServerProcessSignal) -> std::io::Result<()> {
    // Node/libuv treats SIGINT and SIGKILL as TerminateProcess on Windows.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn TerminateProcess(process: *mut std::ffi::c_void, code: u32) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    let pid: i32 = pid.try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "PID is outside the OS process ID range",
        )
    })?;
    unsafe {
        let handle = OpenProcess(0x00100401, 0, pid as u32);
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = TerminateProcess(handle, 1);
        let error = std::io::Error::last_os_error();
        CloseHandle(handle);
        if result != 0 { Ok(()) } else { Err(error) }
    }
}
#[cfg(not(any(unix, windows)))]
fn signal_process(_pid: u64, _signal: ServerProcessSignal) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Process signals unsupported on this host",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[tokio::test]
    async fn diagnostic_projections_and_signal_guards_match_unchanged_services() {
        for line in include_str!("../tests/fixtures/process-diagnostics.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let snapshot = row["snapshot"].clone();
            let latest = snapshot.clone();
            let failed = row["failed"] == true;
            let signal_failed = row["signalFailed"] == true;
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let service = ProcessDiagnostics {
                server_pid: row["serverPid"].as_u64().unwrap_or(111),
                refresh: Arc::new(move || {
                    let snapshot = snapshot.clone();
                    Box::pin(async move {
                        if failed {
                            Err("collector unavailable".into())
                        } else {
                            Ok(snapshot)
                        }
                    })
                }),
                latest: Arc::new(move || latest.clone()),
                signal: Arc::new(move |pid, signal| {
                    recorded
                        .lock()
                        .unwrap()
                        .push(json!({"pid":pid,"signal":signal}));
                    if signal_failed {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "not permitted",
                        ))
                    } else {
                        Ok(())
                    }
                }),
            };
            let actual = match row["op"].as_str().unwrap() {
                "read" => serde_json::to_value(service.read().await.unwrap()).unwrap(),
                "signal" => {
                    let input = serde_json::from_value(row["input"].clone()).unwrap();
                    let value = serde_json::to_value(service.signal(&input).await).unwrap();
                    assert_eq!(
                        *calls.lock().unwrap(),
                        row["calls"].as_array().unwrap().clone()
                    );
                    value
                }
                "history" => {
                    let projected = project_history_value(&row["history"]);
                    let typed: ServerProcessResourceHistoryResult =
                        serde_json::from_value(projected).unwrap();
                    serde_json::to_value(typed).unwrap()
                }
                other => panic!("Unknown case {other}"),
            };
            crate::resource_model::tests::same_numbers(&actual, &row["result"], &row.to_string());
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn real_signal_only_targets_captured_owned_child_after_fresh_identity() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child=tokio::process::Command::new("python3").arg("-c").arg("import signal;signal.signal(signal.SIGINT,lambda *_:exit(0));print('READY',flush=True);signal.pause()").stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let pid = u64::from(child.id().unwrap());
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("READY"));
        let snapshot = json!({"processes":[{"identity":{"pid":pid,"startTimeMs":2000},"category":"server-child"}]});
        let service = ProcessDiagnostics {
            server_pid: u64::from(std::process::id()),
            refresh: Arc::new(move || {
                let snapshot = snapshot.clone();
                Box::pin(async move { Ok(snapshot) })
            }),
            latest: Arc::new(|| Value::Null),
            signal: Arc::new(signal_process),
        };
        let stale: ServerSignalProcessInput =
            serde_json::from_value(json!({"pid":pid,"startTimeMs":2001,"signal":"SIGINT"}))
                .unwrap();
        assert!(!service.signal(&stale).await.signaled);
        assert!(child.try_wait().unwrap().is_none());
        let input: ServerSignalProcessInput =
            serde_json::from_value(json!({"pid":pid,"startTimeMs":2000,"signal":"SIGINT"}))
                .unwrap();
        assert!(service.signal(&input).await.signaled);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
