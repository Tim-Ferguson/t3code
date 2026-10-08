use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_contracts::{RpcClientMessage, RpcExit, RpcRequest, RpcRequestId, RpcServerMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Unary,
    Stream,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub method: String,
    pub kind: RequestKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcEvent {
    Values {
        id: String,
        method: String,
        values: Vec<Value>,
    },
    Complete {
        id: String,
        method: String,
        value: Value,
    },
    Failed {
        id: String,
        method: String,
        cause: Value,
    },
    Pong,
    Notification {
        method: String,
        payload: Value,
    },
    Defect(Value),
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("Invalid RPC JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid RPC envelope: {0}")]
    Invalid(&'static str),
}

/// A new instance is created for each socket generation. Pending mutations are
/// never replayed automatically after reconnect, because acceptance is uncertain.
#[derive(Debug, Default)]
pub struct RpcSession {
    next_id: u64,
    pending: BTreeMap<String, PendingRequest>,
}

impl RpcSession {
    pub fn request(
        &mut self,
        method: impl Into<String>,
        payload: Value,
        kind: RequestKind,
    ) -> (String, Value) {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let method = method.into();
        let frame = serde_json::to_value(RpcClientMessage::Request {
            request: RpcRequest {
                id: id.clone().into(),
                tag: method.clone(),
                payload,
                headers: vec![],
                is_notification: None,
                trace_id: None,
                span_id: None,
                sampled: None,
            },
        })
        .expect("RPC contract is JSON serializable");
        self.pending
            .insert(id.clone(), PendingRequest { method, kind });
        (id, frame)
    }

    pub fn cancel(&mut self, id: &str) -> Option<Value> {
        self.pending
            .remove(id)
            .map(|_| json!({"_tag":"Interrupt", "requestId":id}))
    }

    pub fn disconnect(&mut self) -> Vec<RpcEvent> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|(id, request)| RpcEvent::Failed {
                id,
                method: request.method,
                cause: json!({"message":"Connection closed before acknowledgement"}),
            })
            .collect()
    }

    /// Socket serialization may deliver a single envelope or a batch.
    pub fn receive(&mut self, text: &str) -> Result<(Vec<RpcEvent>, Vec<Value>), ProtocolError> {
        let value: Value = serde_json::from_str(text)?;
        let frames = if let Value::Array(frames) = value {
            frames
        } else {
            vec![value]
        };
        let mut events = Vec::new();
        let mut outgoing = Vec::new();
        for frame in frames {
            match serde_json::from_value::<RpcServerMessage>(frame)? {
                RpcServerMessage::Pong => events.push(RpcEvent::Pong),
                RpcServerMessage::Defect { defect } => events.push(RpcEvent::Defect(defect)),
                RpcServerMessage::ClientProtocolError { error } => {
                    events.push(RpcEvent::Defect(error))
                }
                RpcServerMessage::Request { request } => events.push(RpcEvent::Notification {
                    method: request.tag,
                    payload: request.payload,
                }),
                RpcServerMessage::Chunk { request_id, values } => {
                    let id = request_id_string(&request_id);
                    outgoing.push(
                        serde_json::to_value(RpcClientMessage::Ack { request_id })
                            .expect("RPC ACK is serializable"),
                    );
                    if let Some(request) = self.pending.get(&id) {
                        events.push(RpcEvent::Values {
                            id,
                            method: request.method.clone(),
                            values,
                        });
                    }
                }
                RpcServerMessage::Exit { request_id, exit } => {
                    let id = request_id_string(&request_id);
                    if let Some(request) = self.pending.remove(&id) {
                        events.push(match exit {
                            RpcExit::Success { value } => RpcEvent::Complete {
                                id,
                                method: request.method,
                                value,
                            },
                            RpcExit::Failure { cause } => RpcEvent::Failed {
                                id,
                                method: request.method,
                                cause: serde_json::to_value(cause)
                                    .expect("RPC cause is serializable"),
                            },
                        });
                    }
                }
            }
        }
        Ok((events, outgoing))
    }
}

fn request_id_string(id: &RpcRequestId) -> String {
    match id {
        RpcRequestId::String(id) => id.clone(),
        RpcRequestId::Number(id) => id.to_string(),
    }
}
