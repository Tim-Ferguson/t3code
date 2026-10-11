//! Pi coding agent RPC transport: `pi --mode rpc` over stdio, one JSON record per LF-delimited line.
//! Mirrors `PiRpc.ts`. Requests carry an `id` (`t3-N`, per process) and are answered by a `response`
//! record with the same id; every other record is a session event, delivered in arrival order. A
//! `prompt`, `steer` or `compact` is sent without an id and its acknowledgement comes back id-less,
//! as an event. Lines are split on LF only (a trailing CR is dropped), a line that is not JSON is
//! dropped, and a line over 8 MiB is dropped, so a chatty extension cannot take the session down.
//!
//! This module knows nothing about orchestration. The adapter consumes [`Event`]s.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{mpsc, oneshot, watch},
};

/// `MAX_PI_RECORD_CHARS`.
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// `TERMINATION_GRACE`: how long a stopped process group has to wind down after SIGTERM before SIGKILL.
const TERMINATION_GRACE: Duration = Duration::from_secs(1);
/// How long a process whose stdout closed has to be seen exiting, so the exit code can be reported.
const EXIT_DIAGNOSIS: Duration = Duration::from_millis(250);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Launch {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    /// Variables removed from the inherited environment: a Pi child never reuses credentials meant for another session.
    pub remove_env: Vec<String>,
}

/// Why a request produced no data (`PiRpcError`, `PiRpcTimeoutError`).
#[derive(Clone, Debug, PartialEq)]
pub enum RpcError {
    /// Pi answered `success: false`. `detail` is bounded; the raw payload is kept for classification.
    Failed { operation: String, detail: String, raw: Value },
    Timeout { operation: String, millis: u64 },
    /// The process is gone or was stopped.
    Closed(String),
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Failed { operation, detail, .. } => write!(f, "Pi RPC {operation} failed: {detail}."),
            RpcError::Timeout { operation, millis } => write!(f, "Pi RPC {operation} failed: timed out after {millis}ms."),
            RpcError::Closed(detail) => write!(f, "Pi RPC failed: {detail}."),
        }
    }
}
impl std::error::Error for RpcError {}
impl RpcError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, RpcError::Timeout { .. })
    }
}

