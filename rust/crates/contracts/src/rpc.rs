//! Effect 4 RPC transport envelopes, from .repos/effect-smol/src/rpc/RpcMessage.ts.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcRequestId {
    String(String),
    Number(serde_json::Number),
}
impl From<String> for RpcRequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<&str> for RpcRequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
impl From<u64> for RpcRequestId {
    fn from(value: u64) -> Self {
        Self::Number(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRequest {
    pub id: RpcRequestId,
    pub tag: String,
    pub payload: Value,
    pub headers: Vec<(String, String)>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "notification_flag"
    )]
    pub is_notification: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled: Option<bool>,
}
fn notification_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    if bool::deserialize(d)? {
        Ok(Some(true))
    } else {
        Err(serde::de::Error::custom(
            "isNotification must be true when present",
        ))
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum RpcClientMessage {
    Request {
        #[serde(flatten)]
        request: RpcRequest,
    },
    Ack {
        request_id: RpcRequestId,
    },
    Interrupt {
        request_id: RpcRequestId,
    },
    Ping,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum RpcServerMessage {
    Request {
        #[serde(flatten)]
        request: RpcRequest,
    },
    Chunk {
        request_id: RpcRequestId,
        #[serde(deserialize_with = "non_empty_chunk")]
        values: Vec<Value>,
    },
    Exit {
        request_id: RpcRequestId,
        exit: RpcExit,
    },
    Defect {
        defect: Value,
    },
    Pong,
    ClientProtocolError {
        error: Value,
    },
}
fn non_empty_chunk<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
    let values = Vec::<Value>::deserialize(d)?;
    if values.is_empty() {
        Err(serde::de::Error::custom(
            "a response chunk must contain a value",
        ))
    } else {
        Ok(values)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum RpcExit {
    Success { value: Value },
    Failure { cause: Vec<RpcCause> },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum RpcCause {
    Fail {
        error: Value,
    },
    Die {
        defect: Value,
    },
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fiber_id: Option<serde_json::Number>,
    },
}
