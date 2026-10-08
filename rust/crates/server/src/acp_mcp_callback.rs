//! Negotiated MCP-over-ACP callbacks, backed by the same authenticated HTTP
//! endpoint as the native stdio bridge. Connection state is transport-local.
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use t3_acp::{AcpError, Client, RpcError};
use tokio::sync::{Mutex as AsyncMutex, watch};

const MAX_CONNECTIONS: usize = 16;
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Metadata {
    session: Option<String>,
    version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Method, StatusCode},
        response::{IntoResponse, Response},
        routing::any,
    };
    use tokio::sync::mpsc;

    type Recorded = (Method, HeaderMap, Value);
    async fn fixture(
        State(sent): State<mpsc::UnboundedSender<Recorded>>,
        method: Method,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let value: Value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&body).unwrap()
        };
        sent.send((method.clone(), headers, value.clone())).unwrap();
        if method == Method::DELETE {
            return StatusCode::NOT_FOUND.into_response();
        }
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        match value["method"].as_str().unwrap() {
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "http-error" => StatusCode::FORBIDDEN.into_response(),
            "wrong-id" => axum::Json(json!({"id":id.to_string(),"result":{}})).into_response(),
            "error-null" => axum::Json(json!({"id":id,"error":null})).into_response(),
            "error-message" => axum::Json(json!({"id":id,"error":{"message":"Exact provider error"}})).into_response(),
            "null" => axum::Json(json!({"id":id})).into_response(),
            "initialize" => Response::builder().header("content-type", "text/event-stream").header("mcp-session-id", "session-42")
                .body(Body::from(format!("data: {}\n\ndata: {}\n\n",json!({"id":"unrelated","result":{}}),json!({"id":id.as_u64().unwrap() as f64,"result":{"protocolVersion":"2025-06-18"}})))).unwrap(),
            _ => axum::Json(json!({"id":id,"result":{"tools":[]}})).into_response(),
        }
    }
    async fn server() -> (
        Bridge,
        mpsc::UnboundedReceiver<Recorded>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sent, receiver) = mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::new(
            format!("http://{}/mcp", listener.local_addr().unwrap()),
            "Bearer isolated-test".into(),
        );
        let router = Router::new().route("/mcp", any(fixture)).with_state(sent);
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (bridge, receiver, task)
    }
    #[tokio::test]
    async fn source_authenticated_session_headers_opaque_sse_and_disconnect() {
        let (bridge, mut recorded, task) = server().await;
        let id = bridge.connect("t3-code").unwrap()["connectionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let result = bridge
            .message(&json!({"connectionId":id,"method":"initialize","params":null}))
            .await
            .unwrap();
        assert_eq!(result, json!({"protocolVersion":"2025-06-18"}));
        let (_, headers, body) = recorded.recv().await.unwrap();
        assert_eq!(headers["authorization"], "Bearer isolated-test");
        assert!(body.get("params").is_none());
        assert_eq!(body["id"], 1);
        assert_eq!(
            bridge
                .message(&json!({"connectionId":id,"method":"tools/list","params":[1,true]}))
                .await
                .unwrap(),
            json!({"tools":[]})
        );
        let (_, headers, body) = recorded.recv().await.unwrap();
        assert_eq!(headers["mcp-session-id"], "session-42");
        assert_eq!(headers["mcp-protocol-version"], "2025-06-18");
        assert_eq!(body["params"], json!([1, true]));
        bridge
            .notification(&json!({"connectionId":id,"method":"notifications/initialized"}))
            .await
            .unwrap();
        assert!(recorded.recv().await.unwrap().2.get("id").is_none());
        bridge.disconnect(&id).await.unwrap();
        let (method, headers, _) = recorded.recv().await.unwrap();
        assert_eq!(method, Method::DELETE);
        assert_eq!(headers["mcp-session-id"], "session-42");
        assert_eq!(headers["mcp-protocol-version"], "2025-06-18");
        assert!(bridge.connection(&id).is_err());
        task.abort();
        task.await.unwrap_err();
    }
    #[tokio::test]
    async fn source_limits_and_error_response_correlation() {
        let (bridge, _recorded, task) = server().await;
        assert!(
            bridge
                .connect("other")
                .unwrap_err()
                .to_string()
                .contains("Unknown ACP MCP server")
        );
        assert!(
            bridge
                .message(&json!({"connectionId":"missing","method":"tools/list"}))
                .await
                .unwrap_err()
                .to_string()
                .contains("Unknown MCP-over-ACP connection")
        );
        let id = bridge.connect("t3-code").unwrap()["connectionId"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 1..MAX_CONNECTIONS {
            bridge.connect("t3-code").unwrap();
        }
        assert!(
            bridge
                .connect("t3-code")
                .unwrap_err()
                .to_string()
                .contains("Too many")
        );
        let large = json!({"connectionId":id,"method":"tools/call","params":{"payload":"x".repeat(MAX_MESSAGE_BYTES)}});
        assert!(
            bridge
                .message(&large)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 8 MiB")
        );
        for (method, expected) in [
            ("wrong-id", "matching response"),
            ("error-null", "The MCP server returned an error."),
            ("error-message", "Exact provider error"),
            ("http-error", "HTTP 403"),
        ] {
            assert!(
                bridge
                    .message(&json!({"connectionId":id,"method":method}))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
        assert_eq!(
            bridge
                .message(&json!({"connectionId":id,"method":"null"}))
                .await
                .unwrap(),
            Value::Null
        );
        bridge.dispose().await;
        assert!(bridge.connect("t3-code").is_err());
        task.abort();
        task.await.unwrap_err();
    }
    #[tokio::test]
    async fn interrupted_http_request_is_dropped_before_owned_disposal_returns() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bridge = Bridge::new(
                format!("http://{}/mcp", listener.local_addr().unwrap()),
                "Bearer isolated-test".into(),
            );
            let id = bridge.connect("t3-code").unwrap()["connectionId"]
                .as_str()
                .unwrap()
                .to_owned();
            let owned = bridge.clone();
            let request = tokio::spawn(async move {
                owned
                    .message(&json!({"connectionId":id,"method":"tools/list"}))
                    .await
            });
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let size = socket.read(&mut bytes).await.unwrap();
            assert!(size > 0);
            // Acceptance of actual request bytes is the admission milestone;
            // never send response headers, keeping reqwest inside owned I/O.
            bridge.dispose().await;
            assert!(matches!(request.await.unwrap(), Err(AcpError::Closed)));
            assert_eq!(socket.read(&mut bytes).await.unwrap(), 0);
            socket.shutdown().await.unwrap();
        })
        .await
        .unwrap();
    }
}
#[derive(Default)]
struct Connection {
    sending: AsyncMutex<()>,
    metadata: Mutex<Metadata>,
    next_id: AtomicU64,
}
struct Inner {
    endpoint: String,
    authorization: String,
    http: reqwest::Client,
    connections: Mutex<IndexMap<String, Arc<Connection>>>,
    stopped: watch::Sender<bool>,
    disposal: AsyncMutex<()>,
}
#[derive(Clone)]
pub(crate) struct Bridge(Arc<Inner>);