/// What the process wrote after its last record: how it ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exit {
    pub code: Option<i32>,
    /// The host stopped it (`terminate`) rather than it ending on its own.
    pub closed_by_host: bool,
}
impl Exit {
    /// `describePiStdoutClosure`. Stderr stays out of every public detail: it can carry credentials or prompt text.
    pub fn describe(&self) -> String {
        match self.code {
            Some(code) => format!("pi process exited with code {code}"),
            None => "pi process closed stdout".into(),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    /// A session event: every record that is not the answer to a pending request.
    Record(Value),
    /// stdout ended. Always the last event.
    Closed(Exit),
}

/// The requests waiting for an answer. Once the process has ended nothing more can be answered, so the map
/// is closed under the same lock that drains it: a request made after that fails at once instead of waiting
/// out its timeout.
#[derive(Default)]
struct Waiting {
    closed: Option<RpcError>,
    waiters: HashMap<String, oneshot::Sender<Result<Value, RpcError>>>,
}
type Pending = Arc<Mutex<Waiting>>;
impl Waiting {
    fn close(&mut self, reason: RpcError) {
        for (_, waiter) in self.waiters.drain() {
            let _ = waiter.send(Err(reason.clone()));
        }
        self.closed.get_or_insert(reason);
    }
}

/// A running `pi --mode rpc`. Dropping it stops the process group.
pub struct PiProcess {
    writer: mpsc::UnboundedSender<String>,
    events: mpsc::UnboundedSender<Event>,
    pending: Pending,
    next_id: AtomicU64,
    stop: watch::Sender<bool>,
    /// The reader has reported the end, and the supervisor has ended the group.
    done: watch::Receiver<bool>,
    ended: watch::Receiver<bool>,
}

/// `routeRecord`.
fn route(line: &[u8], events: &mpsc::UnboundedSender<Event>, pending: &Pending) {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        tracing::debug!(line_length = line.len(), "Dropping non-JSON pi stdout line.");
        return;
    };
    if !record.is_object() {
        return;
    }
    if record["type"] == "response" {
        if let Some(id) = record["id"].as_str() {
            if let Some(waiter) = pending.lock().unwrap().waiters.remove(id) {
                let _ = waiter.send(if record["success"] == true {
                    Ok(record["data"].clone())
                } else {
                    Err(RpcError::Failed { operation: record["command"].as_str().unwrap_or("request").to_owned(), detail: summarize_error(&record["error"]), raw: record["error"].clone() })
                });
                return;
            }
        }
    }
    let _ = events.send(Event::Record(record));
}
/// `summarizePiError`: bounded and human-readable, so a message never carries unbounded remote text.
fn summarize_error(error: &Value) -> String {
    let text = match error {
        Value::String(text) => text.clone(),
        Value::Null => return "unknown error".into(),
        other => other.to_string(),
    };
    if text.chars().count() > 200 { format!("{}…", text.chars().take(200).collect::<String>()) } else { text }
}

/// Splits a byte stream into LF-delimited records: lines over the cap are dropped whole.
struct Framer {
    buffer: Vec<u8>,
    dropping: bool,
}
impl Framer {
    fn new() -> Self {
        Self { buffer: vec![], dropping: false }
    }
    fn push(&mut self, chunk: &[u8], mut line: impl FnMut(&[u8])) {
        let mut start = 0;
        while start < chunk.len() {
            let newline = chunk[start..].iter().position(|byte| *byte == b'\n').map(|at| start + at);
            let end = newline.unwrap_or(chunk.len());
            if !self.dropping {
                if self.buffer.len() + (end - start) > MAX_RECORD_BYTES {
                    self.buffer.clear();
                    self.dropping = true;
                } else {
                    self.buffer.extend_from_slice(&chunk[start..end]);
                }
            }
            let Some(newline) = newline else { break };
            if !self.dropping && !self.buffer.is_empty() {
                let complete = if self.buffer.last() == Some(&b'\r') { &self.buffer[..self.buffer.len() - 1] } else { &self.buffer[..] };
                line(complete);
            }
            self.buffer.clear();
            self.dropping = false;
            start = newline + 1;
        }
    }
    fn finish(&mut self, mut line: impl FnMut(&[u8])) {
        self.push(b"\n", &mut line);
    }
}

/// SIGTERM to the group, SIGKILL after the grace period unless the group is gone. Run once the leader has
/// exited, the group is whatever the leader started and detached.
#[cfg(unix)]
async fn end_group(group: Option<i32>) {
    let Some(group) = group else { return };
    // SAFETY: plain signal delivery to the process group this process led.
    if unsafe { libc::kill(-group, libc::SIGTERM) } != 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
    while tokio::time::Instant::now() < deadline {
        // SAFETY: signal 0 only reports whether the group still has members.
        if unsafe { libc::kill(-group, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    unsafe { libc::kill(-group, libc::SIGKILL) };
}
#[cfg(not(unix))]
async fn end_group(_: Option<i32>) {}

impl PiProcess {
    pub fn spawn(launch: &Launch) -> Result<(Self, mpsc::UnboundedReceiver<Event>), String> {
        let mut command = Command::new(&launch.binary);
        command.args(&launch.args).current_dir(&launch.cwd);
        // Removed first: the session's own credential, when it has one, is among the variables added after.
        for name in &launch.remove_env {
            command.env_remove(name);
        }
        command.envs(&launch.env);
        // Its own process group, so stopping the session ends the extensions' subprocesses with it.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().map_err(|error| format!("Failed to spawn Pi process: {error}"))?;
        let pid = child.id();
        let group = pid.and_then(|pid| i32::try_from(pid).ok());
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (writer, mut lines) = mpsc::unbounded_channel::<String>();
        let (events, receiver) = mpsc::unbounded_channel();
        let pending: Pending = Default::default();
        let (stop, mut stopping) = watch::channel(false);
        let (done_sender, done) = watch::channel(false);
        let (ended_sender, ended) = watch::channel(false);
        let (exit_sender, exit) = watch::channel::<Option<Option<i32>>>(None);
        let host_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The supervisor owns the child: it reaps it, and on a stop ends the whole group.
        let supervisor_host_closed = host_closed.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = async { let _ = stopping.wait_for(|stop| *stop).await; } => {
                    supervisor_host_closed.store(true, Ordering::SeqCst);
                    #[cfg(unix)]
                    if let Some(group) = group {
                        // SAFETY: plain signal delivery to the process group this process leads.
                        unsafe { libc::kill(-group, libc::SIGTERM) };
                    }
                    match tokio::time::timeout(TERMINATION_GRACE, child.wait()).await {
                        Ok(status) => status.ok(),
                        Err(_) => {
                            #[cfg(unix)]
                            if let Some(group) = group {
                                unsafe { libc::kill(-group, libc::SIGKILL) };
                            }
                            let _ = child.kill().await;
                            child.wait().await.ok()
                        }
                    }
                }
            };
            let _ = exit_sender.send(Some(status.and_then(|status| status.code())));
            end_group(group).await;
            let _ = ended_sender.send(true);
        });
        // Stderr is never logged beyond its length: it can carry credentials or prompt text.
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(size) = stderr.read(&mut buffer).await {
                if size == 0 {
                    break;
                }
                tracing::debug!(stderr_length = size, "pi stderr");
            }
        });
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                // The reader reports the process's end; a broken pipe is only a symptom of it.
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            drop(stdin);
        });
        let reader_events = events.clone();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut framer = Framer::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(size) => framer.push(&buffer[..size], |line| route(line, &reader_events, &reader_pending)),
                }
            }
            framer.finish(|line| route(line, &reader_events, &reader_pending));
            // Claim the closure before anything else can: requests still waiting will never be answered.
            let mut exit = exit;
            let code = tokio::time::timeout(EXIT_DIAGNOSIS, async { exit.wait_for(|exit| exit.is_some()).await.ok().and_then(|exit| *exit) }).await.ok().flatten().flatten();
            let closed_by_host = host_closed.load(Ordering::SeqCst);
            reader_pending.lock().unwrap().close(RpcError::Closed(if closed_by_host { "pi process was stopped".into() } else { Exit { code, closed_by_host }.describe() }));
            let _ = reader_events.send(Event::Closed(Exit { code, closed_by_host }));
            let _ = done_sender.send(true);
        });
        Ok((Self { writer, events, pending, next_id: AtomicU64::new(0), stop, done, ended }, receiver))
    }
    /// Write one JSON record (`send`): fire and forget.
    pub fn send(&self, record: &Value) -> Result<(), RpcError> {
        if let Some(reason) = self.pending.lock().unwrap().closed.clone() {
            return Err(reason);
        }
        self.writer.send(record.to_string()).map_err(|_| RpcError::Closed("pi process was stopped".into()))
    }
    /// A synthetic record queued behind the events Pi has already written, so a check made off to the side
    /// is seen in stream order.
    pub fn inject(&self, record: Value) {
        let _ = self.events.send(Event::Record(record));
    }
    /// Correlated request: assigns an `id`, waits for the matching response and returns its `data`.
    pub async fn request(&self, mut record: Value, timeout: Duration) -> Result<Value, RpcError> {
        let operation = record["type"].as_str().unwrap_or("request").to_owned();
        let id = format!("t3-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        record["id"] = json!(id);
        let (sender, receiver) = oneshot::channel();
        {
            let mut waiting = self.pending.lock().unwrap();
            if let Some(reason) = waiting.closed.clone() {
                return Err(reason);
            }
            waiting.waiters.insert(id.clone(), sender);
        }
        if let Err(error) = self.send(&record) {
            self.pending.lock().unwrap().waiters.remove(&id);
            return Err(error);
        }
        struct Forget<'a>(&'a Pending, String);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap().waiters.remove(&self.1);
            }
        }
        let _forget = Forget(&self.pending, id);
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Closed("pi process was stopped".into())),
            Err(_) => Err(RpcError::Timeout { operation, millis: timeout.as_millis() as u64 }),
        }
    }
    /// `terminate`: fail everything waiting, stop the process group (SIGTERM, grace, SIGKILL) and wait for it.
    pub async fn terminate(&self) {
        self.pending.lock().unwrap().close(RpcError::Closed("pi process was stopped".into()));
        let _ = self.stop.send(true);
        let (mut done, mut ended) = (self.done.clone(), self.ended.clone());
        let _ = done.wait_for(|done| *done).await;
        let _ = ended.wait_for(|ended| *ended).await;
    }
}
impl Drop for PiProcess {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::HANG_BOUND;
    use super::*;
    fn frames(chunks: &[&[u8]]) -> Vec<String> {
        let mut framer = Framer::new();
        let mut out = vec![];
        for chunk in chunks {
            framer.push(chunk, |line| out.push(String::from_utf8_lossy(line).into_owned()));
        }
        framer.finish(|line| out.push(String::from_utf8_lossy(line).into_owned()));
        out
    }
    #[test]
    fn lines_split_on_lf_only_and_drop_a_trailing_cr() {
        assert_eq!(frames(&[b"{\"a\":1}\r\n{\"b\"", b":2}\n\n{\"c\":3}"]), ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]);
        // U+2028 is part of a record, never a separator.
        assert_eq!(frames(&["{\"t\":\"a\u{2028}b\"}\n".as_bytes()]), ["{\"t\":\"a\u{2028}b\"}"]);
    }
    #[test]
    fn a_line_over_the_cap_is_dropped_whole_and_the_next_one_survives() {
        let big = vec![b'x'; MAX_RECORD_BYTES + 10];
        let out = frames(&[&big[..1000], &big[1000..], b"\n{\"ok\":true}\n"]);
        assert_eq!(out, ["{\"ok\":true}"]);
    }
    #[test]
    fn failed_responses_carry_a_bounded_detail() {
        assert_eq!(summarize_error(&json!("nope")), "nope");
        assert_eq!(summarize_error(&Value::Null), "unknown error");
        let long = summarize_error(&json!("x".repeat(500)));
        assert_eq!(long.chars().count(), 201);
        assert!(long.ends_with('…'));
    }
    #[test]
    fn records_route_by_response_id_and_everything_else_is_an_event() {
        let (events, mut received) = mpsc::unbounded_channel();
        let pending: Pending = Default::default();
        let (sender, mut answer) = oneshot::channel();
        pending.lock().unwrap().waiters.insert("t3-0".into(), sender);
        route(br#"{"type":"response","command":"get_state","id":"t3-0","success":true,"data":{"x":1}}"#, &events, &pending);
        assert_eq!(answer.try_recv().unwrap().unwrap(), json!({"x":1}));
        // An id-less acknowledgement, an unknown id and a session event are all events, in order.
        route(br#"{"type":"response","command":"prompt","success":true}"#, &events, &pending);
        route(br#"{"type":"response","command":"x","id":"t3-9","success":true}"#, &events, &pending);
        route(br#"{"type":"agent_start"}"#, &events, &pending);
        route(b"not json", &events, &pending);
        let kinds: Vec<String> = std::iter::from_fn(|| received.try_recv().ok()).map(|event| match event {
            Event::Record(record) => record["command"].as_str().or(record["type"].as_str()).unwrap().to_owned(),
            Event::Closed(_) => "closed".into(),
        }).collect();
        assert_eq!(kinds, ["prompt", "x", "agent_start"]);
        let (sender, mut failed) = oneshot::channel();
        pending.lock().unwrap().waiters.insert("t3-1".into(), sender);
        route(br#"{"type":"response","command":"set_model","id":"t3-1","success":false,"error":"no such model"}"#, &events, &pending);
        assert_eq!(failed.try_recv().unwrap().unwrap_err(), RpcError::Failed { operation: "set_model".into(), detail: "no such model".into(), raw: json!("no such model") });
    }
    /// A credential inherited from the server never reaches the child; the one the session was given does.
    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_variables_are_removed_before_the_session_own_are_added() {
        let directory = tempfile::tempdir().unwrap();
        let script = r#"printf '{"home":"%s","own":"%s"}\n' "${HOME-unset}" "${T3_OWN-unset}""#;
        let launch = |remove_env: Vec<String>, env: HashMap<String, String>| Launch { binary: "/bin/sh".into(), args: vec!["-c".into(), script.into()], cwd: directory.path().into(), env, remove_env };
        for (launch, expected) in [
            (launch(vec!["HOME".into(), "T3_OWN".into()], HashMap::from([("T3_OWN".to_owned(), "session".to_owned())])), json!({"home":"unset","own":"session"})),
            (launch(vec![], HashMap::new()), json!({"home":std::env::var("HOME").unwrap(),"own":"unset"})),
        ] {
            let (_process, mut events) = PiProcess::spawn(&launch).unwrap();
            let record = loop {
                if let Event::Record(record) = events.recv().await.unwrap() {
                    break record;
                }
            };
            assert_eq!(record, expected);
        }
    }
    /// A process that ends on its own reports its exit code, and a stop ends the whole group it led.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exit_reports_its_code_and_a_stop_ends_what_the_process_started() {
        let directory = tempfile::tempdir().unwrap();
        let launch = |script: &str| Launch { binary: "/bin/sh".into(), args: vec!["-c".into(), script.into()], cwd: directory.path().into(), env: HashMap::new(), remove_env: vec![] };
        let (_process, mut events) = PiProcess::spawn(&launch("exit 3")).unwrap();
        let exit = loop {
            if let Event::Closed(exit) = events.recv().await.unwrap() {
                break exit;
            }
        };
        assert_eq!((exit.code, exit.closed_by_host, exit.describe().as_str()), (Some(3), false, "pi process exited with code 3"));

        // A grandchild that outlives its parent in the same group holds the write end of a pipe: it opens it when
        // it starts and the read below ends when it is gone, so the stop is seen without waiting on a clock.
        let witness = directory.path().join("witness.fifo");
        let path = std::ffi::CString::new(witness.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: creating a FIFO at a path of the test's own directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let reader = tokio::task::spawn_blocking({
            let witness = witness.clone();
            move || {
                use std::io::Read;
                let mut sink = vec![];
                std::fs::File::open(witness).unwrap().read_to_end(&mut sink).unwrap();
            }
        });
        let script = format!("trap '' TERM; (trap '' TERM; exec sleep 3600) > '{}' & echo '{{\"type\":\"ready\"}}'; wait", witness.display());
        let (process, mut events) = PiProcess::spawn(&launch(&script)).unwrap();
        loop {
            if let Event::Record(record) = events.recv().await.unwrap() {
                assert_eq!(record["type"], "ready");
                break;
            }
        }
        process.terminate().await;
        let closed = loop {
            if let Event::Closed(exit) = events.recv().await.unwrap() {
                break exit;
            }
        };
        assert!(closed.closed_by_host);
        // The grandchild sleeps for an hour: the pipe closes now only if the stop ended it rather than waited for it.
        tokio::time::timeout(HANG_BOUND, reader).await.expect("the stop must end the process the session started, not wait for it").unwrap();
    }
}
