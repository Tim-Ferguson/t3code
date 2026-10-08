//! Bidirectional provider JSON-lines transport (raw Codex or versioned JSON-RPC).
//! The process belongs to the client lifetime, and response waiters are removed on
//! cancellation. Notification consumers must treat broadcast lag as lost continuity.
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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Semaphore, broadcast, mpsc, oneshot, watch},
};

const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 128;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProcessError {
    #[error("Provider process failed: {0}")]
    Io(String),
    #[error("Provider protocol failed: {0}")]
    Protocol(String),
    #[error("Provider request {method} timed out")]
    Timeout { method: String },
    #[error("Provider request failed ({code}): {message}")]
    Remote {
        code: i64,
        message: String,
        data: Value,
    },
    #[error("Provider process ended: {0}")]
    Closed(String),
    #[error("Provider process ended: {message}")]
    Exited {
        code: Option<i32>,
        pid: Option<u32>,
        message: String,
    },
}
#[derive(Debug, Clone)]
pub enum ProcessEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Closed(ProcessError),
}
#[derive(Debug, Clone)]
pub struct ProcessOptions {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: HashMap<String, String>,
}
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, ProcessError>>>>>;
struct Outgoing {
    value: Value,
    written: oneshot::Sender<Result<(), ProcessError>>,
}
struct Inner {
    outbound: mpsc::Sender<Outgoing>,
    pending: Pending,
    next_id: AtomicU64,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<ProcessEvent>,
    first_receiver: Mutex<Option<broadcast::Receiver<ProcessEvent>>>,
    shutdown: watch::Sender<bool>,
    closed: Arc<Mutex<Option<ProcessError>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}
