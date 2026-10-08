//! Bidirectional Codex app-server JSON-lines transport.
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
struct Inner {
    outbound: mpsc::Sender<Value>,
    pending: Pending,
    next_id: AtomicU64,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<ProcessEvent>,
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
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let (outbound, mut outgoing) = mpsc::channel::<Value>(64);
        let (events, _) = broadcast::channel(1024);
        let (shutdown, mut stopping) = watch::channel(false);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(Mutex::new(None));
        let inner = Arc::new(Inner {
            outbound,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            permits: Arc::new(Semaphore::new(MAX_PENDING)),
            events: events.clone(),
            shutdown,
            closed: closed.clone(),
        });
        let (writer_error, mut errors) = mpsc::channel::<ProcessError>(1);
        tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(value) = outgoing.recv().await {
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
                                if let Err(error) = dispatch_message(value, &pending, &events) { break error; }
                            }
                            Ok(None) => {
                                // EOF may precede process exit; never wait indefinitely for an
                                // executable that closed stdout but continues running.
                                let status = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
                                let details = String::from_utf8_lossy(&diagnostic.lock().unwrap()).into_owned();
                                break ProcessError::Closed(format!("{status:?} {details}"));
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
        self.0.events.subscribe()
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
        self.0
            .outbound
            .send(value)
            .await
            .map_err(|_| ProcessError::Closed("stdin writer closed".into()))
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
fn dispatch_message(
    value: Value,
    pending: &Pending,
    events: &broadcast::Sender<ProcessEvent>,
) -> Result<(), ProcessError> {
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
    let id = value["id"]
        .as_u64()
        .or_else(|| value["id"].as_str().and_then(|id| id.parse().ok()))
        .ok_or_else(|| ProcessError::Protocol("response has no request id".into()))?;
    let result = if let Some(error) = value.get("error") {
        Err(ProcessError::Remote {
            code: error["code"].as_i64().unwrap_or(-32603),
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
                Err(ProcessError::Protocol(_) | ProcessError::Closed(_))
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
