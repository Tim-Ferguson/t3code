//! Provider event logging for the ACP family, Grok, Antigravity and registry agents (`provider/acp/AcpNativeLogging.ts`).
//!
//! Two kinds of line go to the thread's file (`provider_event_log`): `request` for the handful of session-setup calls the original's
//! runtime wraps in `runLoggedRequest` (each logged when it starts and again when it succeeds or fails), and `protocol` for decoded wire
//! frames. Both carry bounded structural summaries, never the values.
use crate::{persistence::Store, provider_event_log as log};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_acp::{
    AcpError,
    transport::{LogDirection, LogStage, ProtocolLogEvent, ProtocolLogger, ProtocolOptions},
};

/// The requests the original's runtime logs (`runLoggedRequest` and the `session/load` and `session/set_config_option` call sites).
/// A prompt, a cancel and the mode and model setters are not among them.
const LOGGED_METHODS: [&str; 11] = [
    "initialize",
    "authenticate",
    "session/new",
    "session/load",
    "session/resume",
    "session/fork",
    "session/list",
    "session/close",
    "session/delete",
    "session/set_config_option",
    "providers/list",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Started,
    Succeeded,
    Failed,
}

/// `AcpSessionRequestLogEvent`.
pub struct RequestLogEvent<'a> {
    pub method: &'a str,
    pub payload: &'a Value,
    pub status: Status,
    pub result: Option<&'a Value>,
    pub error: Option<&'a AcpError>,
}

/// What a peer calls around each request; a `None` logger is a server that keeps no provider event log.
pub type RequestLogger = Arc<dyn Fn(RequestLogEvent<'_>) + Send + Sync>;

pub fn logs_method(method: &str) -> bool {
    LOGGED_METHODS.contains(&method)
}

/// `makeAcpNativeLoggerFactory`'s `requestLogger` for one thread of `provider` (the driver name).
pub fn request_logger(store: &Store, provider: &str, thread_id: &str) -> Option<RequestLogger> {
    let logger = log::native(store)?;
    let (provider, thread_id) = (provider.to_owned(), thread_id.to_owned());
    Some(Arc::new(move |event| log::write_acp_record(&logger, &provider, &thread_id, "request", request_payload(&event))))
}

/// `formatRequestLogPayload`.
fn request_payload(event: &RequestLogEvent<'_>) -> Value {
    let mut payload = json!({
        "method": log::structural_protocol_method(event.method),
        "status": match event.status { Status::Started => "started", Status::Succeeded => "succeeded", Status::Failed => "failed" },
        "request": log::summarize_native_protocol_payload(event.payload),
    });
    if let Some(result) = event.result {
        payload["result"] = log::summarize_native_protocol_payload(result);
    }
    if let Some(error) = event.error {
        payload["errorTag"] = json!(error_tag(error));
        // A failed ACP call has the one reason an Effect cause would carry.
        payload["reasonCount"] = json!(1);
    }
    payload
}

/// `causeErrorTag` for the error classes of `effect-acp` (`errors.ts`): the class a failure belongs to.
fn error_tag(error: &AcpError) -> &'static str {
    use t3_acp::errors::Failure;
    match error {
        AcpError::Failure(failure) => match failure.as_ref() {
            Failure::Request(_) => "AcpRequestError",
            Failure::Spawn(_) => "AcpSpawnError",
            Failure::ProcessExited(_) => "AcpProcessExitedError",
            Failure::ProtocolParse(_) => "AcpProtocolParseError",
            Failure::Transport(_) => "AcpTransportError",
            Failure::InputStreamEnded(_) => "AcpInputStreamEndedError",
        },
        AcpError::ResponseError { .. } | AcpError::Request(_) => "AcpRequestError",
        AcpError::ResponseDefect { .. } => "Die",
        AcpError::ResponseCause { .. } => "AcpRequestError",
        AcpError::Schema(_) => "AcpProtocolParseError",
        AcpError::ProcessExited { .. } => "AcpProcessExitedError",
        AcpError::Transport(_) | AcpError::Timeout { .. } | AcpError::Closed => "AcpTransportError",
    }
}

// ---- protocol frames ---------------------------------------------------------------------------------------------------------------

// TODO(original AcpNativeLogging.ts): every caller of `makeNativeLogger` leaves `verboseProtocolLogging` unset, so the original
// writes no `protocol` line for any ACP agent. The port builds the logger and keeps it off in the same way.
pub const VERBOSE_PROTOCOL_LOGGING: bool = false;

const TRANSIENT_UPDATES: [&str; 2] = ["agent_message_chunk", "agent_thought_chunk"];

