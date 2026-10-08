//! Source ACP error classes and factories. Causes remain typed for callers;
//! protocol replies and Display expose only stable, source-defined fields.
use crate::{
    AcpError, RequestId, RpcError,
    schema::{IssueDiagnostics, SchemaError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::{fmt, sync::Arc};

#[derive(Debug, Clone)]
pub enum FailureCause {
    Value(Value),
    Schema(Arc<SchemaError>),
    Error(Arc<AcpError>),
}
impl From<Value> for FailureCause {
    fn from(value: Value) -> Self {
        Self::Value(value)
    }
}
impl From<SchemaError> for FailureCause {
    fn from(value: SchemaError) -> Self {
        Self::Schema(Arc::new(value))
    }
}
impl From<AcpError> for FailureCause {
    fn from(value: AcpError) -> Self {
        Self::Error(Arc::new(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestOperation {
    DecodeExtensionRequestPayload,
    EncodeExtensionResponse,
    HandleRequest,
    HandleExtensionRequest,
    ReceiveResponse,
    ReceiveStreamingResponse,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProtocolParseOperation {
    EncodeMessage,
    DecodeWireMessage,
    DecodeNotificationPayload,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportOperation {
    CallRpc,
    ReadInputStream,
    ReadProcessExitStatus,
}
fn operation_name<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("operation enum")
        .as_str()
        .unwrap()
        .into()
}

#[derive(Debug, Clone, Default)]
pub struct RequestDiagnostics {
    pub method: Option<String>,
    pub request_id: Option<RequestId>,
    pub operation: Option<RequestOperation>,
    pub issues: Option<IssueDiagnostics>,
    pub cause: Option<FailureCause>,
}
#[derive(Debug, Clone)]
pub struct RequestError {
    pub code: i64,
    pub error_message: String,
    pub data: Option<Value>,
    pub diagnostics: RequestDiagnostics,
}
impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error_message)
    }
}
impl std::error::Error for RequestError {}
impl RequestError {
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            error_message: message.into(),
            data,
            diagnostics: RequestDiagnostics::default(),
        }
    }
    pub fn parse_error(message: Option<&str>, data: Option<Value>) -> Self {
        Self::new(-32700, message.unwrap_or("Parse error"), data)
    }
    pub fn invalid_request(message: Option<&str>, data: Option<Value>) -> Self {
        Self::new(-32600, message.unwrap_or("Invalid request"), data)
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {method}"), None)
    }
    pub fn invalid_params(message: Option<&str>, data: Option<Value>) -> Self {
        Self::new(-32602, message.unwrap_or("Invalid params"), data)
    }
    pub fn internal_error(
        message: Option<&str>,
        data: Option<Value>,
        diagnostics: RequestDiagnostics,
    ) -> Self {
        Self {
            diagnostics,
            ..Self::new(-32603, message.unwrap_or("Internal error"), data)
        }
    }
    pub fn auth_required(message: Option<&str>, data: Option<Value>) -> Self {
        Self::new(-32000, message.unwrap_or("Authentication required"), data)
    }
    pub fn resource_not_found(message: Option<&str>, data: Option<Value>) -> Self {
        Self::new(-32002, message.unwrap_or("Resource not found"), data)
    }
    pub fn from_protocol_error(
        error: RpcError,
        method: &str,
        request_id: Option<RequestId>,
        cause: Option<FailureCause>,
    ) -> Self {
        let cause = cause.or_else(|| {
            Some(FailureCause::Value(
                serde_json::to_value(&error).expect("protocol error"),
            ))
        });
        Self {
            code: error.code,
            error_message: error.message,
            data: error.data,
            diagnostics: RequestDiagnostics {
                method: Some(method.into()),
                request_id,
                operation: Some(RequestOperation::ReceiveResponse),
                cause,
                ..Default::default()
            },
        }
    }
    pub fn invalid_extension_payload(method: &str, cause: SchemaError) -> Self {
        let issues = cause.issue.diagnostics();
        Self {
            code: -32602,
            error_message: format!("Invalid payload for ACP extension method '{method}'."),
            data: Some(serde_json::to_value(&issues).expect("issue diagnostics")),
            diagnostics: RequestDiagnostics {
                method: Some(method.into()),
                operation: Some(RequestOperation::DecodeExtensionRequestPayload),
                issues: Some(issues),
                cause: Some(cause.into()),
                ..Default::default()
            },
        }
    }
    pub fn from_extension_response_failure(
        method: &str,
        request_id: RequestId,
        cause: FailureCause,
    ) -> Self {
        Self::internal_error(
            Some("Extension request failed"),
            None,
            RequestDiagnostics {
                method: Some(method.into()),
                request_id: Some(request_id),
                operation: Some(RequestOperation::ReceiveResponse),
                cause: Some(cause),
                ..Default::default()
            },
        )
    }
    pub fn from_extension_response_encoding_error(
        method: &str,
        request_id: RequestId,
        cause: ProtocolParseError,
    ) -> Self {
        Self::internal_error(
            None,
            None,
            RequestDiagnostics {
                method: Some(method.into()),
                request_id: Some(request_id),
                operation: Some(RequestOperation::EncodeExtensionResponse),
                cause: Some(AcpError::from(cause).into()),
                ..Default::default()
            },
        )
    }
    pub fn unsupported_streaming_response(method: &str, request_id: RequestId) -> Self {
        Self::internal_error(
            Some("Streaming extension responses are not supported"),
            None,
            RequestDiagnostics {
                method: Some(method.into()),
                request_id: Some(request_id),
                operation: Some(RequestOperation::ReceiveStreamingResponse),
                ..Default::default()
            },
        )
    }
    pub fn from_core_handler_error(error: AcpError, method: &str) -> Self {
        Self::from_handler_error(error, method, false)
    }
    pub fn from_extension_handler_error(error: AcpError, method: &str) -> Self {
        Self::from_handler_error(error, method, true)
    }
    fn from_handler_error(error: AcpError, method: &str, extension: bool) -> Self {
        match &error {
            AcpError::Request(error) => {
                return Self::new(error.code, error.message.clone(), error.data.clone());
            }
            AcpError::Failure(failure) => {
                if let Failure::Request(error) = failure.as_ref() {
                    return error.as_ref().clone();
                }
            }
            _ => {}
        }
        Self::internal_error(
            Some(&format!(
                "ACP {}request handler failed for method '{method}'",
                if extension { "extension " } else { "" }
            )),
            None,
            RequestDiagnostics {
                method: Some(method.into()),
                operation: Some(if extension {
                    RequestOperation::HandleExtensionRequest
                } else {
                    RequestOperation::HandleRequest
                }),
                cause: Some(error.into()),
                ..Default::default()
            },
        )
    }
    pub fn to_protocol_error(&self) -> RpcError {
        RpcError {
            code: self.code,
            message: self.error_message.clone(),
            data: self.data.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpawnError {
    pub command: Option<String>,
    pub cause: FailureCause,
}
impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Failed to spawn ACP process")?;
        if let Some(command) = &self.command {
            if !command.is_empty() {
                write!(f, " for command: {command}")?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for SpawnError {}
#[derive(Debug, Clone)]
pub struct ProcessExitedError {
    pub code: Option<Number>,
    pub pid: Option<i64>,
    pub stderr: Option<String>,
    pub cause: Option<FailureCause>,
}
impl fmt::Display for ProcessExitedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ACP process exited")?;
        if let Some(code) = &self.code {
            write!(f, " with code {}", crate::protocol::js_number(code))?;
        }
        if let Some(stderr) = &self.stderr {
            let excerpt = stderr.trim_matches(js_whitespace);
            if !excerpt.is_empty() {
                write!(f, "\n{excerpt}")?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for ProcessExitedError {}
fn js_whitespace(c: char) -> bool {
    matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
#[derive(Debug, Clone)]
pub struct ProtocolParseError {
    pub operation: ProtocolParseOperation,
    pub method: Option<String>,
    pub request_id: Option<RequestId>,
    pub issues: Option<IssueDiagnostics>,
    pub cause: FailureCause,
}
impl ProtocolParseError {
    pub fn from_schema_error(
        operation: ProtocolParseOperation,
        method: &str,
        cause: SchemaError,
    ) -> Self {
        Self {
            operation,
            method: Some(method.into()),
            request_id: None,
            issues: Some(cause.issue.diagnostics()),
            cause: cause.into(),
        }
    }
    pub fn from_encoding_error(
        method: Option<String>,
        request_id: Option<RequestId>,
        cause: FailureCause,
    ) -> Self {
        Self {
            operation: ProtocolParseOperation::EncodeMessage,
            method,
            request_id,
            issues: None,
            cause,
        }
    }
}
impl fmt::Display for ProtocolParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ACP protocol operation '{}' failed",
            operation_name(self.operation)
        )?;
        if let Some(method) = &self.method {
            write!(f, " for method '{method}'")?;
        }
        f.write_str(".")
    }
}
impl std::error::Error for ProtocolParseError {}
#[derive(Debug, Clone)]
pub struct TransportError {
    pub operation: Option<TransportOperation>,
    pub method: Option<String>,
    pub detail: Option<String>,
    pub pid: Option<i64>,
    pub cause: FailureCause,
}
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(operation) = self.operation {
            write!(
                f,
                "ACP transport operation {} failed",
                operation_name(operation)
            )?;
            if let Some(method) = &self.method {
                if !method.is_empty() {
                    write!(f, " for method {method}")?;
                }
            }
            f.write_str(".")
        } else {
            f.write_str("ACP transport operation failed.")
        }
    }
}
impl std::error::Error for TransportError {}
#[derive(Debug, Clone, thiserror::Error)]
#[error("ACP input stream ended.")]
pub struct InputStreamEndedError;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Failure {
    #[error(transparent)]
    Request(Box<RequestError>),
    #[error(transparent)]
    Spawn(SpawnError),
    #[error(transparent)]
    ProcessExited(ProcessExitedError),
    #[error(transparent)]
    ProtocolParse(ProtocolParseError),
    #[error(transparent)]
    Transport(TransportError),
    #[error(transparent)]
    InputStreamEnded(InputStreamEndedError),
}
macro_rules! conversions {($(($type:ty,$variant:ident)),*$(,)?)=>{$(impl From<$type> for AcpError{fn from(error:$type)->Self{Self::Failure(Arc::new(Failure::$variant(error)))}})*};}
conversions! {(SpawnError,Spawn),(ProcessExitedError,ProcessExited),(ProtocolParseError,ProtocolParse),(TransportError,Transport),(InputStreamEndedError,InputStreamEnded)}
impl From<RequestError> for AcpError {
    fn from(error: RequestError) -> Self {
        Self::Failure(Arc::new(Failure::Request(Box::new(error))))
    }
}