#[derive(Clone)]
pub struct ProviderProcess(Arc<Inner>);
struct PendingGuard {
    id: u64,
    pending: Pending,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

impl ProviderProcess {
    pub fn spawn(options: ProcessOptions) -> Result<Self, ProcessError> {
        Self::spawn_protocol(options, false)
    }
    /// ACP uses versioned JSON-RPC envelopes; Codex's raw envelopes retain their
    /// existing encoding through `spawn`. Both modes share owned process cleanup.
    pub fn spawn_json_rpc(options: ProcessOptions) -> Result<Self, ProcessError> {
        Self::spawn_protocol(options, true)
    }
    fn spawn_protocol(options: ProcessOptions, json_rpc: bool) -> Result<Self, ProcessError> {
        let mut child = Command::new(options.binary)
            .args(options.args)
            .current_dir(options.cwd)
            .envs(options.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        let mut stdin = child.stdin.take().unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (outbound, mut outgoing) = mpsc::channel::<Outgoing>(64);
        let (events, first_receiver) = broadcast::channel(1024);
        let (shutdown, mut stopping) = watch::channel(false);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(Mutex::new(None));
        let inner = Arc::new(Inner {
            outbound,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            permits: Arc::new(Semaphore::new(MAX_PENDING)),
            events: events.clone(),
            first_receiver: Mutex::new(Some(first_receiver)),
            shutdown,
            closed: closed.clone(),
        });
        let (writer_error, mut errors) = mpsc::channel::<ProcessError>(1);
        tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(Outgoing { mut value, written }) = outgoing.recv().await {
                    if json_rpc {
                        value["jsonrpc"] = json!("2.0");
                    }
                    let result = async {
                        let mut bytes = serde_json::to_vec(&value)
                            .map_err(|e| ProcessError::Protocol(e.to_string()))?;
                        if bytes.len() > MAX_LINE_BYTES {
                            return Err(ProcessError::Protocol(
                                "outgoing message exceeds byte budget".into(),
                            ));
                        }
                        bytes.push(b'\n');
                        stdin
                            .write_all(&bytes)
                            .await
                            .map_err(|e| ProcessError::Io(e.to_string()))?;
                        stdin
                            .flush()
                            .await
                            .map_err(|e| ProcessError::Io(e.to_string()))
                    }
                    .await;
                    let _ = written.send(result.clone());
                    if let Err(error) = result {
                        let _ = writer_error.send(error).await;
                        break;
                    }
                }
            });
            let diagnostic = Arc::new(Mutex::new(Vec::<u8>::new()));
            let diagnostic_writer = diagnostic.clone();
            let stderr_task = tokio::spawn(async move {
                let mut buffer = [0; 1024];
                while let Ok(size) = stderr.read(&mut buffer).await {
                    if size == 0 {
                        break;
                    }
                    let mut tail = diagnostic_writer.lock().unwrap();
                    tail.extend_from_slice(&buffer[..size]);
                    let extra = tail.len().saturating_sub(8192);
                    tail.drain(..extra);
                }
            });
            let mut reader = BufReader::new(stdout);
            let failure = loop {
                tokio::select! {
                    _ = stopping.changed() => { let _=child.kill().await; break ProcessError::Closed("client was released".into()); }
                    Some(error) = errors.recv() => break error,
                    line = bounded_line(&mut reader) => {
                        match line {
                            Ok(Some(line)) => {
                                let value = match serde_json::from_slice::<Value>(&line) { Ok(value) => value, Err(error) => break ProcessError::Protocol(error.to_string()) };
                                if json_rpc && value["jsonrpc"]!="2.0" {break ProcessError::Protocol("Expected JSON-RPC version 2.0.".into());}
                                if let Err(error) = dispatch_message(value, &pending, &events, json_rpc) { break error; }
                            }
                            Ok(None) => {
                                // EOF may precede process exit; never wait indefinitely for an
                                // executable that closed stdout but continues running.
                                let status = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
                                let details = String::from_utf8_lossy(&diagnostic.lock().unwrap()).into_owned();
                                let code = status.as_ref().ok().and_then(|status|status.as_ref().ok()).and_then(|status|status.code());
                                break ProcessError::Exited{code,pid,message:format!("{status:?} {details}")};
                            }
                            Err(error) => break error,
                        }
                    }
                }
            };
            *closed.lock().unwrap() = Some(failure.clone());
            for (_, waiter) in pending.lock().unwrap().drain() {
                let _ = waiter.send(Err(failure.clone()));
            }
            writer.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            let _ = events.send(ProcessEvent::Closed(failure));
        });
        Ok(Self(inner))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProcessEvent> {
        if let Some(receiver) = self.0.first_receiver.lock().unwrap().take() {
            return receiver;
        }
        let closed = self.0.closed.lock().unwrap();
        if let Some(error) = closed.as_ref() {
            let (sender, receiver) = broadcast::channel(1);
            let _ = sender.send(ProcessEvent::Closed(error.clone()));
            receiver
        } else {
            self.0.events.subscribe()
        }
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProcessError> {
        let operation = async {
            let _permit = self
                .0
                .permits
                .acquire()
                .await
                .map_err(|_| ProcessError::Closed("client closed".into()))?;
            // Closed and pending are checked under the same ordering as process
            // teardown so a request cannot get stranded after waiters are drained.
            let (id, response) = {
                let closed = self.0.closed.lock().unwrap();
                if let Some(error) = &*closed {
                    return Err(error.clone());
                }
                let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
                let (waiter, response) = oneshot::channel();
                self.0.pending.lock().unwrap().insert(id, waiter);
                (id, response)
            };
            let _guard = PendingGuard {
                id,
                pending: self.0.pending.clone(),
            };
            self.send(json!({"id":id,"method":method,"params":params}))
                .await?;
            response
                .await
                .map_err(|_| ProcessError::Closed("response channel closed".into()))?
        };
        tokio::time::timeout(timeout, operation)
            .await
            .map_err(|_| ProcessError::Timeout {
                method: method.into(),
            })?
    }
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), ProcessError> {
        let mut value = json!({"method":method});
        if let Some(params) = params {
            value["params"] = params;
        }
        self.send(value).await
    }
    pub async fn respond(
        &self,
        id: Value,
        result: Result<Value, ProcessError>,
    ) -> Result<(), ProcessError> {
        let value = match result {
            Ok(result) => json!({"id":id,"result":result}),
            Err(ProcessError::Remote {
                code,
                message,
                data,
            }) => json!({"id":id,"error":{"code":code,"message":message,"data":data}}),
            Err(error) => json!({"id":id,"error":{"code":-32603,"message":error.to_string()}}),
        };
        self.send(value).await
    }
    async fn send(&self, value: Value) -> Result<(), ProcessError> {
        if let Some(error) = self.0.closed.lock().unwrap().clone() {
            return Err(error);
        }
        let (written, acknowledgement) = oneshot::channel();
        self.0
            .outbound
            .send(Outgoing { value, written })
            .await
            .map_err(|_| self.writer_closed())?;
        acknowledgement.await.map_err(|_| self.writer_closed())?
    }
    fn writer_closed(&self) -> ProcessError {
        self.0
            .closed
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| ProcessError::Closed("stdin writer closed".into()))
    }
}

