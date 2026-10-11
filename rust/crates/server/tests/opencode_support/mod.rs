//! What the OpenCode flows share: a stand-in OpenCode server (`fixtures/opencode-fake-cli.py serve`) the test starts on
//! loopback, with the process it started held so it is ended on every exit path. No real `opencode` runs and nothing
//! touches a network beyond loopback.
#![allow(dead_code)]
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

pub const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/opencode-fake-cli.py");

/// A replaying OpenCode server. Dropping it ends the process it holds the pid of.
pub struct FakeServer {
    child: Child,
    pub url: String,
    pub status: PathBuf,
    gate: Option<PathBuf>,
}
impl FakeServer {
    /// Starts the server for `entries` (a recording's entries) under `directory`. `password` is what it requires.
    pub fn start(directory: &Path, scenario: &str, protocol: &str, version: &str, entries: &Value, password: Option<&str>) -> Self {
        Self::start_gated(directory, scenario, protocol, version, entries, password, None)
    }
    /// With a gate: the event labelled `label` is held back until [`FakeServer::release`] writes to the pipe.
    pub fn start_gated(directory: &Path, scenario: &str, protocol: &str, version: &str, entries: &Value, password: Option<&str>, gate: Option<&str>) -> Self {
        Self::start_with(directory, scenario, protocol, version, entries, password, gate, None)
    }
    /// With the answers a status check gets before the first event subscription, for a recording that holds none
    /// (`OPENCODE_FIXTURE_CANNED`: operation name to body).
    pub fn start_canned(directory: &Path, scenario: &str, protocol: &str, version: &str, entries: &Value, password: Option<&str>, canned: &Value) -> Self {
        Self::start_with(directory, scenario, protocol, version, entries, password, None, Some(canned))
    }
    #[allow(clippy::too_many_arguments)]
    fn start_with(directory: &Path, scenario: &str, protocol: &str, version: &str, entries: &Value, password: Option<&str>, gate: Option<&str>, canned: Option<&Value>) -> Self {
        let transcript = directory.join("transcript.json");
        let mut recording = json!({"scenario":scenario,"protocol":protocol,"version":version,"entries":entries});
        if let Some(label) = gate {
            let fifo = directory.join("replay-gate.fifo");
            let path = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            recording["gate"] = json!({"label":label,"fifo":fifo});
        }
        std::fs::write(&transcript, recording.to_string()).unwrap();
        let status = directory.join("replay-status.json");
        let mut command = Command::new("python3");
        command.arg(FAKE).args(["serve", "--hostname=127.0.0.1", "--port=0"]).env_clear().env("OPENCODE_FIXTURE_TRANSCRIPT", &transcript).env("OPENCODE_FIXTURE_STATUS", &status).env("PATH", std::env::var("PATH").unwrap_or_default()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(canned) = canned {
            command.env("OPENCODE_FIXTURE_CANNED", canned.to_string());
        }
        if let Some(password) = password {
            command.env(if protocol == "opencode-sdk.sse" { "OPENCODE_SERVER_PASSWORD" } else { "OPENCODE_PASSWORD" }, password);
        }
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_owned();
        assert!(url.starts_with("http://127.0.0.1:"), "the fake announced {line:?}");
        Self { child, url, status, gate: gate.map(|_| directory.join("replay-gate.fifo")) }
    }
    /// Lets the held event through. The write blocks until the server reads, so it runs off the async threads.
    pub async fn release(&self) {
        let fifo = self.gate.clone().expect("the server has a gate");
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            std::fs::OpenOptions::new().write(true).open(fifo).unwrap().write_all(b"release").unwrap();
        })
        .await
        .unwrap();
    }
    /// Where the replay stands: its cursor, the number of entries, and the first mismatch.
    pub fn replay_status(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.status).unwrap()).unwrap()
    }
}
impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
