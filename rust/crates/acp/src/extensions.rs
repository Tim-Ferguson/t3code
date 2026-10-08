//! Rust codecs for application-defined ACP extensions. Callers can provide
//! custom pure Rust validators; built-in ACP schemas use the same Wire codecs.
use crate::{
    AcpError, Client, ClientEvent, RequestContext,
    errors::{ProtocolParseError, ProtocolParseOperation, RequestError},
    schema::{SchemaError, SchemaName, Wire},
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub struct PayloadCodec<T> {
    decode: Arc<dyn Fn(Value) -> Result<T, SchemaError> + Send + Sync>,
    encode: Arc<dyn Fn(T) -> Result<Value, SchemaError> + Send + Sync>,
}
impl<T> Clone for PayloadCodec<T> {
    fn clone(&self) -> Self {
        Self {
            decode: self.decode.clone(),
            encode: self.encode.clone(),
        }
    }
}
impl<T> PayloadCodec<T> {
    pub fn new(
        decode: impl Fn(Value) -> Result<T, SchemaError> + Send + Sync + 'static,
        encode: impl Fn(T) -> Result<Value, SchemaError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            decode: Arc::new(decode),
            encode: Arc::new(encode),
        }
    }
    pub fn decode(&self, value: Value) -> Result<T, SchemaError> {
        (self.decode)(value)
    }
    pub fn encode(&self, value: T) -> Result<Value, SchemaError> {
        (self.encode)(value)
    }
}
impl<S: SchemaName + 'static> PayloadCodec<Wire<S>> {
    pub fn wire() -> Self {
        Self::new(Wire::<S>::decode, |value| Ok(value.into_value()))
    }
}
impl Client {
    pub fn handle_extension<Q: Send + 'static, R: Send + 'static>(
        &self,
        method: &str,
        request: PayloadCodec<Q>,
        response: PayloadCodec<R>,
        handler: Arc<
            dyn Fn(Q, RequestContext) -> BoxFuture<'static, Result<R, AcpError>> + Send + Sync,
        >,
    ) {
        let events = self.handler_events();
        let name = method.to_owned();
        self.handle_extension_request(
            method,
            Arc::new(move |value, context| {
                let request = request.clone();
                let response = response.clone();
                let handler = handler.clone();
                let events = events.clone();
                let name = name.clone();
                Box::pin(async move {
                    let result = async {
                        let payload = request.decode(value).map_err(|cause| {
                            RequestError::invalid_extension_payload(&name, cause)
                        })?;
                        let reply = handler(payload, context.clone()).await.map_err(|cause| {
                            RequestError::from_extension_handler_error(cause, &name)
                        })?;
                        response.encode(reply).map_err(|cause| {
                            RequestError::from_extension_response_encoding_error(
                                &name,
                                context.wire_id.clone(),
                                ProtocolParseError::from_encoding_error(
                                    Some(name.clone()),
                                    Some(context.wire_id.clone()),
                                    cause.into(),
                                ),
                            )
                        })
                    }
                    .await;
                    result.map_err(|error| {
                        let protocol = error.to_protocol_error();
                        let _ = events.send(ClientEvent::RequestHandlerFailed {
                            request_id: context.request_id,
                            error: error.into(),
                        });
                        protocol
                    })
                })
            }),
        );
    }
    pub async fn handle_extension_notification<Q: Send + 'static>(
        &self,
        method: &str,
        payload: PayloadCodec<Q>,
        handler: Arc<dyn Fn(Q) -> BoxFuture<'static, Result<(), AcpError>> + Send + Sync>,
    ) {
        let name = method.to_owned();
        self.handle_notification(
            method,
            Arc::new(move |value| {
                let payload = payload.clone();
                let handler = handler.clone();
                let name = name.clone();
                Box::pin(async move {
                    let decoded = payload.decode(value).map_err(|cause| {
                        AcpError::from(ProtocolParseError::from_schema_error(
                            ProtocolParseOperation::DecodeNotificationPayload,
                            &name,
                            cause,
                        ))
                    })?;
                    handler(decoded).await
                })
            }),
        )
        .await;
    }
    pub async fn extension_request(&self, method: &str, payload: Value) -> Result<Value, AcpError> {
        self.raw_request(method, payload)
            .await
            .map_err(|error| match error {
                AcpError::ResponseError {
                    method,
                    request_id,
                    error,
                } => {
                    let cause = serde_json::to_value(vec![crate::EffectCauseReason::Fail {
                        error: serde_json::to_value(&error).expect("protocol error"),
                    }])
                    .expect("JSON cause");
                    RequestError::from_protocol_error(
                        error,
                        &method,
                        Some(request_id),
                        Some(cause.into()),
                    )
                    .into()
                }
                AcpError::ResponseCause {
                    method,
                    request_id,
                    cause,
                } => {
                    if let Some(error) = cause.protocol_error() {
                        RequestError::from_protocol_error(
                            error,
                            &method,
                            Some(request_id),
                            Some(
                                serde_json::to_value(&cause.data)
                                    .expect("JSON cause")
                                    .into(),
                            ),
                        )
                        .into()
                    } else {
                        RequestError::from_extension_response_failure(
                            &method,
                            request_id,
                            serde_json::to_value(&cause.data)
                                .expect("JSON cause")
                                .into(),
                        )
                        .into()
                    }
                }
                AcpError::Request(error) => {
                    RequestError::from_protocol_error(error, method, None, None).into()
                }
                other => other,
            })
    }
    pub async fn extension_notify(&self, method: &str, payload: Value) -> Result<(), AcpError> {
        self.raw_notify(method, payload).await
    }
}
impl crate::Agent {
    pub fn handle_extension<Q: Send + 'static, R: Send + 'static>(
        &self,
        method: &str,
        request: PayloadCodec<Q>,
        response: PayloadCodec<R>,
        handler: Arc<
            dyn Fn(Q, RequestContext) -> BoxFuture<'static, Result<R, AcpError>> + Send + Sync,
        >,
    ) {
        self.connection()
            .handle_extension(method, request, response, handler);
    }
    pub async fn handle_extension_notification<Q: Send + 'static>(
        &self,
        method: &str,
        payload: PayloadCodec<Q>,
        handler: Arc<dyn Fn(Q) -> BoxFuture<'static, Result<(), AcpError>> + Send + Sync>,
    ) {
        self.connection()
            .handle_extension_notification(method, payload, handler)
            .await;
    }
}
