//! What the Pi flows share: the fake `pi` executable, and the replay controller that plays a recorded Pi RPC
//! session (`pi.rpc-jsonl`, the transcripts the original adapter was recorded against) to it.
//!
//! The controller is `PiReplayController` of the original's `PiAdapterV2.testkit.ts`, ported: entries are stdio
//! records, `expect_outbound` is one record the adapter must write to stdin and `emit_inbound` is one record Pi
//! writes to stdout. Pi echoes the adapter's request ids, so a recorded id is rebound to the id the adapter really
//! sent. `<any>` in an outbound frame matches any value. Every spawned process begins with a `process_start` record
//! carrying its argv; records of the Nth process (N > 1) carry an `@pN` label suffix. A write that matches a later
//! outbound record of the same turn is held until the transcript reaches it; a write that matches nothing before the
//! next turn-starting record fails the replay at once. A failure ends every process, and the test reports it.
#![allow(dead_code)]
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::mpsc,
};

pub const FAKE_CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi-fake-cli.py");
const ANY: &str = "<any>";

/// An executable at an absolute path in `directory` that runs the fake: never a bare name on the developer's PATH.
pub fn shim(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let binary = directory.join("pi");
    std::fs::write(&binary, format!("#!/bin/sh\nexec python3 '{FAKE_CLI}' \"$@\"\n")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

struct Process {
    ordinal: usize,
    to_pi: Option<mpsc::UnboundedSender<String>>,
    /// Recorded request id -> the id the adapter sent in its place.
    ids: HashMap<String, String>,
    open: bool,
}
struct State {
    scenario: String,
    entries: Vec<Value>,
    cursor: usize,
    consumed: HashSet<usize>,
    held: Vec<(usize, Value)>,
    processes: Vec<Process>,
    failure: Option<String>,
}

fn matches(expected: &Value, actual: &Value) -> bool {
    if expected == ANY {
        return true;
    }
    match (expected, actual) {
        (Value::Array(left), Value::Array(right)) => left.len() == right.len() && left.iter().zip(right).all(|(left, right)| matches(left, right)),
        (Value::Object(left), Value::Object(right)) => left.keys().chain(right.keys()).all(|key| match (left.get(key), right.get(key)) {
            (Some(left), Some(right)) => matches(left, right),
            // A key the recording lacks, or one only the recording has, is a difference; only `<any>` stands for a missing one.
            (Some(left), None) => left == ANY,
            (None, _) => false,
        }),
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (left, right) => left == right,
    }
}
fn adapter_request_id(frame: &Value) -> Option<&str> {
    frame["id"].as_str().filter(|id| id.strip_prefix("t3-").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())))
}
/// Compares a recorded request against the adapter's own request id, so the correlation Pi recorded holds without
/// pinning counter values. Any other id, such as the Pi request id an `extension_ui_response` must echo, stays exact.
fn with_request_id(expected: &Value, actual: &Value) -> Value {
    match (adapter_request_id(expected), adapter_request_id(actual)) {
        (Some(_), Some(id)) => {
            let mut expected = expected.clone();
            expected["id"] = json!(id);
            expected
        }
        _ => expected.clone(),
    }
}
fn starts_turn(entry: &Value) -> bool {
    entry["type"] == "expect_outbound" && matches!(entry["frame"]["type"].as_str(), Some("prompt" | "compact" | "process_start"))
}
/// `response:get_state@p2` belongs to the second spawned process; no suffix is the first.
fn entry_process(entry: &Value) -> usize {
    entry["label"].as_str().and_then(|label| label.rsplit_once("@p")).and_then(|(_, number)| number.parse().ok()).unwrap_or(1)
}

