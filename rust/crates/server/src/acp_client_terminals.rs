//! Owned ACP client terminals. These are command pipes, separate from interactive PTYs.
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use t3_acp::{AcpError, RpcError};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
const MAX_LIVE: usize = 16;
const MAX_UNRELEASED: usize = 64;
const MAX_RETAINED: usize = 32;
const TOTAL_OUTPUT: usize = 16 * 1024 * 1024;
const DEFAULT_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
fn error(message: String) -> AcpError {
    AcpError::Request(RpcError {
        code: -32603,
        message,
        data: None,
    })
}
#[derive(Default)]
struct OutputBuffer {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    truncated: bool,
    limit: usize,
}
impl OutputBuffer {
    fn trim(&mut self, bytes: usize) {
        let mut remaining = bytes.min(self.bytes);
        while remaining > 0 {
            let head = self.chunks.pop_front().unwrap();
            let take = remaining.min(head.len());
            remaining -= take;
            self.bytes -= take;
            if take < head.len() {
                self.chunks.push_front(head[take..].to_vec());
            }
            self.truncated = true;
        }
    }
    fn append(&mut self, chunk: Vec<u8>) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk);
        if self.bytes > self.limit {
            self.trim(self.bytes - self.limit);
        }
    }
    fn text(&self) -> String {
        let bytes = self.chunks.iter().flatten().copied().collect::<Vec<_>>();
        let skip = if self.truncated {
            bytes
                .iter()
                .take_while(|byte| (**byte & 0b1100_0000) == 0b1000_0000)
                .count()
        } else {
            0
        };
        String::from_utf8_lossy(&bytes[skip..]).into_owned()
    }
}
enum Control {
    Kill(oneshot::Sender<()>),
    Dispose,
}
struct Record {
    #[cfg(test)]
    pid: u32,
    #[cfg(test)]
    leader_exit: watch::Receiver<Option<Value>>,
    session: String,
    command_line: String,
    buffer: OutputBuffer,
    exit_status: Option<Value>,
    released: bool,
    control: mpsc::UnboundedSender<Control>,
    exit: watch::Receiver<Option<Value>>,
    disposed: watch::Receiver<bool>,
}
#[derive(Default)]
struct State {
    records: IndexMap<String, Record>,
    next: u64,
    closed: bool,
}
impl State {
    fn append(&mut self, id: &str, chunk: Vec<u8>) {
        if let Some(record) = self.records.get_mut(id) {
            record.buffer.append(chunk);
        }
        let total = self
            .records
            .values()
            .map(|record| record.buffer.bytes)
            .sum::<usize>();
        let mut excess = total.saturating_sub(TOTAL_OUTPUT);
        for record in self.records.values_mut() {
            let before = record.buffer.bytes;
            record.buffer.trim(excess);
            excess = excess.saturating_sub(before - record.buffer.bytes);
            if excess == 0 {
                break;
            }
        }
    }
    fn prune(&mut self) {
        let mut retained = self
            .records
            .values()
            .filter(|record| record.released)
            .map(|record| record.buffer.bytes)
            .sum::<usize>();
        let mut count = self
            .records
            .values()
            .filter(|record| record.released)
            .count();
        let keys = self
            .records
            .iter()
            .filter(|(_, record)| record.released)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for key in keys {
            if count <= MAX_RETAINED && retained <= TOTAL_OUTPUT {
                break;
            }
            let record = self.records.shift_remove(&key).unwrap();
            retained -= record.buffer.bytes;
            count -= 1;
        }
    }
}
struct Inner {
    state: Mutex<State>,
    creation: tokio::sync::Mutex<()>,
    options: Options,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for record in self.state.get_mut().unwrap().records.values() {
            let _ = record.control.send(Control::Dispose);
        }
    }
}
#[derive(Clone)]
pub(crate) struct Terminals(Arc<Inner>);
#[derive(Clone)]
pub(crate) struct Options {
    pub cwd: PathBuf,
    pub environment: IndexMap<String, String>,
    pub shell_commands: bool,
    pub force_kill_after: Duration,
}
impl Options {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            environment: IndexMap::new(),
            shell_commands: false,
            force_kill_after: Duration::from_secs(5),
        }
    }
}
impl Terminals {
    pub(crate) fn new(options: Options) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                next: 1,
                ..Default::default()
            }),
            creation: tokio::sync::Mutex::new(()),
            options,
        }))
    }
    pub(crate) async fn create(
        &self,
        request: &Value,
        session_environment: &IndexMap<String, String>,
    ) -> Result<Value, AcpError> {
        let _creation = self.0.creation.lock().await;
        {
            let state = self.0.state.lock().unwrap();
            if state.closed {
                return Err(AcpError::Closed);
            }
            if state
                .records
                .values()
                .filter(|record| !record.released)
                .count()
                >= MAX_UNRELEASED
            {
                return Err(error(format!(
                    "ACP terminal/create exceeded the limit of {MAX_UNRELEASED} unreleased terminals."
                )));
            }
            if state
                .records
                .values()
                .filter(|record| !record.released && record.exit_status.is_none())
                .count()
                >= MAX_LIVE
            {
                return Err(error(format!(
                    "ACP terminal/create exceeded the limit of {MAX_LIVE} concurrent terminals."
                )));
            }
        }
        let command = request["command"]
            .as_str()
            .ok_or_else(|| error("Missing ACP terminal command.".into()))?;
        let args = request["args"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let mut process = if self.0.options.shell_commands && args.is_empty() {
            #[cfg(unix)]
            let mut process = Command::new("/bin/sh");
            #[cfg(windows)]
            let mut process =
                Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
            #[cfg(unix)]
            process.args(["-c", command]);
            #[cfg(windows)]
            process.args(["/d", "/s", "/c", command]);
            process
        } else {
            let mut process = Command::new(command);
            process.args(&args);
            process
        };
        process.current_dir(
            request["cwd"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.0.options.cwd.clone()),
        );
        process
            .envs(&self.0.options.environment)
            .envs(session_environment);
        for variable in request["env"].as_array().into_iter().flatten() {
            process.env(
                variable["name"].as_str().unwrap(),
                variable["value"].as_str().unwrap(),
            );
        }
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        let mut child = process
            .spawn()
            .map_err(|_| error(format!("Could not start terminal command '{command}'.")))?;
        let pid = child.id().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (control, commands) = mpsc::unbounded_channel();
        let (exit, exit_rx) = watch::channel(None);
        let (disposed, disposed_rx) = watch::channel(false);
        #[cfg(test)]
        let (leader_exit, leader_exit_rx) = watch::channel(None);
        let id = {
            let mut state = self.0.state.lock().unwrap();
            let id = format!("t3-term-{}", state.next);
            state.next += 1;
            state.records.insert(
                id.clone(),
                Record {
                    #[cfg(test)]
                    pid,
                    #[cfg(test)]
                    leader_exit: leader_exit_rx,
                    session: request["sessionId"].as_str().unwrap().into(),
                    command_line: std::iter::once(command.to_owned())
                        .chain(args)
                        .collect::<Vec<_>>()
                        .join(" "),
                    buffer: OutputBuffer {
                        limit: request["outputByteLimit"]
                            .as_f64()
                            .map(|value| value.min(MAX_OUTPUT as f64) as usize)
                            .unwrap_or(DEFAULT_OUTPUT)
                            .min(MAX_OUTPUT),
                        ..Default::default()
                    },
                    exit_status: None,
                    released: false,
                    control,
                    exit: exit_rx,
                    disposed: disposed_rx,
                },
            );
            id
        };
        let state = Arc::downgrade(&self.0);
        let stdout_task = tokio::spawn(pump(stdout, state.clone(), id.clone()));
        let stderr_task = tokio::spawn(pump(stderr, state.clone(), id.clone()));
        tokio::spawn(owner(
            child,
            pid,
            stdin,
            commands,
            stdout_task,
            stderr_task,
            exit,
            disposed,
            state,
            id.clone(),
            self.0.options.force_kill_after,
            #[cfg(test)]
            leader_exit,
        ));
        Ok(json!({"terminalId":id}))
    }
    fn record<R>(
        &self,
        request: &Value,
        operation: &str,
        read: impl FnOnce(&Record) -> R,
    ) -> Result<R, AcpError> {
        let state = self.0.state.lock().unwrap();
        let id = request["terminalId"].as_str().unwrap_or("");
        match state.records.get(id) {
            Some(record)
                if record.session == request["sessionId"].as_str().unwrap_or("")
                    && !record.released =>
            {
                Ok(read(record))
            }
            _ => Err(error(format!(
                "ACP {operation} received an unknown terminal ID '{id}'."
            ))),
        }
    }
    pub(crate) fn output(&self, request: &Value) -> Result<Value, AcpError> {
        self.record(request, "terminal/output", snapshot)
    }
    pub(crate) async fn wait(&self, request: &Value) -> Result<Value, AcpError> {
        let mut receiver = self.record(request, "terminal/wait_for_exit", |record| {
            record.exit.clone()
        })?;
        loop {
            if let Some(status) = receiver.borrow_and_update().clone() {
                return Ok(status);
            }
            receiver.changed().await.map_err(|_| AcpError::Closed)?;
        }
    }
    pub(crate) async fn kill(&self, request: &Value) -> Result<Value, AcpError> {
        let control = self.record(request, "terminal/kill", |record| record.control.clone())?;
        let (reply, result) = oneshot::channel();
        if control.send(Control::Kill(reply)).is_ok() {
            let _ = result.await;
        }
        Ok(json!({}))
    }
    pub(crate) async fn release(&self, request: &Value) -> Result<Value, AcpError> {
        let (control, mut disposed, mut exit) = {
            let mut state = self.0.state.lock().unwrap();
            let id = request["terminalId"].as_str().unwrap_or("");
            let session = request["sessionId"].as_str().unwrap_or("");
            let record = state
                .records
                .get_mut(id)
                .filter(|record| record.session == session && !record.released)
                .ok_or_else(|| {
                    error(format!(
                        "ACP terminal/release received an unknown terminal ID '{id}'."
                    ))
                })?;
            record.released = true;
            (
                record.control.clone(),
                record.disposed.clone(),
                record.exit.clone(),
            )
        };
        let _ = control.send(Control::Dispose);
        while !*disposed.borrow_and_update() {
            disposed.changed().await.map_err(|_| AcpError::Closed)?;
        }
        while exit.borrow_and_update().is_none() {
            exit.changed().await.map_err(|_| AcpError::Closed)?;
        }
        self.0.state.lock().unwrap().prune();
        Ok(json!({}))
    }
    pub(crate) fn read_snapshot(&self, session: &str, id: &str) -> Option<Value> {
        let state = self.0.state.lock().unwrap();
        state
            .records
            .get(id)
            .filter(|record| record.session == session)
            .map(snapshot)
    }
    #[cfg(test)]
    pub(crate) fn owned_terminal(&self, id: &str) -> Option<(u32, watch::Receiver<bool>)> {
        self.0
            .state
            .lock()
            .unwrap()
            .records
            .get(id)
            .map(|record| (record.pid, record.disposed.clone()))
    }
    pub(crate) fn command_line(&self, id: &str) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap()
            .records
            .get(id)
            .map(|record| record.command_line.clone())
    }
    pub(crate) async fn dispose_all(&self) {
        // Disposal remains reusable like the source service. Holding admission prevents
        // a concurrent create from escaping the snapshot being disposed.
        let _creation = self.0.creation.lock().await;
        self.dispose_records().await;
    }
    pub(crate) async fn shutdown(&self) {
        let _creation = self.0.creation.lock().await;
        self.0.state.lock().unwrap().closed = true;
        self.dispose_records().await;
    }
    async fn dispose_records(&self) {
        let records = {
            let mut state = self.0.state.lock().unwrap();
            state
                .records
                .values_mut()
                .map(|record| {
                    record.released = true;
                    (
                        record.control.clone(),
                        record.disposed.clone(),
                        record.exit.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        futures_util::future::join_all(records.into_iter().map(
            |(control, mut disposed, mut exit)| async move {
                let _ = control.send(Control::Dispose);
                while !*disposed.borrow_and_update() {
                    if disposed.changed().await.is_err() {
                        break;
                    }
                }
                while exit.borrow_and_update().is_none() {
                    if exit.changed().await.is_err() {
                        break;
                    }
                }
            },
        ))
        .await;
        self.0.state.lock().unwrap().prune();
    }
    pub(crate) fn resolve_content(&self, notification: &Value) -> Value {
        let update = &notification["update"];
        if !matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            return notification.clone();
        }
        let Some(content) = update["content"]
            .as_array()
            .filter(|content| content.iter().any(|entry| entry["type"] == "terminal"))
        else {
            return notification.clone();
        };
        let mapped=content.iter().map(|entry|if entry["type"]=="terminal" {let id=entry["terminalId"].as_str().unwrap();let snapshot=self.read_snapshot(notification["sessionId"].as_str().unwrap(),id);json!({"type":"content","content":{"type":"text","text":snapshot.map(|value|value["output"].clone()).unwrap_or_else(||json!(format!("[terminal {id}]")))}})}else{entry.clone()}).collect::<Vec<_>>();
        let mut notification = notification.clone();
        notification["update"]["content"] = json!(mapped);
        notification
    }
}
fn snapshot(record: &Record) -> Value {
    let mut value = json!({"output":record.buffer.text(),"truncated":record.buffer.truncated});
    if let Some(status) = &record.exit_status {
        value["exitStatus"] = status.clone();
    }
    value
}
async fn pump<R: AsyncRead + Unpin>(mut stream: R, state: Weak<Inner>, id: String) {
    let mut buffer = [0; 8192];
    loop {
        let size = match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(size) => size,
        };
        let Some(state) = state.upgrade() else {
            break;
        };
        state
            .state
            .lock()
            .unwrap()
            .append(&id, buffer[..size].to_vec());
    }
}
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    unsafe { libc::kill(-(pid as i32), 0) == 0 }
}
#[cfg(windows)]
fn group_alive(_pid: u32) -> bool {
    false
}
async fn signal(child: &mut Child, pid: u32, force: bool) -> bool {
    #[cfg(unix)]
    {
        if unsafe {
            libc::kill(
                -(pid as i32),
                if force { libc::SIGKILL } else { libc::SIGTERM },
            )
        } == 0
        {
            return true;
        }
        child.start_kill().is_ok()
    }
    #[cfg(windows)]
    {
        let _ = child;
        let _ = force;
        Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok()
    }
}
fn exit_status(status: std::io::Result<std::process::ExitStatus>) -> Value {
    match status {
        Ok(status) => {
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map(|signal| match signal {
                    libc::SIGTERM => "SIGTERM".into(),
                    libc::SIGKILL => "SIGKILL".into(),
                    libc::SIGINT => "SIGINT".into(),
                    libc::SIGHUP => "SIGHUP".into(),
                    libc::SIGPIPE => "SIGPIPE".into(),
                    libc::SIGUSR1 => "SIGUSR1".into(),
                    libc::SIGUSR2 => "SIGUSR2".into(),
                    libc::SIGABRT => "SIGABRT".into(),
                    libc::SIGALRM => "SIGALRM".into(),
                    libc::SIGBUS => "SIGBUS".into(),
                    libc::SIGCHLD => "SIGCHLD".into(),
                    libc::SIGCONT => "SIGCONT".into(),
                    libc::SIGFPE => "SIGFPE".into(),
                    libc::SIGILL => "SIGILL".into(),
                    libc::SIGQUIT => "SIGQUIT".into(),
                    libc::SIGSEGV => "SIGSEGV".into(),
                    libc::SIGSTOP => "SIGSTOP".into(),
                    libc::SIGTRAP => "SIGTRAP".into(),
                    libc::SIGTSTP => "SIGTSTP".into(),
                    libc::SIGTTIN => "SIGTTIN".into(),
                    libc::SIGTTOU => "SIGTTOU".into(),
                    libc::SIGURG => "SIGURG".into(),
                    libc::SIGVTALRM => "SIGVTALRM".into(),
                    libc::SIGXCPU => "SIGXCPU".into(),
                    libc::SIGXFSZ => "SIGXFSZ".into(),
                    libc::SIGPROF => "SIGPROF".into(),
                    libc::SIGSYS => "SIGSYS".into(),
                    libc::SIGWINCH => "SIGWINCH".into(),
                    libc::SIGIO => "SIGIO".into(),
                    other => format!("SIG{other}"),
                })
            };
            #[cfg(windows)]
            let signal: Option<String> = None;
            json!({"exitCode":status.code(),"signal":signal})
        }
        Err(_) => json!({"exitCode":null,"signal":null}),
    }
}
#[allow(clippy::too_many_arguments)]
async fn owner(
    mut child: Child,
    pid: u32,
    mut stdin: Option<tokio::process::ChildStdin>,
    mut controls: mpsc::UnboundedReceiver<Control>,
    stdout: tokio::task::JoinHandle<()>,
    stderr: tokio::task::JoinHandle<()>,
    exit: watch::Sender<Option<Value>>,
    disposed: watch::Sender<bool>,
    state: Weak<Inner>,
    id: String,
    force_after: Duration,
    #[cfg(test)] leader_exit: watch::Sender<Option<Value>>,
) {
    let mut pumps = Box::pin(async {
        let _ = tokio::join!(stdout, stderr);
    });
    let mut pumps_done = false;
    let mut status: Option<Value> = None;
    let mut published = false;
    let mut closing = false;
    let mut controls_open = true;
    let mut force_deadline = None;
    let mut force_sent = false;
    let mut kill_waiters = Vec::<(Instant, oneshot::Sender<()>)>::new();
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if !published && pumps_done {
            if let Some(status) = &status {
                if let Some(state) = state.upgrade() {
                    if let Some(record) = state.state.lock().unwrap().records.get_mut(&id) {
                        record.exit_status = Some(status.clone());
                    }
                }
                exit.send_replace(Some(status.clone()));
                published = true;
            }
        }
        let now = Instant::now();
        // Once the leader is reaped and both inherited pipes reached EOF, there
        // is no remaining owned process evidence. Never probe/signal a retained
        // numeric PGID later: the OS may have reassigned it.
        let owned = status.is_none() || !pumps_done;
        let alive = status.is_none() || (owned && group_alive(pid));
        let mut pending = Vec::new();
        for (deadline, reply) in kill_waiters.drain(..) {
            if status.is_some() && (!alive || now >= deadline) {
                let _ = reply.send(());
            } else {
                pending.push((deadline, reply));
            }
        }
        kill_waiters = pending;
        if closing
            && status.is_some()
            && pumps_done
            && (!alive || (force_sent && force_deadline.is_none()))
        {
            break;
        }
        if closing && force_deadline.is_some_and(|deadline| now >= deadline) && alive {
            if !force_sent {
                let _ = signal(&mut child, pid, true).await;
                force_sent = true;
                force_deadline = Some(now + Duration::from_secs(1));
            } else {
                force_deadline = None;
            }
        }
        tokio::select! {
            result=child.wait(),if status.is_none()=>{stdin.take();let result=exit_status(result);if !pumps_done && result["exitCode"].as_i64().is_some_and(|code|code!=0){let _=signal(&mut child,pid,false).await;}status=Some(result);
                #[cfg(test)]
                leader_exit.send_replace(status.clone());
            },
            _=&mut pumps,if !pumps_done=>{pumps_done=true;},
            command=controls.recv(),if controls_open=>match command {
                Some(Control::Kill(reply))=>{if owned && signal(&mut child,pid,false).await{kill_waiters.push((Instant::now()+Duration::from_secs(1),reply));}else{let _=reply.send(());}},
                Some(Control::Dispose)|None=>{if !closing{closing=true;if owned {let _=signal(&mut child,pid,false).await;force_deadline=Some(Instant::now()+force_after);}}if command.is_none(){controls_open=false;}},
            },
            _=interval.tick(),if closing||!kill_waiters.is_empty()=>{},
        }
    }
    for (_, reply) in kill_waiters {
        let _ = reply.send(());
    }
    disposed.send_replace(true);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures_util::{future::join_all, poll};
    use std::task::Poll;

    fn manager() -> Terminals {
        Terminals::new(Options::new(std::env::current_dir().unwrap()))
    }
    async fn create(manager: &Terminals, command: &str, args: &[&str]) -> Value {
        manager
            .create(
                &json!({"sessionId":"s","command":command,"args":args}),
                &IndexMap::new(),
            )
            .await
            .unwrap()
    }
    fn request(created: &Value) -> Value {
        json!({"sessionId":"s","terminalId":created["terminalId"]})
    }
    fn captured_pid(terminals: &Terminals, created: &Value) -> u32 {
        terminals.0.state.lock().unwrap().records[created["terminalId"].as_str().unwrap()].pid
    }
    fn assert_reaped(pid: u32) {
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "owned child {pid} still alive"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(15), future)
            .await
            .expect("owned terminal milestone")
    }
    #[tokio::test]
    async fn buffers_output_and_actual_exit_status_with_session_environment() {
        bounded(async {
            let terminals=manager();
            let env=IndexMap::from([("T3_ACP_TEST".into(),"session-value".into())]);
            let created=terminals.create(&json!({"sessionId":"s","command":"/bin/sh","args":["-c","printf '%s' \"$T3_ACP_TEST\"; printf error >&2; exit 3"]}),&env).await.unwrap();
            let request=request(&created);
            assert_eq!(terminals.wait(&request).await.unwrap(),json!({"exitCode":3,"signal":null}));
            let output=terminals.output(&request).unwrap();
            let text=output["output"].as_str().unwrap();
            assert!(text.contains("session-value")&&text.contains("error"));
            assert_eq!(output["truncated"],false);
            terminals.shutdown().await;
        }).await;
    }
    #[tokio::test]
    async fn command_only_shell_source_and_request_environment_override_session() {
        bounded(async {
            let mut options=Options::new(std::env::current_dir().unwrap());options.shell_commands=true;
            let terminals=Terminals::new(options);
            let env=IndexMap::from([("T3_ACP_TEST".into(),"session".into())]);
            let created=terminals.create(&json!({"sessionId":"s","command":"printf '%s' \"$T3_ACP_TEST\" | tr a-z A-Z","env":[{"name":"T3_ACP_TEST","value":"request"}]}),&env).await.unwrap();
            let request=request(&created);terminals.wait(&request).await.unwrap();
            assert_eq!(terminals.output(&request).unwrap()["output"],"REQUEST");
            terminals.shutdown().await;
        }).await;
    }
    #[tokio::test]
    async fn retains_utf8_tail_at_byte_limit_and_rewrites_embedded_content() {
        bounded(async {
            let terminals=manager();
            let created=terminals.create(&json!({"sessionId":"s","command":"/bin/echo","args":["prefix😀TAIL"],"outputByteLimit":7.0}),&IndexMap::new()).await.unwrap();
            let req=request(&created);terminals.wait(&req).await.unwrap();
            let output=terminals.output(&req).unwrap();assert_eq!(output["output"],"TAIL\n");assert_eq!(output["truncated"],true);
            terminals.release(&req).await.unwrap();
            let value=json!({"sessionId":"s","update":{"sessionUpdate":"tool_call_update","content":[{"type":"terminal","terminalId":created["terminalId"]},{"type":"terminal","terminalId":"unknown"},{"type":"content","content":{"type":"text","text":"original"}}]}});
            let resolved=terminals.resolve_content(&value);
            assert_eq!(resolved["update"]["content"][0]["content"]["text"],"TAIL\n");
            assert_eq!(resolved["update"]["content"][1]["content"]["text"],"[terminal unknown]");
            assert_eq!(resolved["update"]["content"][2],value["update"]["content"][2]);
            terminals.shutdown().await;
        }).await;
    }
    #[tokio::test]
    async fn kill_reports_named_signal_and_release_rejects_cross_session_handles() {
        bounded(async {
            let terminals = manager();
            let created = create(&terminals, "/bin/cat", &[]).await;
            let req = request(&created);
            assert!(
                terminals
                    .output(&json!({"sessionId":"other","terminalId":created["terminalId"]}))
                    .is_err()
            );
            terminals.kill(&req).await.unwrap();
            assert_eq!(terminals.wait(&req).await.unwrap()["signal"], "SIGTERM");
            terminals.release(&req).await.unwrap();
            assert!(terminals.output(&req).is_err());
            assert!(
                terminals
                    .read_snapshot("s", created["terminalId"].as_str().unwrap())
                    .is_some()
            );
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn reserves_sixteen_live_commands_across_concurrent_creates() {
        bounded(async {
            let terminals = manager();
            let req = json!({"sessionId":"s","command":"/bin/cat"});
            let env = IndexMap::new();
            let results = join_all((0..17).map(|_| terminals.create(&req, &env))).await;
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 16);
            assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn retains_only_latest_thirty_two_released_snapshots() {
        bounded(async {
            let terminals = manager();
            let mut ids = Vec::new();
            for _ in 0..33 {
                let created = create(&terminals, "/bin/echo", &["value"]).await;
                let req = request(&created);
                terminals.wait(&req).await.unwrap();
                terminals.release(&req).await.unwrap();
                ids.push(created["terminalId"].as_str().unwrap().to_owned());
            }
            assert!(terminals.read_snapshot("s", &ids[0]).is_none());
            assert_eq!(
                terminals.read_snapshot("s", ids.last().unwrap()).unwrap()["output"],
                "value\n"
            );
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn unreleased_completed_handles_bound_sixty_four_without_invalidating_existing() {
        bounded(async {
            let terminals = manager();
            let mut ids = Vec::new();
            for _ in 0..64 {
                let created = create(&terminals, "/usr/bin/true", &[]).await;
                terminals.wait(&request(&created)).await.unwrap();
                ids.push(created);
            }
            assert!(
                terminals
                    .create(
                        &json!({"sessionId":"s","command":"/usr/bin/true"}),
                        &IndexMap::new()
                    )
                    .await
                    .is_err()
            );
            assert!(terminals.output(&request(&ids[0])).is_ok());
            assert!(terminals.output(&request(ids.last().unwrap())).is_ok());
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn release_validation_and_marking_are_atomic_during_disposal() {
        bounded(async {
            let terminals = manager();
            let created = create(&terminals, "/bin/cat", &[]).await;
            let req = request(&created);
            let mut first = Box::pin(terminals.release(&req));
            assert!(matches!(poll!(&mut first), Poll::Pending));
            assert!(terminals.release(&req).await.is_err());
            let (released, ()) = tokio::join!(first, terminals.dispose_all());
            assert!(released.is_ok());
            assert!(
                terminals
                    .read_snapshot("s", created["terminalId"].as_str().unwrap())
                    .unwrap()["exitStatus"]
                    .is_object()
            );
            // dispose_all is a reusable source operation, unlike scope shutdown.
            let fresh = create(&terminals, "/usr/bin/true", &[]).await;
            terminals.wait(&request(&fresh)).await.unwrap();
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn shutdown_admission_barrier_rejects_queued_create_and_reaps_existing_owner() {
        bounded(async {
            let terminals = manager();
            let created = create(&terminals, "/bin/cat", &[]).await;
            let pid = captured_pid(&terminals, &created);
            let admission = terminals.0.creation.lock().await;
            let mut shutdown = Box::pin(terminals.shutdown());
            assert!(matches!(poll!(&mut shutdown), Poll::Pending));
            let env = IndexMap::new();
            let req = json!({"sessionId":"s","command":"/bin/cat"});
            let mut queued = Box::pin(terminals.create(&req, &env));
            assert!(matches!(poll!(&mut queued), Poll::Pending));
            drop(admission);
            let ((), result) = tokio::join!(shutdown, queued);
            assert!(matches!(result, Err(AcpError::Closed)));
            let snapshot = terminals
                .read_snapshot("s", created["terminalId"].as_str().unwrap())
                .unwrap();
            assert_eq!(snapshot["exitStatus"]["signal"], "SIGTERM");
            assert_eq!(terminals.0.state.lock().unwrap().next, 2);
            assert_reaped(pid);
        })
        .await;
    }
    #[tokio::test]
    async fn accepted_create_is_in_shutdown_snapshot_when_it_wins_admission() {
        bounded(async {
            let terminals = manager();
            let admission = terminals.0.creation.lock().await;
            let env = IndexMap::new();
            let req = json!({"sessionId":"s","command":"/bin/cat"});
            let mut create = Box::pin(terminals.create(&req, &env));
            assert!(matches!(poll!(&mut create), Poll::Pending));
            let mut shutdown = Box::pin(terminals.shutdown());
            assert!(matches!(poll!(&mut shutdown), Poll::Pending));
            drop(admission);
            let (created, ()) = tokio::join!(create, shutdown);
            let created = created.unwrap();
            assert_reaped(captured_pid(&terminals, &created));
            assert_eq!(
                terminals
                    .read_snapshot("s", created["terminalId"].as_str().unwrap())
                    .unwrap()["exitStatus"]["signal"],
                "SIGTERM"
            );
            assert!(terminals.output(&request(&created)).is_err());
        })
        .await;
    }
    #[tokio::test]
    async fn signal_status_uses_os_name_not_platform_number() {
        bounded(async {
            let terminals = manager();
            let created = create(&terminals, "/bin/sh", &["-c", "kill -USR1 $$"]).await;
            assert_eq!(
                terminals.wait(&request(&created)).await.unwrap()["signal"],
                "SIGUSR1"
            );
            terminals.shutdown().await;
        })
        .await;
    }
    #[tokio::test]
    async fn aggregate_output_evicts_oldest_bytes_without_invalidating_handles() {
        bounded(async {
            let terminals=manager();let mut ids=Vec::new();
            for _ in 0..3 {
                let created=terminals.create(&json!({"sessionId":"s","command":"/bin/sh","args":["-c","head -c 8388608 /dev/zero"],"outputByteLimit":8388608}),&IndexMap::new()).await.unwrap();
                terminals.wait(&request(&created)).await.unwrap();ids.push(created);
            }
            assert_eq!(terminals.output(&request(&ids[0])).unwrap()["output"],"");
            assert_eq!(terminals.output(&request(&ids[0])).unwrap()["truncated"],true);
            assert_eq!(terminals.output(&request(&ids[1])).unwrap()["output"].as_str().unwrap().len(),MAX_OUTPUT);
            assert_eq!(terminals.output(&request(&ids[2])).unwrap()["output"].as_str().unwrap().len(),MAX_OUTPUT);
            terminals.shutdown().await;
        }).await;
    }
    #[tokio::test]
    async fn release_escalates_owned_term_ignoring_child_and_reaps_before_return() {
        bounded(async {
            let directory=tempfile::tempdir().unwrap();
            let socket_path=directory.path().join("milestone.sock");
            let milestones=tokio::net::UnixDatagram::bind(&socket_path).unwrap();
            let script="import os,signal,socket,sys\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ndef term(_s,_f): s.sendto(b'term',sys.argv[1])\nsignal.signal(signal.SIGTERM,term)\ns.sendto(str(os.getpid()).encode(),sys.argv[1])\nwhile True: signal.pause()";
            let mut options=Options::new(directory.path().to_owned());options.force_kill_after=Duration::from_millis(30);
            let terminals=Terminals::new(options);
            let created=terminals.create(&json!({"sessionId":"s","command":"/usr/bin/python3","args":["-c",script,socket_path]}),&IndexMap::new()).await.unwrap();
            let mut bytes=[0;128];let size=milestones.recv(&mut bytes).await.unwrap();
            let pid=std::str::from_utf8(&bytes[..size]).unwrap().parse::<u32>().unwrap();
            assert_eq!(pid,captured_pid(&terminals,&created));
            let req=request(&created);let mut release=Box::pin(terminals.release(&req));
            assert!(matches!(poll!(&mut release),Poll::Pending));
            let size=milestones.recv(&mut bytes).await.unwrap();assert_eq!(&bytes[..size],b"term");
            release.await.unwrap();assert_reaped(pid);
            assert_eq!(terminals.read_snapshot("s",created["terminalId"].as_str().unwrap()).unwrap()["exitStatus"]["signal"],"SIGKILL");
            terminals.shutdown().await;
        }).await;
    }
    #[tokio::test]
    async fn dropping_last_manager_disposes_and_reaps_owned_running_child() {
        bounded(async {
            let terminals = manager();
            let created = create(&terminals, "/bin/cat", &[]).await;
            let pid = captured_pid(&terminals, &created);
            let mut disposed = terminals.0.state.lock().unwrap().records
                [created["terminalId"].as_str().unwrap()]
            .disposed
            .clone();
            drop(terminals);
            while !*disposed.borrow_and_update() {
                disposed.changed().await.unwrap();
            }
            assert_reaped(pid);
        })
        .await;
    }
    #[tokio::test]
    async fn release_cleans_known_descendant_pipes_after_successful_leader_exit() {
        bounded(async {
            let directory=tempfile::tempdir().unwrap();let path=directory.path().join("descendant.sock");
            let milestone=tokio::net::UnixDatagram::bind(&path).unwrap();
            let script="import os,signal,socket,sys\nif os.fork(): sys.exit(0)\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ns.sendto(str(os.getpid()).encode(),sys.argv[1])\nwhile True: signal.pause()";
            let terminals=manager();
            let created=terminals.create(&json!({"sessionId":"s","command":"/usr/bin/python3","args":["-c",script,path]}),&IndexMap::new()).await.unwrap();
            let parent=captured_pid(&terminals,&created);let mut bytes=[0;64];let size=milestone.recv(&mut bytes).await.unwrap();let descendant=std::str::from_utf8(&bytes[..size]).unwrap().parse::<u32>().unwrap();
            let mut leader=terminals.0.state.lock().unwrap().records[created["terminalId"].as_str().unwrap()].leader_exit.clone();
            while leader.borrow_and_update().is_none() {leader.changed().await.unwrap();}
            terminals.release(&request(&created)).await.unwrap();assert_reaped(parent);
            // The owned inherited pipes reached EOF only after group cleanup.
            // Orphan zombie reaping is the host's responsibility; signal0 may
            // briefly still see a zombie, so assert group cleanup through EOF.
            assert_ne!(parent,descendant);
            assert_eq!(terminals.read_snapshot("s",created["terminalId"].as_str().unwrap()).unwrap()["exitStatus"]["exitCode"],0);
            terminals.shutdown().await;
        }).await;
    }
}

#[cfg(test)]
mod output_oracle {
    use super::*;
    #[test]
    fn original_terminal_buffer_helpers_match_byte_boundaries_and_malformed_utf8() {
        let mut count = 0;
        for (index, line) in include_str!("../tests/fixtures/acp-client-terminal-buffer.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let mut buffer = OutputBuffer {
                limit: fixture["input"]["limit"].as_u64().unwrap() as usize,
                ..Default::default()
            };
            for chunk in fixture["input"]["chunks"].as_array().unwrap() {
                buffer.append(
                    chunk
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|byte| byte.as_u64().unwrap() as u8)
                        .collect(),
                );
            }
            buffer.trim(fixture["input"]["trim"].as_u64().unwrap() as usize);
            let actual =
                json!({"text":buffer.text(),"bytes":buffer.bytes,"truncated":buffer.truncated});
            assert_eq!(actual, fixture["output"], "original fixture {}", index + 1);
            count += 1;
        }
        assert_eq!(count, 1296);
    }
}