async fn bounded_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, ProcessError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(ProcessError::Protocol("truncated JSON line".into()))
            };
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1);
        let size = end.unwrap_or(buffer.len());
        if line.len() + size > MAX_LINE_BYTES {
            return Err(ProcessError::Protocol(
                "incoming message exceeds byte budget".into(),
            ));
        }
        line.extend_from_slice(&buffer[..size]);
        reader.consume(size);
        if end.is_some() {
            return Ok(Some(line));
        }
    }
}
fn safe_rpc_error_code(value: &Value) -> Option<i64> {
    const MAX: i64 = (1i64 << 53) - 1;
    if let Some(value) = value.as_i64() {
        return (-MAX..=MAX).contains(&value).then_some(value);
    }
    value
        .as_f64()
        .filter(|value| value.fract() == 0.0 && value.abs() <= MAX as f64)
        .map(|value| value as i64)
}
fn dispatch_message(
    value: Value,
    pending: &Pending,
    events: &broadcast::Sender<ProcessEvent>,
    json_rpc: bool,
) -> Result<(), ProcessError> {
    if json_rpc {
        let object = value
            .as_object()
            .ok_or_else(|| ProcessError::Protocol("JSON-RPC envelope must be an object".into()))?;
        let has_method = object.contains_key("method");
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");
        if has_method {
            if !value["method"].is_string() || has_result || has_error {
                return Err(ProcessError::Protocol(
                    "invalid JSON-RPC request envelope".into(),
                ));
            }
        } else {
            if has_result == has_error || !value["id"].is_string() && !value["id"].is_number() {
                return Err(ProcessError::Protocol(
                    "invalid JSON-RPC response envelope".into(),
                ));
            }
            if has_error
                && (!value["error"].is_object()
                    || safe_rpc_error_code(&value["error"]["code"]).is_none()
                    || !value["error"]["message"].is_string())
            {
                return Err(ProcessError::Protocol(
                    "invalid JSON-RPC error envelope".into(),
                ));
            }
        }
    }
    if let Some(method) = value["method"].as_str() {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let event = match value.get("id") {
            Some(id) if id.is_string() || id.is_number() => ProcessEvent::Request {
                id: id.clone(),
                method: method.into(),
                params,
            },
            Some(_) => return Err(ProcessError::Protocol("invalid server request id".into())),
            None => ProcessEvent::Notification {
                method: method.into(),
                params,
            },
        };
        let _ = events.send(event);
        return Ok(());
    }
    let id = value["id"].as_u64().or_else(|| {
        if json_rpc {
            value["id"]
                .as_f64()
                .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= (1u64 << 53) as f64)
                .map(|n| n as u64)
        } else {
            value["id"].as_str().and_then(|id| id.parse().ok())
        }
    });
    // String IDs are valid ACP envelopes but cannot match our numeric outgoing
    // IDs. Raw Codex retains its historical numeric-string compatibility.
    let Some(id) = id else {
        return if json_rpc {
            Ok(())
        } else {
            Err(ProcessError::Protocol("response has no request id".into()))
        };
    };
    let result = if let Some(error) = value.get("error") {
        Err(ProcessError::Remote {
            code: if json_rpc {
                safe_rpc_error_code(&error["code"]).unwrap()
            } else {
                error["code"].as_i64().unwrap_or(-32603)
            },
            message: error["message"]
                .as_str()
                .unwrap_or("Provider request failed")
                .into(),
            data: error.get("data").cloned().unwrap_or(Value::Null),
        })
    } else {
        Ok(value
            .get("result")
            .cloned()
            .ok_or_else(|| ProcessError::Protocol("response has no result or error".into()))?)
    };
    // Late responses to a timed-out/cancelled request are intentionally ignored.
    if let Some(waiter) = pending.lock().unwrap().remove(&id) {
        let _ = waiter.send(result);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(body: &str) -> (tempfile::TempDir, ProcessOptions) {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("fake-provider.py");
        std::fs::write(&script, format!("import sys,json,time\n{body}\n")).unwrap();
        (
            directory,
            ProcessOptions {
                binary: PathBuf::from("python3"),
                args: vec![script.to_string_lossy().into()],
                cwd: std::env::temp_dir(),
                environment: HashMap::new(),
            },
        )
    }
    #[tokio::test]
    async fn concurrent_requests_notifications_and_server_requests_preserve_routing() {
        let (_directory, options) = fixture(
            "requests=[]\nfor line in sys.stdin:\n r=json.loads(line)\n if r.get('method')=='echo':\n  requests.append(r)\n  if len(requests)==2:\n   print(json.dumps({'method':'progress','params':{'text':'hello'}}),flush=True)\n   print(json.dumps({'id':'approval:1','method':'approval','params':{}}),flush=True)\n   for q in reversed(requests): print(json.dumps({'id':str(q['id']),'result':q['params']}),flush=True)\n elif r.get('id')=='approval:1':\n  print(json.dumps({'method':'accepted','params':r['result']}),flush=True)",
        );
        let process = ProviderProcess::spawn(options).unwrap();
        let mut events = process.subscribe();
        let (first, second) = tokio::join!(
            process.request("echo", json!({"text":"first"}), Duration::from_secs(2)),
            process.request("echo", json!({"text":"second"}), Duration::from_secs(2))
        );
        assert_eq!(first.unwrap()["text"], "first");
        assert_eq!(second.unwrap()["text"], "second");
        assert!(
            matches!(events.recv().await.unwrap(),ProcessEvent::Notification {method,..} if method=="progress")
        );
        let ProcessEvent::Request { id, .. } = events.recv().await.unwrap() else {
            panic!("missing server request")
        };
        process
            .respond(id, Ok(json!({"decision":"accept"})))
            .await
            .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),ProcessEvent::Notification {method,params} if method=="accepted"&&params["decision"]=="accept")
        );
    }
    #[tokio::test]
    async fn versioned_json_rpc_requests_notifications_and_numeric_callbacks_share_owned_transport()
    {
        let (_directory, options) = fixture(
            "for line in sys.stdin:\n r=json.loads(line)\n assert r['jsonrpc']=='2.0'\n if r.get('method')=='initialize':\n  print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{'ready':True}}),flush=True)\n elif r.get('method')=='initialized':\n  print(json.dumps({'jsonrpc':'2.0','id':0,'method':'approval','params':{}}),flush=True)\n elif r.get('id')==0:\n  assert type(r['id']) is int\n  print(json.dumps({'jsonrpc':'2.0','method':'answered','params':r['result']}),flush=True)",
        );
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        let mut events = process.subscribe();
        assert_eq!(
            process
                .request("initialize", json!({}), Duration::from_secs(2))
                .await
                .unwrap()["ready"],
            true
        );
        process
            .notify("initialized", Some(json!({})))
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        let ProcessEvent::Request { id, .. } = request else {
            panic!("missing callback");
        };
        assert_eq!(id, json!(0));
        process
            .respond(id, Ok(json!({"approved":true})))
            .await
            .unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2),events.recv()).await.unwrap().unwrap(),ProcessEvent::Notification{method,params} if method=="answered" && params["approved"]==true)
        );
        drop(process);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            ProcessEvent::Closed(_)
        ));
    }
    #[tokio::test]
    async fn versioned_transport_rejects_missing_or_other_json_rpc_versions() {
        for version in ["None", "'1.0'"] {
            let (_directory, options) = fixture(&format!(
                "r=json.loads(sys.stdin.readline())\nprint(json.dumps({{'jsonrpc':{version},'id':r['id'],'result':{{}}}}),flush=True)"
            ));
            let process = ProviderProcess::spawn_json_rpc(options).unwrap();
            assert!(
                matches!(process.request("initialize",json!({}),Duration::from_secs(2)).await,Err(ProcessError::Protocol(message)) if message.contains("version 2.0"))
            );
        }
    }
    #[tokio::test]
    async fn versioned_response_string_id_cannot_complete_numeric_request() {
        let (_directory, options) = fixture(
            "r=json.loads(sys.stdin.readline())\nprint(json.dumps({'jsonrpc':'2.0','id':str(r['id']),'result':'wrong'}),flush=True)\nprint(json.dumps({'jsonrpc':'2.0','method':'string-sent','params':{}}),flush=True)\ngate=json.loads(sys.stdin.readline())\nassert gate['method']=='release'\nprint(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':'right'}),flush=True)",
        );
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        let mut events = process.subscribe();
        let clone = process.clone();
        let request = tokio::spawn(async move {
            clone
                .request("probe", Value::Null, Duration::from_secs(2))
                .await
        });
        assert!(
            matches!(events.recv().await.unwrap(),ProcessEvent::Notification{method,..}if method=="string-sent")
        );
        assert!(!request.is_finished());
        assert_eq!(process.0.pending.lock().unwrap().len(), 1);
        process.notify("release", Some(json!({}))).await.unwrap();
        assert_eq!(request.await.unwrap().unwrap(), "right");
    }
    #[tokio::test]
    async fn versioned_transport_rejects_ambiguous_or_malformed_error_envelopes() {
        for tail in [
            "'result':None,'error':{'code':1,'message':'bad'}",
            "'error':None",
            "'error':{'code':'1','message':'bad'}",
            "'error':{'code':1,'message':9}",
            "'method':False,'result':None",
        ] {
            let (_directory, options) = fixture(&format!(
                "r=json.loads(sys.stdin.readline())\nprint(json.dumps({{'jsonrpc':'2.0','id':r['id'],{tail}}}),flush=True)"
            ));
            let process = ProviderProcess::spawn_json_rpc(options).unwrap();
            assert!(
                matches!(
                    process
                        .request("probe", Value::Null, Duration::from_secs(2))
                        .await,
                    Err(ProcessError::Protocol(_))
                ),
                "{tail}"
            );
        }
        let (_directory, options) = fixture(
            "r=json.loads(sys.stdin.readline())\nprint(json.dumps({'jsonrpc':'2.0','id':r['id'],'error':{'code':-32000,'message':'detail','data':{'why':'retained'}}}),flush=True)",
        );
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        assert!(
            matches!(process.request("probe",Value::Null,Duration::from_secs(2)).await,Err(ProcessError::Remote{code:-32000,message,data}) if message=="detail"&&data["why"]=="retained")
        );
    }
    #[tokio::test]
    async fn notification_acknowledges_writer_validation_and_process_exit_keeps_identity() {
        let (_directory, options) = fixture("for line in sys.stdin: pass");
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        assert!(
            matches!(process.notify("oversized",Some(json!("x".repeat(MAX_LINE_BYTES)))).await,Err(ProcessError::Protocol(message))if message.contains("outgoing message"))
        );
        let (_directory, options) = fixture("sys.stdin.readline()\nsys.exit(9)");
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        let mut events = process.subscribe();
        assert!(matches!(
            process
                .request("exit", Value::Null, Duration::from_secs(2))
                .await,
            Err(ProcessError::Exited {
                code: Some(9),
                pid: Some(_),
                ..
            })
        ));
        assert!(matches!(
            events.recv().await.unwrap(),
            ProcessEvent::Closed(ProcessError::Exited {
                code: Some(9),
                pid: Some(_),
                ..
            })
        ));
    }
    #[tokio::test]
    async fn versioned_numeric_ids_and_error_codes_follow_javascript_safe_integer_semantics() {
        let (_directory, options) = fixture(
            "r=json.loads(sys.stdin.readline())\nprint(json.dumps({'jsonrpc':'2.0','id':float(r['id']),'result':'integral-float-id'}),flush=True)",
        );
        let process = ProviderProcess::spawn_json_rpc(options).unwrap();
        assert_eq!(
            process
                .request("probe", Value::Null, Duration::from_secs(2))
                .await
                .unwrap(),
            "integral-float-id"
        );
        for (code, accepted) in [
            ("1.0", true),
            ("-0.0", true),
            ("9007199254740991", true),
            ("-9007199254740991", true),
            ("1.5", false),
            ("9007199254740992", false),
            ("-9007199254740992", false),
        ] {
            let (_directory, options) = fixture(&format!(
                "r=json.loads(sys.stdin.readline())\nprint(json.dumps({{'jsonrpc':'2.0','id':r['id'],'error':{{'code':{code},'message':'code'}}}}),flush=True)"
            ));
            let process = ProviderProcess::spawn_json_rpc(options).unwrap();
            let result = process
                .request("probe", Value::Null, Duration::from_secs(2))
                .await;
            if accepted {
                assert!(
                    matches!(result, Err(ProcessError::Remote { .. })),
                    "{code}: {result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(ProcessError::Protocol(_))),
                    "{code}: {result:?}"
                );
            }
        }
    }
    #[tokio::test]
    async fn timeouts_and_task_cancellation_remove_response_waiters() {
        let (_directory, options) = fixture(
            "for line in sys.stdin:\n r=json.loads(line)\n print(json.dumps({'method':'received','params':r['id']}),flush=True)",
        );
        let process = ProviderProcess::spawn(options).unwrap();
        assert!(matches!(
            process
                .request("silent", Value::Null, Duration::from_millis(30))
                .await,
            Err(ProcessError::Timeout { .. })
        ));
        assert!(process.0.pending.lock().unwrap().is_empty());
        let mut events = process.subscribe();
        let clone = process.clone();
        let request = tokio::spawn(async move {
            clone
                .request("silent", Value::Null, Duration::from_secs(30))
                .await
        });
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(event,ProcessEvent::Notification{method,params} if method=="received" && params==2)
            {
                break;
            }
        }
        assert_eq!(process.0.pending.lock().unwrap().len(), 1);
        request.abort();
        let _ = request.await;
        assert!(process.0.pending.lock().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn releasing_last_client_reaps_its_owned_child() {
        let (_directory, options) = fixture(
            "import os\nfor line in sys.stdin:\n r=json.loads(line)\n print(json.dumps({'id':r['id'],'result':os.getpid()}),flush=True)",
        );
        let process = ProviderProcess::spawn(options).unwrap();
        let mut events = process.subscribe();
        let pid = process
            .request("pid", Value::Null, Duration::from_secs(2))
            .await
            .unwrap()
            .as_u64()
            .unwrap();
        drop(process);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            ProcessEvent::Closed(_)
        ));
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!status.success());
    }
    #[tokio::test]
    async fn malformed_output_and_process_exit_fail_pending_requests() {
        for body in [
            "sys.stdin.readline()\nprint('not json',flush=True)",
            "sys.stdin.readline()\nsys.exit(9)",
        ] {
            let (_directory, options) = fixture(body);
            let process = ProviderProcess::spawn(options).unwrap();
            let result = process
                .request("probe", Value::Null, Duration::from_secs(2))
                .await;
            assert!(matches!(
                result,
                Err(ProcessError::Protocol(_)
                    | ProcessError::Closed(_)
                    | ProcessError::Exited { .. })
            ));
            assert!(process.0.pending.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn line_budget_is_checked_before_decoding() {
        let bytes = vec![b'a'; MAX_LINE_BYTES + 1];
        let mut input = std::io::Cursor::new(bytes);
        assert!(matches!(
            bounded_line(&mut input).await,
            Err(ProcessError::Protocol(_))
        ));
    }
}