/// `isTransientProtocolMessage`: a streamed text or thought chunk.
fn is_transient_message(message: &Value) -> bool {
    if !message.is_object() {
        return false;
    }
    let method = message.get("tag").filter(|value| !value.is_null()).or_else(|| message.get("method"));
    if method.and_then(Value::as_str) != Some("session/update") {
        return false;
    }
    let payload = message.get("payload").filter(|value| !value.is_null()).or_else(|| message.get("params"));
    let Some(update) = payload.and_then(|payload| payload.get("update")).filter(|update| update.is_object()) else { return false };
    update["sessionUpdate"].as_str().is_some_and(|kind| TRANSIENT_UPDATES.contains(&kind))
}

/// `rawChunkContainsOnlyTransientMessages`: complete lines that all parse and are all transient.
fn raw_chunk_is_only_transient(payload: &str) -> bool {
    let mut lines: Vec<&str> = payload.split('\n').collect();
    let remainder = lines.pop().unwrap_or("");
    if !remainder.trim().is_empty() {
        return false;
    }
    let mut messages = vec![];
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => messages.push(message),
            Err(_) => return false,
        }
    }
    !messages.is_empty() && messages.iter().all(is_transient_message)
}

/// `filterTransientProtocolLog`: `None` when nothing of the event is worth a line.
pub fn filter_transient(event: &ProtocolLogEvent) -> Option<ProtocolLogEvent> {
    if event.direction != LogDirection::Incoming {
        return Some(event.clone());
    }
    if event.stage == LogStage::Raw {
        if let Some(text) = event.payload.as_str() {
            return (!raw_chunk_is_only_transient(text)).then(|| event.clone());
        }
    }
    if event.stage != LogStage::Decoded {
        return Some(event.clone());
    }
    match event.payload.as_array() {
        None => (!is_transient_message(&event.payload)).then(|| event.clone()),
        Some(messages) => {
            let kept: Vec<Value> = messages.iter().filter(|message| !is_transient_message(message)).cloned().collect();
            (!kept.is_empty()).then(|| ProtocolLogEvent { payload: Value::Array(kept), ..event.clone() })
        }
    }
}

/// `formatAcpProtocolLogPayload`.
fn protocol_payload(event: &ProtocolLogEvent) -> Value {
    json!({
        "direction": match event.direction { LogDirection::Incoming => "incoming", LogDirection::Outgoing => "outgoing" },
        "stage": match event.stage { LogStage::Raw => "raw", LogStage::Decoded => "decoded", LogStage::DecodeFailed => "decode_failed" },
        "payload": log::summarize_native_protocol_payload(&event.payload),
    })
}

