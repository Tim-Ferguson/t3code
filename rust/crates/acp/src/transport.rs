//! Process-neutral stream and logging options, installed before reading stdout.
use crate::{
    AcpError,
    errors::{
        FailureCause, ProtocolParseError, ProtocolParseOperation, TransportError,
        TransportOperation,
    },
};
use futures_util::{Stream, future::BoxFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{pin::Pin, sync::Arc};

pub type StdoutStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, AcpError>> + Send>>;
pub type StdoutTransform = Arc<dyn Fn(StdoutStream) -> StdoutStream + Send + Sync>;
pub type ProtocolLogger = Arc<dyn Fn(ProtocolLogEvent) -> BoxFuture<'static, ()> + Send + Sync>;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDirection {
    Incoming,
    Outgoing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStage {
    Raw,
    Decoded,
    DecodeFailed,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolLogEvent {
    pub direction: LogDirection,
    pub stage: LogStage,
    pub payload: Value,
}
#[derive(Clone, Default)]
pub struct ProtocolOptions {
    /// ACP has no default frame limit. A caller may opt into a byte budget.
    pub max_frame_bytes: Option<usize>,
    pub log_incoming: bool,
    pub log_outgoing: bool,
    pub logger: Option<ProtocolLogger>,
    pub transform_stdout: Option<StdoutTransform>,
}
impl ProtocolOptions {
    pub fn enabled(&self, direction: LogDirection) -> bool {
        match direction {
            LogDirection::Incoming => self.log_incoming,
            LogDirection::Outgoing => self.log_outgoing,
        }
    }
    pub async fn log(&self, event: ProtocolLogEvent) {
        if self.enabled(event.direction) {
            if let Some(logger) = &self.logger {
                logger(event).await;
            } else {
                tracing::debug!(?event, "ACP protocol event");
            }
        }
    }
}
/// The patched source parser decodes all complete frames in a chunk before
/// routing any message. Preserve split UTF-8 and fail the whole chunk if any
/// complete frame is malformed. The final unfinished frame is left buffered.
pub struct NdjsonDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
    initial_bom: bool,
    scanned: usize,
}
impl NdjsonDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_bytes,
            initial_bom: true,
            scanned: 0,
        }
    }
    pub fn decode(&mut self, chunk: &[u8]) -> Result<Vec<Value>, AcpError> {
        Ok(self
            .decode_frames(chunk)?
            .into_iter()
            .flat_map(|value| match value {
                Value::Array(batch) => batch.into_iter().filter(Value::is_object).collect(),
                value if value.is_object() => vec![value],
                _ => Vec::new(),
            })
            .collect())
    }
    /// Preserves JSON-RPC batch boundaries for transports that aggregate replies.
    pub fn decode_frames(&mut self, chunk: &[u8]) -> Result<Vec<Value>, AcpError> {
        self.buffer.extend_from_slice(chunk);
        if self.initial_bom {
            const BOM: &[u8] = &[0xef, 0xbb, 0xbf];
            if self.buffer.len() < BOM.len() && BOM.starts_with(&self.buffer) {
                return Ok(Vec::new());
            }
            if self.buffer.starts_with(BOM) {
                self.buffer.drain(..BOM.len());
            }
            self.initial_bom = false;
        }
        let mut start = 0;
        let mut frames = Vec::new();
        for end in self.scanned..self.buffer.len() {
            if self.buffer[end] != b'\n' {
                continue;
            }
            if end - start > self.max_frame_bytes {
                return Err(wire_parse_error("ACP input frame exceeds byte budget"));
            }
            let text = String::from_utf8_lossy(&self.buffer[start..end]);
            let value: Value =
                serde_json::from_str(&text).map_err(|error| wire_parse_error(error.to_string()))?;
            frames.push(value);
            start = end + 1;
        }
        if self.buffer.len() - start > self.max_frame_bytes {
            return Err(wire_parse_error("ACP input frame exceeds byte budget"));
        }
        self.buffer.drain(..start);
        self.scanned = self.buffer.len();
        Ok(frames)
    }
}
pub fn wire_parse_error(cause: impl Into<String>) -> AcpError {
    ProtocolParseError {
        operation: ProtocolParseOperation::DecodeWireMessage,
        method: None,
        request_id: None,
        issues: None,
        cause: FailureCause::Value(json!({"name":"SyntaxError","message":cause.into()})),
    }
    .into()
}
pub fn read_input_error(cause: impl Into<String>) -> AcpError {
    TransportError {
        operation: Some(TransportOperation::ReadInputStream),
        method: None,
        detail: None,
        pid: None,
        cause: FailureCause::Value(json!({"name":"Error","message":cause.into()})),
    }
    .into()
}
/// Effect's internal envelopes, used only for decoded observation. Empty
/// headers are present here and omitted by the actual JSON-RPC serializer.
pub fn decoded_message(wire: &Value) -> Value {
    let object = wire.as_object().expect("decoder emits only objects");
    if let Some(method) = object.get("method") {
        if object.get("id").is_none_or(Value::is_null) {
            if let Some(tag) = method
                .as_str()
                .and_then(|method| method.strip_prefix("@effect/rpc/"))
            {
                let mut message = json!({"_tag":tag});
                if let Some(id) = wire.pointer("/params/requestId") {
                    message["requestId"] = id.clone();
                }
                return message;
            }
        }
        let mut message = json!({"_tag":"Request","id":object.get("id").filter(|id|!id.is_null()).cloned().unwrap_or(json!("")),"tag":method,"payload":object.get("params").filter(|value|!value.is_null()).cloned().unwrap_or(Value::Null),"headers":object.get("headers").filter(|value|!value.is_null()).cloned().unwrap_or(json!([]))});
        if !object.contains_key("id") {
            message["isNotification"] = json!(true);
        }
        if object.get("spanId").is_some_and(js_truthy) {
            for key in ["traceId", "spanId", "sampled"] {
                if let Some(value) = object.get(key) {
                    message[key] = value.clone();
                }
            }
        }
        message
    } else if wire.pointer("/error/_tag").and_then(Value::as_str) == Some("Defect") {
        let mut message = json!({"_tag":"Defect"});
        if let Some(value) = wire.pointer("/error/data") {
            message["defect"] = value.clone();
        }
        message
    } else if object.get("chunk") == Some(&json!(true)) {
        let mut message = json!({"_tag":"Chunk","requestId":object.get("id").filter(|id|!id.is_null()).cloned().unwrap_or(json!(""))});
        if let Some(value) = object.get("result") {
            message["values"] = value.clone();
        }
        message
    } else {
        let exit = if let Some(error) = object.get("error").filter(|error| !error.is_null()) {
            let cause = if error.get("_tag").and_then(Value::as_str) == Some("Cause") {
                error.get("data").cloned()
            } else {
                Some(json!([{"_tag":"Fail","error":error}]))
            };
            let mut exit = json!({"_tag":"Failure"});
            if let Some(cause) = cause {
                exit["cause"] = cause;
            }
            exit
        } else {
            let mut exit = json!({"_tag":"Success"});
            if let Some(value) = object.get("result") {
                exit["value"] = value.clone();
            }
            exit
        };
        json!({"_tag":"Exit","requestId":object.get("id").filter(|id|!id.is_null()).cloned().unwrap_or(json!("")),"exit":exit})
    }
}
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}
/// Exact source key order, retained in raw logging as well as stdin bytes.
pub fn outgoing_wire(value: &Value) -> Value {
    let mut wire = json!({"jsonrpc":"2.0"});
    let keys: &[&str] = if value.get("method").is_some() {
        &["method", "params", "id"]
    } else {
        &["id", "result", "error"]
    };
    for key in keys {
        if let Some(value) = value.get(*key) {
            wire[*key] = value.clone();
        }
    }
    wire
}
pub fn outgoing_decoded(value: &Value) -> Value {
    if value.get("method").is_some() && value.get("id").is_none() {
        let mut message = json!({"_tag":"Notification","tag":value["method"]});
        if let Some(value) = value.get("params") {
            message["payload"] = value.clone();
        }
        message
    } else {
        decoded_message(value)
    }
}
