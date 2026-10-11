//! The hidden `t3 __ssh-helper` subcommands (`apps/server/src/cli/sshHelper.ts`). The launch script
//! ([`crate::ssh_scripts`]) runs them on the remote host through the release-archive `t3`
//! executable instead of inline `node -` snippets, so a remote host needs no Node. The script
//! parses their output and exit codes, which therefore must not change:
//!
//! * `pick-port <port-file> <default-port> <scan-window>` prints the first loopback port that can be
//!   bound, starting at the integer in `<port-file>` (else `<default-port>`); exit 1 when none is
//!   free.
//! * `wait-ready <port> <timeout-ms> <probe-timeout-ms>` exits 0 once `GET http://127.0.0.1:<port>/`
//!   answers 2xx, 1 when the deadline passes first.
//! * `runtime-port <runtime-file>` prints `<pid> <port>` for a live server recorded in a
//!   `server-runtime.json` (an `http:` loopback origin and a running pid), else exits 1.
//!
//! The Rust server binary exposes them as the `__ssh-helper` subcommand and calls [`run`].

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// What the subcommand writes and the code it exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHelperOutput {
    pub stdout: String,
    pub exit_code: i32,
}

fn parse_int(text: &str) -> Option<i64> {
    crate::util::js_parse_int(text)
}

/// `pick-port`. A port outside `0..=65535` in the scanned range makes the original throw, which is
/// also exit 1; port 0 binds but is skipped, because the original tests the returned port for
/// truthiness.
pub fn pick_port(port_file: &Path, default_port: i64, scan_window: i64) -> Option<i64> {
    let raw = std::fs::read_to_string(port_file).unwrap_or_default();
    let start = parse_int(raw.trim()).unwrap_or(default_port);
    let mut port = start;
    while port < start + scan_window {
        if !(0..=65_535).contains(&port) {
            return None;
        }
        let bound = std::net::TcpListener::bind(("127.0.0.1", port as u16)).is_ok();
        if bound && port != 0 {
            return Some(port);
        }
        port += 1;
    }
    None
}

async fn probe(port: i64, probe_timeout: Duration) -> bool {
    let call = async {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
    };
    match tokio::time::timeout(probe_timeout, call).await {
        Ok(Ok(response)) => response.status().is_success(),
        _ => false,
    }
}

/// `wait-ready`: probes every 100 ms until `timeout` has passed.
pub async fn wait_ready(port: i64, timeout: Duration, probe_timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if probe(port, probe_timeout).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// JavaScript's `Number(value)` for the JSON values `runtime-port` meets.
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => {
            let trimmed = crate::util::js_trim(s);
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Bool(b)) => f64::from(*b),
        Some(Value::Null) => 0.0,
        _ => f64::NAN,
    }
}

fn is_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0
}

#[cfg(unix)]
fn process_exists(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists and may be signalled; Node's
    // `process.kill(pid, 0)` is the same call, and any error (including EPERM) counts as failure.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn process_exists(_pid: i64) -> bool {
    false
}

/// `runtime-port`: `Some((pid, port))` for a live default-home server.
pub fn runtime_port(runtime_file: &Path) -> Option<(i64, i64)> {
    let text = std::fs::read_to_string(runtime_file).ok()?;
    let runtime: Value = serde_json::from_str(&text).ok()?;
    let pid = js_number(runtime.get("pid"));
    let port = js_number(runtime.get("port"));
    if !is_integer(pid) || pid <= 0.0 || !is_integer(port) {
        return None;
    }
    // `String(runtime.origin ?? "")`
    let origin = match runtime.get("origin") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let origin = url::Url::parse(&origin).ok()?;
    if origin.scheme() != "http" || !matches!(origin.host_str(), Some("127.0.0.1" | "localhost")) {
        return None;
    }
    process_exists(pid as i64).then_some((pid as i64, port as i64))
}

/// Runs `__ssh-helper <args...>` (the arguments after `__ssh-helper`).
pub async fn run(args: &[String]) -> SshHelperOutput {
    let usage = |message: &str| SshHelperOutput {
        stdout: String::new(),
        exit_code: {
            eprintln!("{message}");
            2
        },
    };
    let int = |index: usize| args.get(index).and_then(|a| a.parse::<i64>().ok());
    match args.first().map(String::as_str) {
        Some("pick-port") => {
            let (Some(file), Some(default_port), Some(window)) = (args.get(1), int(2), int(3))
            else {
                return usage("usage: __ssh-helper pick-port <port-file> <default-port> <scan-window>");
            };
            match pick_port(Path::new(file), default_port, window) {
                Some(port) => SshHelperOutput {
                    stdout: port.to_string(),
                    exit_code: 0,
                },
                None => SshHelperOutput {
                    stdout: String::new(),
                    exit_code: 1,
                },
            }
        }
        Some("wait-ready") => {
            let (Some(port), Some(timeout), Some(probe_timeout)) = (int(1), int(2), int(3)) else {
                return usage("usage: __ssh-helper wait-ready <port> <timeout-ms> <probe-timeout-ms>");
            };
            let ready = wait_ready(
                port,
                Duration::from_millis(timeout.max(0) as u64),
                Duration::from_millis(probe_timeout.max(0) as u64),
            )
            .await;
            SshHelperOutput {
                stdout: String::new(),
                exit_code: i32::from(!ready),
            }
        }
        Some("runtime-port") => {
            let Some(file) = args.get(1) else {
                return usage("usage: __ssh-helper runtime-port <runtime-file>");
            };
            match runtime_port(Path::new(file)) {
                Some((pid, port)) => SshHelperOutput {
                    stdout: format!("{pid} {port}"),
                    exit_code: 0,
                },
                None => SshHelperOutput {
                    stdout: String::new(),
                    exit_code: 1,
                },
            }
        }
        _ => usage("usage: __ssh-helper <pick-port|wait-ready|runtime-port> ..."),
    }
}