impl State {
    fn receive(&mut self, process: usize, actual: Value) {
        if self.failure.is_some() {
            return;
        }
        let ordinal = self.processes[process].ordinal;
        let mut possible = false;
        for index in self.cursor..self.entries.len() {
            let entry = &self.entries[index];
            let pending = entry["type"] == "expect_outbound" && !self.consumed.contains(&index);
            possible = pending && entry_process(entry) == ordinal && matches(&with_request_id(&entry["frame"], &actual), &actual);
            if possible || (pending && starts_turn(entry)) {
                break;
            }
        }
        if !possible {
            let expected = self.entries.get(self.cursor).cloned().unwrap_or(Value::Null);
            self.fail(format!("Pi replay frame mismatch in {} at cursor {} (expected {}, received {} from process {ordinal})\n  expected: {}\n  actual:   {}", self.scenario, self.cursor, expected["label"], actual["type"], expected["frame"], actual));
            return;
        }
        self.held.push((process, actual));
        self.advance();
    }
    /// Emits due inbound records and consumes held writes until neither can progress.
    fn advance(&mut self) {
        let mut progressed = true;
        while progressed && self.failure.is_none() {
            progressed = self.drain_inbound();
            for index in 0..self.held.len() {
                let (process, actual) = self.held[index].clone();
                if self.consume_pending(process, &actual) {
                    self.held.remove(index);
                    progressed = true;
                    break;
                }
            }
        }
    }
    /// Matches a write against the outbound records due before the next inbound one.
    fn consume_pending(&mut self, process: usize, actual: &Value) -> bool {
        let ordinal = self.processes[process].ordinal;
        for index in self.cursor..self.entries.len() {
            let entry = self.entries[index].clone();
            if entry["type"] != "expect_outbound" {
                return false;
            }
            if self.consumed.contains(&index) || entry_process(&entry) != ordinal || !matches(&with_request_id(&entry["frame"], actual), actual) {
                continue;
            }
            if let (Some(recorded), Some(sent)) = (adapter_request_id(&entry["frame"]), adapter_request_id(actual)) {
                self.processes[process].ids.insert(recorded.to_owned(), sent.to_owned());
            }
            self.consumed.insert(index);
            while self.consumed.contains(&self.cursor) {
                self.cursor += 1;
            }
            return true;
        }
        false
    }
    fn drain_inbound(&mut self) -> bool {
        let mut emitted = false;
        while self.failure.is_none() {
            let Some(entry) = self.entries.get(self.cursor).cloned() else { return emitted };
            if entry["type"] == "runtime_exit" {
                // Pi exited on its own in the recording: the newest process's stdout ends.
                self.cursor += 1;
                emitted = true;
                if let Some(last) = self.processes.last_mut() {
                    last.to_pi = None;
                    last.open = false;
                }
                continue;
            }
            if entry["type"] != "emit_inbound" {
                return emitted;
            }
            let Some(process) = self.processes.get_mut(entry_process(&entry) - 1) else { return emitted };
            self.cursor += 1;
            emitted = true;
            // Output a process wrote after the adapter released it was never observed.
            if !process.open {
                continue;
            }
            let mut frame = entry["frame"].clone();
            if let Some(rebound) = frame["id"].as_str().and_then(|id| process.ids.get(id)) {
                frame["id"] = json!(rebound);
            }
            if let Some(sender) = &process.to_pi {
                let _ = sender.send(frame.to_string());
            }
        }
        emitted
    }
    fn fail(&mut self, cause: String) {
        if self.failure.is_none() {
            self.failure = Some(cause);
            for process in &mut self.processes {
                process.to_pi = None;
            }
        }
    }
}

pub struct Replay {
    pub socket: PathBuf,
    state: Arc<Mutex<State>>,
    _accept: tokio::task::JoinHandle<()>,
}
impl Replay {
    /// Plays `entries` (the recording's `transcript`, without its start record) to every process that connects.
    pub async fn start(directory: &Path, scenario: &str, entries: &[Value]) -> Self {
        let socket = directory.join("replay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(Mutex::new(State { scenario: scenario.to_owned(), entries: entries.iter().filter(|entry| entry["type"] != "transcript_start").cloned().collect(), cursor: 0, consumed: HashSet::new(), held: vec![], processes: vec![], failure: None }));
        let served = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = served.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let (to_pi, mut from_replay) = mpsc::unbounded_channel::<String>();
                    tokio::spawn(async move {
                        while let Some(line) = from_replay.recv().await {
                            if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                                return;
                            }
                        }
                        // Replay over for this process: its stdout ends.
                        let _ = write.shutdown().await;
                    });
                    let mut lines = BufReader::new(read).lines();
                    let Ok(Some(first)) = lines.next_line().await else { return };
                    let index = {
                        let mut state = state.lock().unwrap();
                        let ordinal = state.processes.len() + 1;
                        state.processes.push(Process { ordinal, to_pi: Some(to_pi), ids: HashMap::new(), open: true });
                        let index = state.processes.len() - 1;
                        state.receive(index, serde_json::from_str(&first).unwrap());
                        index
                    };
                    while let Ok(Some(line)) = lines.next_line().await {
                        state.lock().unwrap().receive(index, serde_json::from_str(&line).unwrap());
                    }
                    // The adapter released the process: its scope closed.
                    let mut state = state.lock().unwrap();
                    state.processes[index].open = false;
                    state.processes[index].to_pi = None;
                });
            }
        });
        Self { socket, state, _accept: accept }
    }
    pub fn failure(&self) -> Option<String> {
        self.state.lock().unwrap().failure.clone()
    }
    /// How far the recording was played, and what is left of it.
    pub fn progress(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (state.cursor, state.entries.len())
    }
    /// Panics with the replay's own account when it failed, or ended with part of the recording unplayed.
    pub fn assert_complete(&self) {
        let state = self.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            panic!("{failure}");
        }
        assert!(state.cursor == state.entries.len() && state.held.is_empty(), "Pi replay of {} ended with {} unconsumed entries at cursor {} (next {})", state.scenario, state.entries.len() - state.cursor, state.cursor, state.entries.get(state.cursor).map_or(Value::Null, |entry| entry["label"].clone()));
    }
}
