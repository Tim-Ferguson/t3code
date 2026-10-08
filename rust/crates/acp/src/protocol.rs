use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(Number),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectCauseTag {
    Cause,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum EffectCauseReason {
    Fail {
        error: Value,
    },
    Die {
        defect: Value,
    },
    Interrupt {
        #[serde(
            rename = "fiberId",
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_value"
        )]
        fiber_id: Option<Value>,
    },
}
fn present_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Ok(Some(Value::deserialize(d)?))
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectCause {
    #[serde(rename = "_tag")]
    pub tag: EffectCauseTag,
    pub code: i64,
    pub message: String,
    pub data: Vec<EffectCauseReason>,
}
fn error_code<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let value = Number::deserialize(d)?;
    let number = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("expected an ACP integer error code"))?;
    if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 {
        Ok(number as i64)
    } else {
        Err(serde::de::Error::custom(
            "expected an ACP safe integer error code",
        ))
    }
}
impl<'de> Deserialize<'de> for EffectCause {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            #[serde(rename = "_tag")]
            tag: EffectCauseTag,
            #[serde(deserialize_with = "error_code")]
            code: i64,
            message: String,
            data: Vec<EffectCauseReason>,
        }
        let value = Value::deserialize(d)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("expected an Effect Cause object"));
        }
        let fields: Fields = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            tag: fields.tag,
            code: fields.code,
            message: fields.message,
            data: fields.data,
        })
    }
}
impl EffectCause {
    /// Mirrors RpcSerialization.encodeJsonRpcMessage: the first Fail supplies
    /// the code/message; a defect-only cause uses zero and its encoded array.
    pub fn new(data: Vec<EffectCauseReason>) -> Self {
        let protocol = data
            .iter()
            .find(|reason| matches!(reason, EffectCauseReason::Fail { .. }))
            .and_then(|reason| {
                if let EffectCauseReason::Fail { error } = reason {
                    serde_json::from_value::<RpcError>(error.clone()).ok()
                } else {
                    None
                }
            });
        let (code, message) = protocol
            .map(|error| (error.code, error.message))
            .unwrap_or_else(|| (0, serde_json::to_string(&data).expect("JSON cause")));
        Self {
            tag: EffectCauseTag::Cause,
            code,
            message,
            data,
        }
    }
    pub fn defect(defect: Value) -> Self {
        Self::new(vec![EffectCauseReason::Die { defect }])
    }
    pub fn protocol_error(&self) -> Option<RpcError> {
        self.data
            .iter()
            .find(|reason| matches!(reason, EffectCauseReason::Fail { .. }))
            .and_then(|reason| {
                if let EffectCauseReason::Fail { error } = reason {
                    serde_json::from_value(error.clone()).ok()
                } else {
                    None
                }
            })
    }
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
pub fn js_number(number: &Number) -> String {
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
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("ACP request failed ({code}): {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl<'de> Deserialize<'de> for RpcError {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        fn data<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
            Ok(Some(Value::deserialize(d)?))
        }
        #[derive(Deserialize)]
        struct Fields {
            #[serde(deserialize_with = "error_code")]
            code: i64,
            message: String,
            #[serde(default, deserialize_with = "data")]
            data: Option<Value>,
        }
        let value = Value::deserialize(d)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "expected an ACP protocol error object",
            ));
        }
        let value: Fields = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            code: value.code,
            message: value.message,
            data: value.data,
        })
    }
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
    Failure(std::sync::Arc<crate::errors::Failure>),
    #[error("ACP response error for method '{method}': {error}")]
    ResponseError {
        method: String,
        request_id: RequestId,
        error: RpcError,
    },
    #[error("ACP remote defect for method '{method}'")]
    ResponseDefect {
        method: String,
        request_id: RequestId,
        cause: EffectCause,
        decode_error: Option<std::sync::Arc<crate::schema::SchemaError>>,
    },
    #[error("ACP response contains an Effect failure cause for method '{method}'")]
    ResponseCause {
        method: String,
        request_id: RequestId,
        cause: EffectCause,
    },
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
/// A transport releases a correlated reply only after the client has consumed
/// this barrier in wire order. It also gates pending failures from input end.
#[derive(Debug, Clone)]
pub struct IngressAcknowledgement(watch::Sender<bool>);
impl Default for IngressAcknowledgement {
    fn default() -> Self {
        Self::new()
    }
}
impl IngressAcknowledgement {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }
    pub fn acknowledge(&self) {
        self.0.send_replace(true);
    }
    pub async fn wait(&self) {
        let mut acknowledged = self.0.subscribe();
        while !*acknowledged.borrow_and_update() {
            if acknowledged.changed().await.is_err() {
                break;
            }
        }
    }
}
#[derive(Debug, Clone)]
pub enum PeerEvent {
    IngressBarrier {
        acknowledgement: IngressAcknowledgement,
    },
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
/// A response buffered inside an incoming JSON-RPC batch has not been written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDisposition {
    Written,
    Buffered,
}
/// Implementations own framing/correlation and must acknowledge writes after
/// completion, remove canceled request waiters, and propagate terminal errors.
/// Subscribe before initialization so no callback or update is lost.
pub trait Peer: Send + Sync + 'static {
    /// Output failure is independent of ordered input routing: a held input
    /// callback cannot delay failure of the writer and its pending requests.
    fn external_failure(&self) -> BoxFuture<'_, AcpError> {
        Box::pin(std::future::pending())
    }
    /// Called before subscribing or issuing requests. Ordered transports emit
    /// barriers before correlated replies and terminal pending failures, then
    /// await them before returning request results. The default preserves
    /// compatibility for simple peers that already sequence their own ingress.
    fn enable_ordered_ingress(&self) {}
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>>;
    /// Typed Effect RPC calls use a disjoint numeric ID namespace beginning at
    /// 2^32. Legacy peers may delegate when their transport owns its allocator.
    fn request_core<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>> {
        self.request(method, params, timeout)
    }
    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), AcpError>>;
    fn respond<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<(), AcpError>>;
    /// Batch-aware peers report only the final response as acknowledged.
    fn respond_observed<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<ResponseDisposition, AcpError>> {
        Box::pin(async move {
            self.respond(id, result)
                .await
                .map(|()| ResponseDisposition::Written)
        })
    }
    fn respond_cause_observed<'a>(
        &'a self,
        id: RequestId,
        cause: EffectCause,
    ) -> BoxFuture<'a, Result<ResponseDisposition, AcpError>> {
        Box::pin(async move {
            self.respond_cause(id, cause)
                .await
                .map(|()| ResponseDisposition::Written)
        })
    }
    fn subscribe(&self) -> broadcast::Receiver<PeerEvent>;
    /// Effect's private Cause response is distinct from a standard ACP error.
    /// Transports supporting source agent core defect parity override this.
    fn respond_cause<'a>(
        &'a self,
        _id: RequestId,
        _cause: EffectCause,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async {
            Err(crate::errors::TransportError {
                operation: None,
                method: None,
                detail: Some("Peer does not support Effect Cause responses".into()),
                pid: None,
                cause: Value::Null.into(),
            }
            .into())
        })
    }
}