fn failure(message: impl Into<String>) -> AcpError {
    RpcError {
        code: -32603,
        message: message.into(),
        data: None,
    }
    .into()
}
impl Bridge {
    pub(crate) fn new(endpoint: String, authorization: String) -> Self {
        Self(Arc::new(Inner {
            endpoint,
            authorization,
            http: reqwest::Client::new(),
            connections: Default::default(),
            stopped: watch::channel(false).0,
            disposal: Default::default(),
        }))
    }
    pub(crate) async fn register(&self, client: &Client) {
        let bridge = self.clone();
        client.handle_mcp_connect(Arc::new(move |request, _| {
            let bridge = bridge.clone();
            Box::pin(async move {
                let result = bridge.connect(request.as_value()["serverId"].as_str().unwrap())?;
                serde_json::from_value(result).map_err(|error| failure(error.to_string()))
            })
        }));
        let bridge = self.clone();
        client.handle_mcp_message(Arc::new(move |request, _| {
            let bridge = bridge.clone();
            Box::pin(async move {
                let result = bridge.message(request.as_value()).await?;
                serde_json::from_value(result).map_err(|error| failure(error.to_string()))
            })
        }));
        let bridge = self.clone();
        client.handle_mcp_disconnect(Arc::new(move |request, _| {
            let bridge = bridge.clone();
            Box::pin(async move {
                bridge
                    .disconnect(request.as_value()["connectionId"].as_str().unwrap())
                    .await?;
                serde_json::from_value(json!({})).map_err(|error| failure(error.to_string()))
            })
        }));
        let bridge = self.clone();
        client
            .handle_mcp_notification(Arc::new(move |request| {
                let bridge = bridge.clone();
                Box::pin(async move { bridge.notification(request.as_value()).await })
            }))
            .await;
    }
    fn connect(&self, server: &str) -> Result<Value, AcpError> {
        if server != "t3-code" {
            return Err(failure(format!("Unknown ACP MCP server \"{server}\".")));
        }
        let mut connections = self.0.connections.lock().unwrap();
        if *self.0.stopped.borrow() {
            return Err(AcpError::Closed);
        }
        if connections.len() >= MAX_CONNECTIONS {
            return Err(failure("Too many MCP-over-ACP connections."));
        }
        let id = uuid::Uuid::new_v4().to_string();
        connections.insert(id.clone(), Arc::new(Connection::default()));
        Ok(json!({"connectionId":id}))
    }
    fn connection(&self, id: &str) -> Result<Arc<Connection>, AcpError> {
        self.0
            .connections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| failure(format!("Unknown MCP-over-ACP connection \"{id}\".")))
    }
    async fn send(&self, connection: &Connection, envelope: Value) -> Result<Vec<Value>, AcpError> {
        let mut stopped = self.0.stopped.subscribe();
        if *stopped.borrow() {
            return Err(AcpError::Closed);
        }
        tokio::select! {
            biased;
            _ = stopped.changed() => Err(AcpError::Closed),
            result = async {
                let _sending = connection.sending.lock().await;
                let body = crate::device_actions::stringify(&envelope);
                if body.len() > MAX_MESSAGE_BYTES { return Err(failure("MCP-over-ACP message exceeds 8 MiB.")); }
                // Fetch rejects URL credentials; avoid reqwest's synthesized Basic authentication.
                let url = url::Url::parse(&self.0.endpoint).map_err(|error| failure(error.to_string()))?;
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(failure("Request cannot be constructed from a URL that includes credentials"));
                }
                let mut request = self.0.http.post(url)
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("authorization", &self.0.authorization).body(body);
                {
                    let metadata = connection.metadata.lock().unwrap();
                    if let Some(session) = &metadata.session { request = request.header("mcp-session-id", session); }
                    if let Some(version) = &metadata.version { request = request.header("mcp-protocol-version", version); }
                }
                let response = request.send().await.map_err(|error| failure(error.without_url().to_string()))?;
                if let Some(session) = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()) {
                    connection.metadata.lock().unwrap().session = Some(session.into());
                }
                if !response.status().is_success() {
                    return Err(failure(format!("T3 Code MCP endpoint responded with HTTP {}.", response.status().as_u16())));
                }
                let mut payloads = Vec::new();
                crate::acp_mcp_bridge::response_payloads(response, |value| {
                    payloads.push(value);
                    std::future::ready(Ok(()))
                }).await.map_err(failure)?;
                // Unlike stdio, source ACP publishes versions only after the entire response is decoded.
                for payload in &payloads {
                    if let Some(version) = payload.get("result").filter(|value| value.is_object())
                        .and_then(|value| value.get("protocolVersion")).and_then(Value::as_str).filter(|value| !value.is_empty()) {
                        connection.metadata.lock().unwrap().version = Some(version.into());
                    }
                }
                Ok(payloads)
            } => result,
        }
    }
    async fn message(&self, request: &Value) -> Result<Value, AcpError> {
        let connection = self.connection(request["connectionId"].as_str().unwrap())?;
        let id = connection.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut envelope = json!({"jsonrpc":"2.0", "id":id, "method":request["method"]});
        if let Some(params) = request.get("params").filter(|value| !value.is_null()) {
            envelope["params"] = params.clone();
        }
        let payloads = self.send(&connection, envelope).await?;
        let response = payloads
            .iter()
            .find(|payload| {
                payload.is_object() && payload.get("id").and_then(Value::as_f64) == Some(id as f64)
            })
            .ok_or_else(|| failure("MCP server did not return a matching response."))?;
        if let Some(error) = response.get("error") {
            return Err(failure(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("The MCP server returned an error."),
            ));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
    async fn notification(&self, request: &Value) -> Result<(), AcpError> {
        let connection = self.connection(request["connectionId"].as_str().unwrap())?;
        let mut envelope = json!({"jsonrpc":"2.0", "method":request["method"]});
        if let Some(params) = request.get("params").filter(|value| !value.is_null()) {
            envelope["params"] = params.clone();
        }
        self.send(&connection, envelope).await.map(|_| ())
    }
    async fn disconnect_connection(&self, connection: &Connection) -> Result<(), AcpError> {
        let mut request = self
            .0
            .http
            .delete(&self.0.endpoint)
            .header("authorization", &self.0.authorization);
        {
            let metadata = connection.metadata.lock().unwrap();
            let Some(session) = &metadata.session else {
                return Ok(());
            };
            request = request.header("mcp-session-id", session);
            if let Some(version) = &metadata.version {
                request = request.header("mcp-protocol-version", version);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|error| failure(error.without_url().to_string()))?;
        if !response.status().is_success() && response.status().as_u16() != 404 {
            return Err(failure(format!(
                "T3 Code MCP endpoint rejected disconnect with HTTP {}.",
                response.status().as_u16()
            )));
        }
        Ok(())
    }
    async fn disconnect(&self, id: &str) -> Result<(), AcpError> {
        let connection = self
            .0
            .connections
            .lock()
            .unwrap()
            .shift_remove(id)
            .ok_or_else(|| failure(format!("Unknown MCP-over-ACP connection \"{id}\".")))?;
        self.disconnect_connection(&connection).await
    }
    /// Runtime scope finalization cancels owned HTTP requests before deleting
    /// their sessions. Awaiting permits proves the canceled request futures
    /// have dropped before provider credentials may be revoked.
    pub(crate) async fn dispose(&self) {
        // Scope cleanup can itself be canceled. Keep the admitted drain owned
        // until HTTP requests and session deletion finish; another disposer
        // awaits this same serialization barrier before returning.
        let bridge = self.clone();
        let _ = tokio::spawn(async move { bridge.dispose_owned().await }).await;
    }
    async fn dispose_owned(&self) {
        let _disposal = self.0.disposal.lock().await;
        self.0.stopped.send_replace(true);
        let connections = std::mem::take(&mut *self.0.connections.lock().unwrap());
        for (_, connection) in connections {
            let _sending = connection.sending.lock().await;
            let _ = self.disconnect_connection(&connection).await;
        }
    }
}