/// `protocolLogging`: turns the process's frame logging on, when `verbose` and the server keeps a log. The other fields of `options`
/// (an Antigravity launch's stdout transform and stderr observer) stay as they were.
pub fn log_protocol(options: &mut ProtocolOptions, store: &Store, provider: &str, thread_id: &str, verbose: bool) {
    let Some(logger) = verbose.then(|| log::native(store)).flatten() else { return };
    let (provider, thread_id) = (provider.to_owned(), thread_id.to_owned());
    let write: ProtocolLogger = Arc::new(move |event| {
        if let Some(filtered) = filter_transient(&event) {
            log::write_acp_record(&logger, &provider, &thread_id, "protocol", protocol_payload(&filtered));
        }
        Box::pin(async {})
    });
    options.log_incoming = true;
    options.log_outgoing = true;
    options.logger = Some(write);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(kind: &str) -> Value {
        json!({"_tag":"Request","tag":"session/update","payload":{"update":{"sessionUpdate":kind}}})
    }
    fn event(direction: LogDirection, stage: LogStage, payload: Value) -> ProtocolLogEvent {
        ProtocolLogEvent { direction, stage, payload }
    }

    /// Streamed chunks are dropped before formatting, whether raw lines or decoded frames, and a mixed frame keeps what is left.
    #[test]
    fn transient_chunks_are_dropped_and_the_rest_is_kept() {
        let raw = |lines: &str| event(LogDirection::Incoming, LogStage::Raw, json!(lines));
        let chunk = json!({"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"}}}).to_string();
        assert_eq!(filter_transient(&raw(&format!("{chunk}\n{chunk}\n"))), None);
        assert!(filter_transient(&raw(&format!("{chunk}\n{{\"partial\":"))).is_some(), "an unterminated line is kept");
        assert!(filter_transient(&raw("not json\n")).is_some());
        assert_eq!(filter_transient(&event(LogDirection::Incoming, LogStage::Decoded, json!([update("agent_thought_chunk"), update("agent_message_chunk")]))), None);
        let mixed = filter_transient(&event(LogDirection::Incoming, LogStage::Decoded, json!([update("agent_message_chunk"), update("tool_call")]))).unwrap();
        assert_eq!(mixed.payload, json!([update("tool_call")]));
        let outgoing = event(LogDirection::Outgoing, LogStage::Decoded, update("agent_message_chunk"));
        assert_eq!(filter_transient(&outgoing), Some(outgoing));
    }

    fn lines(directory: &std::path::Path) -> Vec<Value> {
        let file = directory.join("provider").join("events.thread-1.log");
        std::fs::read_to_string(file).unwrap().lines().map(|line| serde_json::from_str(line.split_once("NTIVE: ").unwrap().1).unwrap()).collect()
    }
    fn store_with_log(directory: &std::path::Path) -> Store {
        let store = Store::open(directory.join("rust-state.sqlite")).unwrap();
        let options = log::Options { batch_window_ms: 0, ..log::Options::default() };
        log::install(&store, log::EventLogStore::open(&directory.join("provider").join("events.log"), options, Arc::new(|| 1_700_000_000_000), None).unwrap());
        store
    }

    #[test]
    fn requests_and_protocol_frames_are_written_as_bounded_summaries() {
        let directory = tempfile::tempdir().unwrap();
        let bare = Store::open(directory.path().join("bare.sqlite")).unwrap();
        assert!(request_logger(&bare, "grok", "thread-1").is_none());
        let store = store_with_log(directory.path());
        let secret = "secret-token-value";
        let requests = request_logger(&store, "grok", "thread-1").unwrap();
        let payload = json!({"cwd":secret,"mcpServers":[]});
        requests(RequestLogEvent { method: "session/new", payload: &payload, status: Status::Started, result: None, error: None });
        requests(RequestLogEvent { method: "session/new", payload: &payload, status: Status::Succeeded, result: Some(&json!({"sessionId":secret})), error: None });
        let failure = AcpError::Request(t3_acp::RpcError { code: -32603, message: secret.into(), data: Some(json!({"token":secret})) });
        requests(RequestLogEvent { method: "session/new", payload: &payload, status: Status::Failed, result: None, error: Some(&failure) });
        let mut options = ProtocolOptions::default();
        log_protocol(&mut options, &store, "grok", "thread-1", false);
        assert!(options.logger.is_none(), "the original never turns protocol logging on");
        log_protocol(&mut options, &store, "grok", "thread-1", true);
        let logger = options.logger.clone().unwrap();
        futures_executor_block(logger(event(LogDirection::Incoming, LogStage::Raw, json!(format!("{{\"token\":\"{secret}\"}}\n")))));
        futures_executor_block(logger(event(LogDirection::Incoming, LogStage::Decoded, json!([update("agent_message_chunk")]))));
        futures_executor_block(logger(event(LogDirection::Outgoing, LogStage::Decoded, json!({"_tag":"Request","tag":"session/prompt","payload":{"prompt":secret}}))));
        log::current(&store).unwrap().close();
        let written = lines(directory.path());
        assert_eq!(written.len(), 5, "the chunk is not kept: {written:?}");
        assert!(!serde_json::to_string(&written).unwrap().contains(secret));
        let first = &written[0]["event"];
        assert_eq!((first["kind"].as_str(), first["provider"].as_str(), first["threadId"].as_str(), first["createdAt"].as_str()), (Some("request"), Some("grok"), Some("thread-1"), Some("2023-11-14T22:13:20.000Z")));
        assert_eq!(written[0]["observedAt"], first["createdAt"]);
        assert_eq!(first["payload"], json!({"method":"session/new","status":"started","request":{"valueType":"object","fieldCount":2}}));
        assert_eq!(written[1]["event"]["payload"]["result"], json!({"valueType":"object","fieldCount":1}));
        let failed = &written[2]["event"]["payload"];
        assert_eq!((failed["status"].as_str(), failed["errorTag"].as_str(), failed["reasonCount"].as_u64()), (Some("failed"), Some("AcpRequestError"), Some(1)));
        assert_eq!(written[3]["event"]["kind"], "protocol");
        assert_eq!(written[3]["event"]["payload"], json!({"direction":"incoming","stage":"raw","payload":{"valueType":"string","byteLength":format!("{{\"token\":\"{secret}\"}}\n").len()}}));
        assert_eq!(written[4]["event"]["payload"]["payload"], json!({"valueType":"object","fieldCount":3,"messageTag":"Request","method":"session/prompt"}));
    }

    /// The logger returns an already-complete future, so the test drives it without a runtime.
    fn futures_executor_block(future: futures_util::future::BoxFuture<'static, ()>) {
        futures_util::FutureExt::now_or_never(future).expect("the logger writes synchronously");
    }
}
