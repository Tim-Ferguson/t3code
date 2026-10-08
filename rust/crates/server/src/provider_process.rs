//! Bidirectional provider JSON-lines transport (raw Codex or versioned JSON-RPC).
//! The process belongs to the client lifetime, and response waiters are removed on
//! cancellation. Notification consumers must treat broadcast lag as lost continuity.
use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use t3_acp::transport::{
    LogDirection, LogStage, NdjsonDecoder, ProtocolLogEvent, ProtocolOptions, StdoutStream,
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
    #[error(transparent)]
    Acp(std::sync::Arc<t3_acp::AcpError>),
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
    #[error("Provider JSON-RPC request failed ({code}): {message}")]
    JsonRpcRemote {
        method: String,
        request_id: Value,
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("Provider request {method} failed with an Effect cause")]
    EffectCause {
        method: String,
        request_id: Value,
        cause: t3_acp::EffectCause,
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
    IngressBarrier {
        acknowledgement: t3_acp::IngressAcknowledgement,
    },
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
struct PendingReply {
    result: Result<Value, ProcessError>,
    acknowledgement: Option<t3_acp::IngressAcknowledgement>,
}
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<PendingReply>>>>;
// A source batch stores replies in completion order and emits only when all
// non-notification requests have replied. Associations are consumed once.
type Batches = Arc<Mutex<HashMap<String, Arc<Mutex<ResponseBatch>>>>>;
struct ResponseBatch {
    expected: usize,
    responses: indexmap::IndexMap<String, Value>,
}
fn response_identity(id: &Value) -> Option<String> {
    serde_json::from_value::<t3_acp::RequestId>(id.clone())
        .ok()
        .map(|id| id.identity())
}
fn register_batch(batch: &[Value], batches: &Batches) {
    let ids: Vec<_> = batch
        .iter()
        .filter(|item| item.get("method").is_some())
        .filter_map(|item| item.get("id").and_then(response_identity))
        .collect();
    let group = Arc::new(Mutex::new(ResponseBatch {
        expected: ids.len(),
        responses: indexmap::IndexMap::new(),
    }));
    let mut batches = batches.lock().unwrap();
    for id in ids {
        batches.insert(id, group.clone());
    }
}
fn batch_response(value: Value, batches: &Batches) -> Option<Value> {
    let group = value
        .get("id")
        .and_then(response_identity)
        .and_then(|id| batches.lock().unwrap().remove(&id).map(|group| (id, group)));
    if let Some((id, group)) = group {
        let mut group = group.lock().unwrap();
        group.responses.insert(id, value);
        if group.responses.len() != group.expected {
            return None;
        }
        Some(Value::Array(
            group.responses.drain(..).map(|(_, value)| value).collect(),
        ))
    } else {
        Some(value)
    }
}
struct Outgoing {
    value: Value,
    written: oneshot::Sender<Result<(), ProcessError>>,
}
struct Inner {
    outbound: mpsc::Sender<Outgoing>,
    json_rpc: bool,
    ordered_ingress: Arc<AtomicBool>,
    pending: Pending,
    next_id: AtomicU64,
    next_core_id: AtomicU64,
    permits: Arc<Semaphore>,
    events: broadcast::Sender<ProcessEvent>,
    first_receiver: Mutex<Option<broadcast::Receiver<ProcessEvent>>>,
    shutdown: watch::Sender<bool>,
    closed: Arc<Mutex<Option<ProcessError>>>,
    external_failure: watch::Sender<Option<ProcessError>>,
    protocol_options: ProtocolOptions,
    batches: Batches,
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
        Self::spawn_protocol(options, false, ProtocolOptions::default())
    }
    /// ACP uses versioned JSON-RPC envelopes; Codex's raw envelopes retain their
    /// existing encoding through `spawn`. Both modes share owned process cleanup.
    pub fn spawn_json_rpc(options: ProcessOptions) -> Result<Self, ProcessError> {
        Self::spawn_json_rpc_with_options(options, ProtocolOptions::default())
    }
    pub fn spawn_json_rpc_with_options(
        options: ProcessOptions,
        protocol_options: ProtocolOptions,
    ) -> Result<Self, ProcessError> {
        Self::spawn_protocol(options, true, protocol_options)
    }
    fn spawn_protocol(
        options: ProcessOptions,
        json_rpc: bool,
        protocol_options: ProtocolOptions,
    ) -> Result<Self, ProcessError> {
        let mut builder = if json_rpc && cfg!(windows) {
            let mut environment: indexmap::IndexMap<String, String> = std::env::vars().collect();
            environment.extend(options.environment.clone());
            crate::acp_registry_spawn::command(
                &options.binary.to_string_lossy(),
                &options.args,
                Some(&environment),
            )
        } else {
            let mut builder = Command::new(&options.binary);
            builder.args(&options.args).envs(&options.environment);
            builder
        };
        let mut child = builder
            .current_dir(options.cwd)
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
        let (external_failure, _) = watch::channel(None);
        let ordered_ingress = Arc::new(AtomicBool::new(false));
        let batches: Batches = Arc::new(Mutex::new(HashMap::new()));
        let inner = Arc::new(Inner {
            outbound,
            json_rpc,
            ordered_ingress: ordered_ingress.clone(),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            next_core_id: AtomicU64::new(1u64 << 32),
            permits: Arc::new(Semaphore::new(MAX_PENDING)),
            events: events.clone(),
            first_receiver: Mutex::new(Some(first_receiver)),
            shutdown,
            closed: closed.clone(),
            external_failure: external_failure.clone(),
            protocol_options: protocol_options.clone(),
            batches: batches.clone(),
        });
        let mut input = if json_rpc {
            let stream: StdoutStream = Box::pin(futures_util::stream::unfold(
                stdout,
                |mut stdout| async move {
                    let mut buffer = vec![0; 8192];
                    match stdout.read(&mut buffer).await {
                        Ok(0) => None,
                        Ok(size) => {
                            buffer.truncate(size);
                            Some((Ok(buffer), stdout))
                        }
                        Err(error) => Some((
                            Err(t3_acp::transport::read_input_error(error.to_string())),
                            stdout,
                        )),
                    }
                },
            ));
            ProcessInput::Json {
                stream: if let Some(transform) = &protocol_options.transform_stdout {
                    transform(stream)
                } else {
                    stream
                },
                decoder: NdjsonDecoder::new(protocol_options.max_frame_bytes.unwrap_or(usize::MAX)),
            }
        } else {
            ProcessInput::Raw(BufReader::new(stdout))
        };
        let (writer_error, mut errors) = mpsc::channel::<ProcessError>(1);
        let writer_limit = if json_rpc {
            protocol_options.max_frame_bytes
        } else {
            Some(MAX_LINE_BYTES)
        };
        tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(Outgoing { mut value, written }) = outgoing.recv().await {
                    if json_rpc && value.is_object() {
                        value["jsonrpc"] = json!("2.0");
                    }
                    let result = async {
                        let mut bytes = serde_json::to_vec(&value)
                            .map_err(|e| ProcessError::Protocol(e.to_string()))?;
                        if writer_limit.is_some_and(|limit| bytes.len() > limit) {
                            return Err(ProcessError::Protocol(
                                "outgoing message exceeds byte budget".into(),
                            ));
                        }
                        bytes.push(b'\n');
                        stdin
                            .write_all(&bytes)
                            .await
                            .map_err(|e| writer_failure(json_rpc, e.to_string()))?;
                        stdin
                            .flush()
                            .await
                            .map_err(|e| writer_failure(json_rpc, e.to_string()))
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
            let (failure, input_failure) = 'reader: loop {
                tokio::select! {
                    _ = stopping.changed() => { let _=child.kill().await; break (ProcessError::Closed("client was released".into()), false); }
                    Some(error) = errors.recv() => break (error, false),
                    result = next_input(&mut input, &protocol_options, &batches) => {
                        match result {
                            Ok(Some(values)) => {
                                for value in values {
                                    if json_rpc && value["jsonrpc"]!="2.0" {break 'reader (ProcessError::Protocol("Expected JSON-RPC version 2.0.".into()), true);}
                                    let acknowledgement = match dispatch_message(value, &pending, &events, json_rpc, ordered_ingress.load(Ordering::Acquire)) {
                                        Ok(acknowledgement) => acknowledgement,
                                        Err(error) => break 'reader (error, true),
                                    };
                                    if let Some(acknowledgement) = acknowledgement {
                                        tokio::select! {
                                            _=acknowledgement.wait()=>{},
                                            _=stopping.changed()=>break 'reader (ProcessError::Closed("client was released".into()), false),
                                            Some(error)=errors.recv()=>break 'reader (error, false),
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                let status = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
                                let details = String::from_utf8_lossy(&diagnostic.lock().unwrap()).into_owned();
                                let code = status.as_ref().ok().and_then(|status|status.as_ref().ok()).and_then(|status|status.code());
                                break (ProcessError::Exited{code,pid,message:format!("{status:?} {details}")}, true);
                            }
                            Err(error) => break (error, true),
                        }
                    }
                }
            };
            let acknowledgement =
                (json_rpc && input_failure && ordered_ingress.load(Ordering::Acquire)).then(|| {
                    let acknowledgement = t3_acp::IngressAcknowledgement::new();
                    let _ = events.send(ProcessEvent::IngressBarrier {
                        acknowledgement: acknowledgement.clone(),
                    });
                    acknowledgement
                });
            if !input_failure {
                external_failure.send_replace(Some(failure.clone()));
            }
            *closed.lock().unwrap() = Some(failure.clone());
            for (_, waiter) in pending.lock().unwrap().drain() {
                let _ = waiter.send(PendingReply {
                    result: Err(failure.clone()),
                    acknowledgement: acknowledgement.clone(),
                });
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
    pub fn enable_ordered_ingress(&self) {
        if self.0.json_rpc {
            self.0.ordered_ingress.store(true, Ordering::Release);
        }
    }
    /// Stop and reap this owned child before releasing its runtime scope.
    pub async fn shutdown(&self) {
        let mut events = self.subscribe();
        let _ = self.0.shutdown.send(true);
        while let Ok(event) = events.recv().await {
            if matches!(event, ProcessEvent::Closed(_)) {
                break;
            }
        }
    }
    pub async fn external_failure(&self) -> ProcessError {
        let mut receiver = self.0.external_failure.subscribe();
        loop {
            if let Some(error) = receiver.borrow().clone() {
                return error;
            }
            if receiver.changed().await.is_err() {
                return ProcessError::Closed("provider writer was released".into());
            }
        }
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProcessError> {
        self.request_with_mode(method, params, timeout, false).await
    }
    pub async fn request_core(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProcessError> {
        self.request_with_mode(method, params, timeout, true).await
    }
    async fn request_with_mode(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        core: bool,
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
                let counter = if self.0.json_rpc && core {
                    &self.0.next_core_id
                } else {
                    &self.0.next_id
                };
                let id = counter.fetch_add(1, Ordering::Relaxed);
                let (waiter, response) = oneshot::channel();
                self.0.pending.lock().unwrap().insert(id, waiter);
                (id, response)
            };
            let _guard = PendingGuard {
                id,
                pending: self.0.pending.clone(),
            };
            self.send_admitted(json!({"id":id,"method":method,"params":params}))
                .await?;
            let reply = response
                .await
                .map_err(|_| ProcessError::Closed("response channel closed".into()))?;
            if let Some(acknowledgement) = reply.acknowledgement {
                acknowledgement.wait().await;
            }
            reply.result.map_err(|error| match error {
                ProcessError::EffectCause {
                    request_id, cause, ..
                } => ProcessError::EffectCause {
                    method: method.into(),
                    request_id,
                    cause,
                },
                ProcessError::JsonRpcRemote {
                    request_id,
                    code,
                    message,
                    data,
                    ..
                } => ProcessError::JsonRpcRemote {
                    method: method.into(),
                    request_id,
                    code,
                    message,
                    data,
                },
                other => other,
            })
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
        self.send_admitted(value).await
    }
    pub async fn respond(
        &self,
        id: Value,
        result: Result<Value, ProcessError>,
    ) -> Result<(), ProcessError> {
        self.respond_observed(id, result).await.map(|_| ())
    }
    pub async fn respond_observed(
        &self,
        id: Value,
        result: Result<Value, ProcessError>,
    ) -> Result<t3_acp::ResponseDisposition, ProcessError> {
        let value = match result {
            Ok(result) => json!({"id":id,"result":result}),
            Err(ProcessError::Remote {
                code,
                message,
                data,
            }) => json!({"id":id,"error":{"code":code,"message":message,"data":data}}),
            Err(ProcessError::JsonRpcRemote {
                code,
                message,
                data,
                ..
            }) => {
                let mut response = json!({"id":id,"error":{"code":code,"message":message}});
                if let Some(data) = data {
                    response["error"]["data"] = data;
                }
                response
            }
            Err(error) => json!({"id":id,"error":{"code":-32603,"message":error.to_string()}}),
        };
        self.send_response(value).await
    }
    /// Effect RPC's private Cause envelope is used only by versioned ACP peers.
    /// Writes retain the same bounded-queue / completed-stdin acknowledgement.
    pub async fn respond_cause(
        &self,
        id: Value,
        cause: t3_acp::EffectCause,
    ) -> Result<(), ProcessError> {
        self.respond_cause_observed(id, cause).await.map(|_| ())
    }
    pub async fn respond_cause_observed(
        &self,
        id: Value,
        cause: t3_acp::EffectCause,
    ) -> Result<t3_acp::ResponseDisposition, ProcessError> {
        if !self.0.json_rpc {
            return Err(ProcessError::Protocol(
                "Effect causes require versioned JSON-RPC".into(),
            ));
        }
        if !id.is_string() && !id.is_number() {
            return Err(ProcessError::Protocol(
                "invalid JSON-RPC response id".into(),
            ));
        }
        let cause_value = serde_json::to_value(cause)
            .map_err(|error| ProcessError::Protocol(error.to_string()))?;
        if safe_rpc_error_code(&cause_value["code"]).is_none() {
            return Err(ProcessError::Protocol(
                "invalid JSON-RPC Effect cause code".into(),
            ));
        }
        serde_json::from_value::<t3_acp::EffectCause>(cause_value.clone())
            .map_err(|error| ProcessError::Protocol(error.to_string()))?;
        self.send_response(json!({"id":id,"error":cause_value}))
            .await
    }
    async fn send_admitted(&self, value: Value) -> Result<(), ProcessError> {
        self.send_with_ack(value, !self.0.json_rpc)
            .await
            .map(|_| ())
    }
    async fn send_response(
        &self,
        value: Value,
    ) -> Result<t3_acp::ResponseDisposition, ProcessError> {
        self.send_with_ack(value, true).await
    }
    async fn send_with_ack(
        &self,
        value: Value,
        acknowledge_write: bool,
    ) -> Result<t3_acp::ResponseDisposition, ProcessError> {
        let value = if self.0.json_rpc {
            if let Some(error) = self.0.closed.lock().unwrap().clone() {
                return Err(error);
            }
            if self.0.protocol_options.log_outgoing {
                log_event(
                    &self.0.protocol_options,
                    ProtocolLogEvent {
                        direction: LogDirection::Outgoing,
                        stage: LogStage::Decoded,
                        payload: t3_acp::transport::outgoing_decoded(&value),
                    },
                )
                .await;
            }
            let wire = t3_acp::transport::outgoing_wire(&value);
            let wire = if acknowledge_write {
                match batch_response(wire, &self.0.batches) {
                    Some(wire) => wire,
                    None => return Ok(t3_acp::ResponseDisposition::Buffered),
                }
            } else {
                wire
            };
            if let Some(limit) = self.0.protocol_options.max_frame_bytes {
                let size = serde_json::to_vec(&wire)
                    .map_err(|error| ProcessError::Protocol(error.to_string()))?
                    .len();
                if size > limit {
                    return Err(ProcessError::Protocol(
                        "outgoing message exceeds byte budget".into(),
                    ));
                }
            }
            if self.0.protocol_options.log_outgoing {
                let raw = serde_json::to_string(&wire)
                    .map_err(|error| ProcessError::Protocol(error.to_string()))?
                    + "\n";
                log_event(
                    &self.0.protocol_options,
                    ProtocolLogEvent {
                        direction: LogDirection::Outgoing,
                        stage: LogStage::Raw,
                        payload: json!(raw),
                    },
                )
                .await;
            }
            wire
        } else {
            value
        };
        if let Some(error) = self.0.closed.lock().unwrap().clone() {
            return Err(error);
        }
        let (written, acknowledgement) = oneshot::channel();
        self.0
            .outbound
            .send(Outgoing { value, written })
            .await
            .map_err(|_| self.writer_closed())?;
        if acknowledge_write {
            acknowledgement.await.map_err(|_| self.writer_closed())??;
        }
        Ok(t3_acp::ResponseDisposition::Written)
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

pub(crate) async fn bounded_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, ProcessError> {
    bounded_line_with_limit(reader, MAX_LINE_BYTES).await
}
pub(crate) async fn bounded_line_with_limit(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    max_bytes: usize,
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
        if line.len().saturating_add(size) > max_bytes {
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
enum ProcessInput {
    Raw(BufReader<tokio::process::ChildStdout>),
    Json {
        stream: StdoutStream,
        decoder: NdjsonDecoder,
    },
}
fn writer_failure(json_rpc: bool, cause: String) -> ProcessError {
    if json_rpc {
        ProcessError::Acp(Arc::new(
            t3_acp::errors::TransportError {
                operation: None,
                method: None,
                detail: Some("Failed to write an outgoing ACP message".into()),
                pid: None,
                cause: t3_acp::errors::FailureCause::Value(json!({"name":"Error","message":cause})),
            }
            .into(),
        ))
    } else {
        ProcessError::Io(cause)
    }
}
async fn log_event(options: &ProtocolOptions, event: ProtocolLogEvent) {
    options.log(event).await;
}
async fn next_input(
    input: &mut ProcessInput,
    options: &ProtocolOptions,
    batches: &Batches,
) -> Result<Option<Vec<Value>>, ProcessError> {
    let read = async {
        match input {
            ProcessInput::Raw(reader) => match bounded_line(reader).await? {
                Some(line) => serde_json::from_slice(&line)
                    .map(|value| Some(vec![value]))
                    .map_err(|error| ProcessError::Protocol(error.to_string())),
                None => Ok(None),
            },
            ProcessInput::Json { stream, decoder } => {
                let chunk = match stream.next().await {
                    None => return Ok(None),
                    Some(Err(error)) => return Err(ProcessError::Acp(Arc::new(error))),
                    Some(Ok(chunk)) => chunk,
                };
                if options.log_incoming {
                    log_event(
                        options,
                        ProtocolLogEvent {
                            direction: LogDirection::Incoming,
                            stage: LogStage::Raw,
                            payload: json!(String::from_utf8_lossy(&chunk)),
                        },
                    )
                    .await;
                }
                let frames = match decoder.decode_frames(&chunk) {
                    Ok(values) => values,
                    Err(error) => {
                        if options.log_incoming {
                            log_event(
                                options,
                                ProtocolLogEvent {
                                    direction: LogDirection::Incoming,
                                    stage: LogStage::DecodeFailed,
                                    payload: json!({"operation":"decode-wire-message"}),
                                },
                            )
                            .await;
                        }
                        return Err(ProcessError::Acp(Arc::new(error)));
                    }
                };
                let mut values = Vec::new();
                for frame in frames {
                    match frame {
                        Value::Array(batch) => {
                            register_batch(&batch, batches);
                            values.extend(batch.into_iter().filter(Value::is_object));
                        }
                        value if value.is_object() => values.push(value),
                        _ => {}
                    }
                }
                if options.log_incoming {
                    log_event(
                        options,
                        ProtocolLogEvent {
                            direction: LogDirection::Incoming,
                            stage: LogStage::Decoded,
                            payload: json!(
                                values
                                    .iter()
                                    .map(t3_acp::transport::decoded_message)
                                    .collect::<Vec<_>>()
                            ),
                        },
                    )
                    .await;
                }
                Ok(Some(values))
            }
        }
    };
    AssertUnwindSafe(read)
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("Rust input callback panicked");
            Err(ProcessError::Acp(Arc::new(
                t3_acp::transport::read_input_error(message),
            )))
        })
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
    ordered_ingress: bool,
) -> Result<Option<t3_acp::IngressAcknowledgement>, ProcessError> {
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
            if has_error && value["error"]["_tag"] == "Cause" {
                if safe_rpc_error_code(&value["error"]["code"]).is_none() {
                    return Err(ProcessError::Protocol(
                        "invalid JSON-RPC Effect cause code".into(),
                    ));
                }
                serde_json::from_value::<t3_acp::EffectCause>(value["error"].clone()).map_err(
                    |cause| {
                        ProcessError::Protocol(format!(
                            "invalid JSON-RPC Effect cause envelope: {cause}"
                        ))
                    },
                )?;
            } else if has_error
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
        let acknowledgement = (json_rpc && ordered_ingress).then(|| {
            let acknowledgement = t3_acp::IngressAcknowledgement::new();
            let _ = events.send(ProcessEvent::IngressBarrier {
                acknowledgement: acknowledgement.clone(),
            });
            acknowledgement
        });
        return Ok(acknowledgement);
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
            Ok(None)
        } else {
            Err(ProcessError::Protocol("response has no request id".into()))
        };
    };
    let result = if json_rpc && value["error"]["_tag"] == "Cause" {
        Err(ProcessError::EffectCause {
            method: String::new(),
            request_id: value["id"].clone(),
            cause: serde_json::from_value(value["error"].clone()).expect("validated Effect cause"),
        })
    } else if let Some(error) = value.get("error") {
        let code = if json_rpc {
            safe_rpc_error_code(&error["code"]).unwrap()
        } else {
            error["code"].as_i64().unwrap_or(-32603)
        };
        let message = error["message"]
            .as_str()
            .unwrap_or("Provider request failed")
            .into();
        if json_rpc {
            Err(ProcessError::JsonRpcRemote {
                method: String::new(),
                request_id: value["id"].clone(),
                code,
                message,
                data: error.get("data").cloned(),
            })
        } else {
            Err(ProcessError::Remote {
                code,
                message,
                data: error.get("data").cloned().unwrap_or(Value::Null),
            })
        }
    } else {
        Ok(value
            .get("result")
            .cloned()
            .ok_or_else(|| ProcessError::Protocol("response has no result or error".into()))?)
    };
    // Late responses to a timed-out/cancelled request are intentionally ignored.
    if let Some(waiter) = pending.lock().unwrap().remove(&id) {
        let acknowledgement = (json_rpc && ordered_ingress).then(|| {
            let acknowledgement = t3_acp::IngressAcknowledgement::new();
            let _ = events.send(ProcessEvent::IngressBarrier {
                acknowledgement: acknowledgement.clone(),
            });
            acknowledgement
        });
        let _ = waiter.send(PendingReply {
            result,
            acknowledgement: acknowledgement.clone(),
        });
        return Ok(acknowledgement);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn batch_association_and_collision_behavior_matches_actual_source_serializer() {
        for line in include_str!("../../acp/tests/fixtures/batch.jsonl").lines() {
            let case: Value = serde_json::from_str(line).unwrap();
            let batches: Batches = Arc::new(Mutex::new(HashMap::new()));
            // Source decodes the entire chunk before routing any request.
            for frame in case["frames"].as_array().unwrap() {
                if let Some(batch) = frame.as_array() {
                    register_batch(batch, &batches);
                }
            }
            for (response, expected) in case["responses"]
                .as_array()
                .unwrap()
                .iter()
                .zip(case["observations"].as_array().unwrap())
            {
                let actual = batch_response(t3_acp::transport::outgoing_wire(response), &batches);
                assert_eq!(actual.unwrap_or(Value::Null), *expected, "{}", case["name"]);
            }
        }
    }

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
            matches!(process.request("probe",Value::Null,Duration::from_secs(2)).await,Err(ProcessError::JsonRpcRemote{code:-32000,message,data:Some(data),..}) if message=="detail"&&data["why"]=="retained")
        );
    }
    #[tokio::test]
    async fn opt_in_notification_frame_budget_and_process_exit_keep_identity() {
        let (_directory, options) = fixture("for line in sys.stdin: pass");
        let process = ProviderProcess::spawn_json_rpc_with_options(
            options,
            ProtocolOptions {
                max_frame_bytes: Some(MAX_LINE_BYTES),
                ..Default::default()
            },
        )
        .unwrap();
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
                    matches!(result, Err(ProcessError::JsonRpcRemote { .. })),
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
