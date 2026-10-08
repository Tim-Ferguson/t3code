use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(Number),
}
impl RequestId {
    /// Stable callback identity without conflating numeric and string IDs.
    pub fn identity(&self) -> String {
        const PREFIX: &str = "$t3:jsonrpc:";
        match self {
            Self::Number(n) => format!("{PREFIX}number:{}", js_number(n)),
            Self::String(s) if s.starts_with(PREFIX) => format!("{PREFIX}string:{s}"),
            Self::String(s) => s.clone(),
        }
    }
}
fn js_number(number: &Number) -> String {
    let n = number.as_f64().expect("finite JSON number");
    if n == 0.0 {
        return "0".into();
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let formatted = format!("{n:e}");
        let (mantissa, exponent) = formatted.split_once('e').unwrap();
        let exponent: i32 = exponent.parse().unwrap();
        format!(
            "{mantissa}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        )
    } else {
        n.to_string()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("ACP request failed ({code}): {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }
    pub fn invalid_params() -> Self {
        Self {
            code: -32602,
            message: "Invalid params".into(),
            data: None,
        }
    }
    pub fn internal() -> Self {
        Self {
            code: -32603,
            message: "Internal error".into(),
            data: None,
        }
    }
}
#[derive(Debug, Clone, thiserror::Error)]
pub enum AcpError {
    #[error(transparent)]
    Request(#[from] RpcError),
    #[error(transparent)]
    Schema(#[from] crate::schema::SchemaError),
    #[error("ACP transport failed: {0}")]
    Transport(String),
    #[error("ACP process ended: {message}")]
    ProcessExited {
        code: Option<i64>,
        pid: Option<u32>,
        message: String,
    },
    #[error("ACP request {method} timed out")]
    Timeout { method: String },
    #[error("ACP client was closed")]
    Closed,
}
#[derive(Debug, Clone)]
pub enum PeerEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    Closed(AcpError),
}
/// Implementations own framing/correlation and must acknowledge writes after
/// completion, remove canceled request waiters, and propagate terminal errors.
/// Subscribe before initialization so no callback or update is lost.
pub trait Peer: Send + Sync + 'static {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>>;
    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), AcpError>>;
    fn respond<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<(), AcpError>>;
    fn subscribe(&self) -> broadcast::Receiver<PeerEvent>;
}
