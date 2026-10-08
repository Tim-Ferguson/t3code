//! Authenticated MCP HTTP profile used by the original patched runtime.
use crate::{
    mcp_device::McpDeviceTools,
    mcp_invocation::{InvocationScope, McpFailure},
    mcp_sessions::McpSessionRegistry,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use t3_contracts::{DeviceToolCloseInput, DeviceToolOpenInput, DeviceToolTargetInput};
const VERSION: &str = "2025-06-18";
#[derive(Clone)]
pub struct McpHttpService {
    registry: McpSessionRegistry,
    devices: Option<McpDeviceTools>,
    state: Arc<Mutex<HttpState>>,
    stopped: tokio::sync::watch::Sender<bool>,
    idle: Arc<tokio::sync::Notify>,
}
#[derive(Default)]
struct HttpState {
    sessions: HashSet<String>,
    closed: bool,
    active: usize,
}
struct Admission {
    state: Arc<Mutex<HttpState>>,
    idle: Arc<tokio::sync::Notify>,
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.state.lock().unwrap().active -= 1;
        self.idle.notify_waiters();
    }
}
fn empty(status: u16) -> Response {
    StatusCode::from_u16(status).unwrap().into_response()
}
fn json_response(status: u16, value: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        format!("{}\n", value),
    )
        .into_response()
}
fn error(status: u16, id: Value, code: i64, tag: &str, message: impl Into<String>) -> Response {
    json_response(
        status,
        json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into(),"_tag":tag}}),
    )
}
fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key).and_then(|value| value.to_str().ok())
}
fn media_types(headers: &HeaderMap, key: &str) -> Vec<String> {
    header(headers, key)
        .unwrap_or("")
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.split(';');
            let media = t3_contracts::trim_wire_string(pieces.next().unwrap()).to_ascii_lowercase();
            if let Some(quality) = pieces
                .map(|part| t3_contracts::trim_wire_string(part).to_ascii_lowercase())
                .find(|part| part.starts_with("q="))
            {
                let quality = &quality[2..];
                let value = if quality.is_empty() {
                    0.0
                } else {
                    quality.parse::<f64>().unwrap_or(f64::NAN)
                };
                if !value.is_finite() || value <= 0.0 || value > 1.0 {
                    return None;
                }
            }
            Some(media)
        })
        .collect()
}
impl McpHttpService {
    pub fn new(registry: McpSessionRegistry, devices: Option<McpDeviceTools>) -> Self {
        let (stopped, _) = tokio::sync::watch::channel(false);
        Self {
            registry,
            devices,
            state: Default::default(),
            stopped,
            idle: Default::default(),
        }
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route("/mcp", any(handle))
            .with_state(self.clone())
    }
    pub async fn shutdown(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.sessions.clear();
        }
        self.stopped.send_replace(true);
        self.registry.revoke_all();
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().unwrap().active == 0 {
                return;
            }
            notified.await;
        }
    }
    async fn authorized(&self, method: Method, headers: HeaderMap, body: Body) -> Response {
        let _admission = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return empty(503);
            }
            state.active += 1;
            Admission {
                state: self.state.clone(),
                idle: self.idle.clone(),
            }
        };
        let mut stopped = self.stopped.subscribe();
        let token = header(&headers, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(t3_contracts::trim_wire_string)
            .unwrap_or("");
        let Some(scope) = self.registry.resolve(token) else {
            let mut response = json_response(
                401,
                json!({"error":"invalid_mcp_credential","message":"A valid T3 Code MCP credential is required."}),
            );
            response
                .headers_mut()
                .insert("cache-control", "no-store".parse().unwrap());
            response.headers_mut().insert(
                "www-authenticate",
                if token.is_empty() {
                    "Bearer"
                } else {
                    "Bearer error=\"invalid_token\""
                }
                .parse()
                .unwrap(),
            );
            return response;
        };
        tokio::select! {biased;_=stopped.wait_for(|value|*value)=>empty(503),response=self.process(method, headers, body, &scope)=>response}
    }
    async fn process(
        &self,
        method: Method,
        headers: HeaderMap,
        body: Body,
        scope: &InvocationScope,
    ) -> Response {
        // The original layer passes no allowedOrigins; exact-origin browser requests are denied.
        if headers.contains_key("origin") {
            return empty(403);
        }
        let session = header(&headers, "mcp-session-id");
        if method == Method::DELETE {
            return empty(match session {
                None => 400,
                Some(session) => {
                    if self.state.lock().unwrap().sessions.remove(session) {
                        204
                    } else {
                        404
                    }
                }
            });
        }
        if method != Method::POST {
            let mut response = empty(405);
            response
                .headers_mut()
                .insert("allow", "POST".parse().unwrap());
            return response;
        }
        if media_types(&headers, "content-type")
            .first()
            .map(String::as_str)
            != Some("application/json")
        {
            return empty(415);
        }
        let accepted = media_types(&headers, "accept");
        if !["application/json", "text/event-stream"]
            .iter()
            .all(|value| accepted.iter().any(|item| item == value))
        {
            return empty(406);
        }
        let bytes = match to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(_) => return empty(400),
        };
        let parsed = serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes));
        let input = parsed.as_ref().ok();
        let id = input
            .and_then(|input| input.get("id"))
            .filter(|id| id.is_number() || id.is_string())
            .cloned()
            .unwrap_or(Value::Null);
        let initialize = input.is_some_and(|input| {
            input["jsonrpc"] == "2.0" && input["method"] == "initialize" && !id.is_null()
        });
        if session.is_some_and(|session| !self.state.lock().unwrap().sessions.contains(session)) {
            return empty(404);
        }
        if !initialize
            && header(&headers, "mcp-protocol-version").is_some_and(|version| version != VERSION)
        {
            return empty(400);
        }
        if !initialize
            && session.is_some()
            && header(&headers, "mcp-protocol-version") != Some(VERSION)
        {
            return empty(400);
        }
        let input = match parsed {
            Ok(input) => input,
            Err(_) => return error(200, Value::Null, -32700, "ParseError", "Parse error"),
        };
        if let Some(batch) = input.as_array() {
            return if batch.is_empty() {
                error(
                    400,
                    Value::Null,
                    -32600,
                    "InvalidRequest",
                    "Invalid Request",
                )
            } else {
                empty(400)
            };
        }
        let has_id = input.get("id").is_some();
        let valid_id = !has_id
            || input
                .get("id")
                .is_some_and(|id| id.is_number() || id.is_string());
        let is_request = input["jsonrpc"] == "2.0" && valid_id && input["method"].is_string();
        let is_response = input["jsonrpc"] == "2.0"
            && has_id
            && input
                .get("id")
                .is_some_and(|id| id.is_number() || id.is_string() || id.is_null())
            && (input.get("result").is_some() != input.get("error").is_some());
        if !is_request && !is_response {
            return error(200, id, -32600, "InvalidRequest", "Invalid Request");
        }
        if initialize {
            if session.is_some() {
                return empty(400);
            }
        } else if session.is_none() {
            return empty(400);
        }
        let mut response = if !is_request || !has_id {
            empty(202)
        } else {
            let method = input["method"].as_str().unwrap();
            let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
            match method {
                "initialize" => {
                    if !params["protocolVersion"].is_string()
                        || !params["capabilities"].is_object()
                        || !params["clientInfo"]["name"].is_string()
                        || !params["clientInfo"]["version"].is_string()
                    {
                        return error(
                            200,
                            id,
                            -32602,
                            "InvalidParams",
                            "Invalid initialize parameters",
                        );
                    }
                    let session = uuid::Uuid::new_v4().to_string();
                    {
                        let mut state = self.state.lock().unwrap();
                        if state.closed {
                            return empty(503);
                        }
                        state.sessions.insert(session.clone());
                    }
                    let mut capabilities = json!({"logging":{},"completions":{}});
                    if self.devices.is_some() {
                        capabilities["tools"] = json!({"listChanged":true});
                    }
                    let mut response = json_response(
                        200,
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":VERSION,"capabilities":capabilities,"serverInfo":{"name":"T3 Code","version":"0.0.45"}}}),
                    );
                    response
                        .headers_mut()
                        .insert("mcp-session-id", session.parse().unwrap());
                    response
                }
                "ping" => json_response(200, json!({"jsonrpc":"2.0","id":id,"result":{}})),
                "tools/list" => {
                    let tools: Value = if self.devices.is_some() {
                        serde_json::from_str(include_str!("mcp_device_catalog.json")).unwrap()
                    } else {
                        json!([])
                    };
                    json_response(
                        200,
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":tools}}),
                    )
                }
                "tools/call" => {
                    let Some(name) = params["name"].as_str() else {
                        return error(200, id, -32602, "InvalidParams", "Tool name is required");
                    };
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let Some(tools) = self.devices.as_ref().filter(|_| {
                        [
                            "device_list",
                            "device_open",
                            "device_close",
                            "device_screenshot",
                        ]
                        .contains(&name)
                    }) else {
                        return error(
                            200,
                            id,
                            -32602,
                            "InvalidParams",
                            format!("Tool '{name}' not found"),
                        );
                    };
                    match call_device(tools, scope, name, arguments).await {
                        Ok(result) => {
                            json_response(200, json!({"jsonrpc":"2.0","id":id,"result":result}))
                        }
                        Err(message) => error(200, id, -32602, "InvalidParams", message),
                    }
                }
                _ => error(
                    200,
                    id,
                    -32601,
                    "MethodNotFound",
                    format!("Method not found: {method}"),
                ),
            }
        };
        response
            .headers_mut()
            .insert("mcp-protocol-version", VERSION.parse().unwrap());
        response
    }
}
async fn handle(
    State(service): State<McpHttpService>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    service.authorized(parts.method, parts.headers, body).await
}
fn success(value: Value) -> Value {
    json!({"isError":false,"structuredContent":value,"content":[{"type":"text","text":value.to_string()}]})
}
fn failure(error: McpFailure) -> Value {
    let text = crate::mcp_device::failure_text(&error);
    json!({"isError":true,"content":[{"type":"text","text":text}]})
}
// MCP toolkit handlers decode the raw schema, rather than its JSON codec.
// In particular, optional fields reject explicit null before trusted access checks.
fn device_parameters(name: &str, mut input: Value) -> Result<Value, String> {
    if name != "device_screenshot" && input.is_null() {
        input = json!({});
    }
    let Some(fields) = input.as_object() else {
        return Err("Expected object".into());
    };
    let keys: &[&str] = match name {
        "device_list" => &["hostId"],
        "device_open" => &["deviceId", "platform", "hostId"],
        "device_close" => &["deviceId", "hostId", "shutdown"],
        _ => &["deviceId", "hostId"],
    };
    let mut result = serde_json::Map::new();
    let mut errors = Vec::new();
    for &key in keys {
        let Some(value) = fields.get(key) else {
            continue;
        };
        let issue = match key {
            "platform" => {
                if value == "ios" || value == "android" {
                    None
                } else if value.is_string() {
                    Some("Expected \"ios\" | \"android\"".to_owned())
                } else {
                    Some("Expected \"ios\" | \"android\" | undefined".to_owned())
                }
            }
            "shutdown" => {
                if value.is_boolean() {
                    None
                } else {
                    Some("Expected boolean | undefined".to_owned())
                }
            }
            _ => match value.as_str() {
                None => Some("Expected string | undefined".to_owned()),
                Some(_) if name == "device_list" => None,
                Some(text) => {
                    let text = t3_contracts::trim_wire_string(text);
                    let limit = if key == "deviceId" { 256 } else { 128 };
                    if text.is_empty() {
                        Some("Expected a non-blank string".to_owned())
                    } else if text.encode_utf16().count() > limit {
                        Some(format!("Expected a value with a length of at most {limit}"))
                    } else {
                        result.insert(key.into(), json!(text));
                        None
                    }
                }
            },
        };
        if let Some(issue) = issue {
            errors.push(format!("{issue}\n  at [\"{key}\"]"));
        } else {
            result
                .entry(key.to_owned())
                .or_insert_with(|| value.clone());
        }
    }
    if errors.is_empty() {
        Ok(Value::Object(result))
    } else {
        Err(errors.join("\n"))
    }
}
fn screenshot_failure(tag: &str) -> Value {
    json!({"isError":true,"structuredContent":{"error":{"_tag":tag,"operation":"screenshot","failureCount":1}},"content":[{"type":"text","text":"Device screenshot failed."}]})
}
async fn call_device(
    tools: &McpDeviceTools,
    scope: &InvocationScope,
    name: &str,
    input: Value,
) -> Result<Value, String> {
    let include = input["includeImage"] != false;
    let input = match device_parameters(name, input) {
        Ok(input) => input,
        Err(_) if name == "device_screenshot" => return Ok(screenshot_failure("AiError")),
        Err(message) => return Err(format!("Invalid parameters for tool '{name}': {message}")),
    };
    // The raw-schema validation above makes the typed JSON codec infallible here.
    let decode_error = |error: serde_json::Error| error.to_string();
    let result = match name {
        "device_list" => {
            let host = input.get("hostId").and_then(Value::as_str);
            tools
                .list(scope, host)
                .await
                .map(|result| success(serde_json::to_value(result).unwrap()))
        }
        "device_open" => {
            let input: DeviceToolOpenInput = serde_json::from_value(input).map_err(decode_error)?;
            tools
                .open(scope, input)
                .await
                .map(|result| success(serde_json::to_value(result).unwrap()))
        }
        "device_close" => {
            let input: DeviceToolCloseInput =
                serde_json::from_value(input).map_err(decode_error)?;
            tools.close(scope, input).await.map(success)
        }
        "device_screenshot" => {
            let input: DeviceToolTargetInput =
                serde_json::from_value(input).map_err(decode_error)?;
            return Ok(match tools.screenshot(scope, input).await {
                Ok(result) => {
                    let data = result.screenshot.data.clone();
                    let mut metadata = serde_json::to_value(result).unwrap();
                    metadata["screenshot"]
                        .as_object_mut()
                        .unwrap()
                        .remove("data");
                    let mut content = vec![json!({"type":"text","text":metadata.to_string()})];
                    if include {
                        content.push(json!({"type":"image","data":data,"mimeType":"image/png"}));
                    }
                    json!({"isError":false,"structuredContent":metadata,"content":content})
                }
                Err(error) => {
                    let tag = error.0["_tag"].as_str().unwrap_or("device_screenshotError");
                    screenshot_failure(tag)
                }
            });
        }
        _ => unreachable!(),
    };
    Ok(result.unwrap_or_else(failure))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn original_registered_device_tool_parameter_and_failure_oracle() {
        let root = tempfile::tempdir().unwrap();
        let fixture = crate::device_service::tests::fixture(root.path()).await;
        let (store, scope) = crate::mcp_invocation::tests::fixture();
        let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
        projection["runs"] = json!([{"id":"run:1","status":"running"}]);
        store
            .transaction(|tx| {
                crate::persistence::write_projection(tx, "thread", "thread:1", &projection)
            })
            .unwrap();
        assert!(scope.acts_as_caller(&store).is_ok());
        let tools = McpDeviceTools {
            devices: fixture.service.clone(),
            store,
            state_dir: root.path().join("launcher"),
            executable: std::env::current_exe().unwrap(),
        };
        let mut count = 0;
        let mut mismatches = Vec::new();
        for line in include_str!("../tests/fixtures/mcp-device-calls.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let mut scope = scope.clone();
            if row["device"] == true {
                scope
                    .capabilities
                    .insert(crate::mcp_invocation::McpCapability::Device);
            } else {
                scope
                    .capabilities
                    .shift_remove(&crate::mcp_invocation::McpCapability::Device);
            }
            let actual = match call_device(
                &tools,
                &scope,
                row["name"].as_str().unwrap(),
                row["input"].clone(),
            )
            .await
            {
                Ok(result) => json!({"result":result}),
                Err(message) => {
                    json!({"error":{"code":-32602,"message":message,"_tag":"InvalidParams"}})
                }
            };
            if actual != row["result"] {
                mismatches.push(format!("case {count}: {row} actual {actual}"));
            }
            count += 1;
        }
        fixture.service.shutdown().await;
        fixture.settings.shutdown().await;
        assert_eq!(count, 136);
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }
    #[tokio::test]
    async fn actual_patched_original_http_profile_oracle() {
        let registry = McpSessionRegistry::new(
            "mcp-oracle".parse().unwrap(),
            None,
            Arc::new(|| 1),
            24 * 60 * 60 * 1000,
        );
        let service = McpHttpService::new(registry, None);
        let (_, scope) = crate::mcp_invocation::tests::fixture();
        let mut session = String::new();
        let mut count = 0;
        for line in include_str!("../tests/fixtures/mcp-http.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let mut headers = HeaderMap::new();
            for (key, value) in row["headers"].as_object().unwrap() {
                let value = value.as_str().unwrap();
                headers.insert(
                    key.parse::<axum::http::HeaderName>().unwrap(),
                    if value == "$session" {
                        session.as_str()
                    } else {
                        value
                    }
                    .parse()
                    .unwrap(),
                );
            }
            let body = match row.get("input") {
                None => Body::empty(),
                Some(Value::String(input)) => Body::from(input.clone()),
                Some(value) => Body::from(value.to_string()),
            };
            let response = service
                .process(
                    row["method"].as_str().unwrap().parse().unwrap(),
                    headers,
                    body,
                    &scope,
                )
                .await;
            let status = response.status().as_u16();
            let mut returned = serde_json::Map::new();
            for key in ["content-type", "allow", "mcp-protocol-version"] {
                if let Some(value) = header(response.headers(), key) {
                    returned.insert(key.into(), json!(value));
                }
            }
            let returned_session = header(response.headers(), "mcp-session-id").map(str::to_owned);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            };
            if let Some(value) = returned_session.as_ref() {
                session = value.clone();
            }
            let actual = json!({"status":status,"headers":returned,"body":body,"session":returned_session.map(|_|"$session")});
            assert_eq!(actual, row["result"], "{}", row["name"]);
            count += 1;
        }
        assert_eq!(count, 36);
    }
    #[tokio::test]
    async fn shutdown_cancels_admitted_body_and_prevents_late_initialize_publication() {
        use crate::mcp_sessions::CredentialRequest;
        let registry =
            McpSessionRegistry::new("mcp-stop".parse().unwrap(), None, Arc::new(|| 1), 1000);
        let credential = registry
            .issue(CredentialRequest {
                thread_id: "thread:1".parse().unwrap(),
                provider_instance_id: "codex".parse().unwrap(),
                browser_tools_available: Some(false),
                capabilities: None,
            })
            .unwrap();
        let service = McpHttpService::new(registry.clone(), None);
        let (polled, ready) = tokio::sync::oneshot::channel();
        let mut polled = Some(polled);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct DropMarker(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropMarker {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let marker = DropMarker(dropped.clone());
        let stream = futures_util::stream::poll_fn(move |_cx| {
            let _ = &marker;
            if let Some(polled) = polled.take() {
                let _ = polled.send(());
            }
            std::task::Poll::<Option<Result<axum::body::Bytes, std::io::Error>>>::Pending
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            credential.authorization_header.parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert(
            "accept",
            "application/json,text/event-stream".parse().unwrap(),
        );
        let mut tasks = tokio::task::JoinSet::new();
        let request = service.clone();
        tasks.spawn(async move {
            request
                .authorized(Method::POST, headers, Body::from_stream(stream))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), ready)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), service.shutdown())
            .await
            .unwrap();
        assert_eq!(
            tasks.join_next().await.unwrap().unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(service.state.lock().unwrap().sessions.is_empty());
        assert_eq!(service.state.lock().unwrap().active, 0);
        assert!(
            registry
                .resolve(
                    credential
                        .authorization_header
                        .strip_prefix("Bearer ")
                        .unwrap()
                )
                .is_none()
        );
        service.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn authenticated_http_uses_fresh_grants_scoped_sessions_live_parent_and_image_results() {
        use crate::{mcp_sessions::CredentialRequest, persistence::write_projection};
        use t3_contracts::{DeviceToolListResult, DeviceToolOpenResult};
        tokio::time::timeout(std::time::Duration::from_secs(30),async {
            let root=tempfile::tempdir().unwrap();let fixture=crate::device_service::tests::fixture(root.path()).await;
            let(store,scope)=crate::mcp_invocation::tests::fixture();let thread=scope.thread.unwrap().thread_id;
            let mutate=|apply: &dyn Fn(&mut Value)|{let mut value=store.projection("thread",thread.as_str()).unwrap().unwrap();apply(&mut value);store.transaction(|tx|write_projection(tx,"thread",thread.as_str(),&value)).unwrap();};
            mutate(&|value|value["runs"]=json!([{"id":"run:1","status":"running"}]));
            let clock=Arc::new(std::sync::atomic::AtomicI64::new(1));let time=clock.clone();
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
            let registry=McpSessionRegistry::new("mcp-http-fixture".parse().unwrap(),Some(address),Arc::new(move||time.load(std::sync::atomic::Ordering::SeqCst)),1000);
            let issue=|id:&str,instance:&str,device:bool|registry.issue(CredentialRequest{thread_id:id.parse().unwrap(),provider_instance_id:instance.parse().unwrap(),browser_tools_available:Some(false),capabilities:Some(if device{[crate::mcp_invocation::McpCapability::Device].into_iter().collect()}else{Default::default()})}).unwrap();
            let first=issue(thread.as_str(),"codex",true);let sibling=issue("thread-other","codex",true);let denied=issue(thread.as_str(),"codex",false);let switched=issue(thread.as_str(),"different-provider",true);
            let tools=McpDeviceTools{devices:fixture.service.clone(),store:store.clone(),state_dir:root.path().join("launcher"),executable:std::env::current_exe().unwrap()};
            let service=McpHttpService::new(registry.clone(),Some(tools));
            let (stop,stopped)=tokio::sync::oneshot::channel();let mut server=tokio::task::JoinSet::new();let router=service.router();server.spawn(async move{axum::serve(listener,router).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
            let client=reqwest::Client::new();let url=format!("http://{address}/mcp");
            let request=|authorization:&str,session:Option<&str>,body:Value|{let mut request=client.post(&url).header("authorization",authorization).header("accept","application/json, text/event-stream").json(&body);if let Some(session)=session{request=request.header("mcp-session-id",session).header("mcp-protocol-version",VERSION);}request};
            for authorization in ["", "bearer invalid", "Bearer invalid", "Bearer client.invalid"] {
                let response=request(authorization,None,json!({})).send().await.unwrap();assert_eq!(response.status(),401);assert_eq!(response.headers()["cache-control"],"no-store");assert!(!response.headers()["www-authenticate"].to_str().unwrap().contains("resource_metadata"));let body:Value=response.json().await.unwrap();assert_eq!(body["error"],"invalid_mcp_credential");assert!(!body.to_string().contains("client.invalid"));
            }
            let initialize=json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"native-fixture","version":"1"}}});
            let response=request(&first.authorization_header,None,initialize).send().await.unwrap();assert_eq!(response.status(),200);let session=response.headers()["mcp-session-id"].to_str().unwrap().to_owned();let result:Value=response.json().await.unwrap();assert_eq!(result["result"]["capabilities"]["tools"]["listChanged"],true);
            let call=|token:&str,name:&str,args:Value|request(token,Some(&session),json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}}));
            let response:Value=request(&first.authorization_header,Some(&session),json!({"jsonrpc":"2.0","id":0,"method":"tools/list","params":{}})).send().await.unwrap().json().await.unwrap();assert_eq!(response["result"]["tools"],serde_json::from_str::<Value>(include_str!("mcp_device_catalog.json")).unwrap());
            let missing:Value=call(&first.authorization_header,"unknown_tool",json!([])).send().await.unwrap().json().await.unwrap();assert_eq!(missing["error"],json!({"code":-32602,"message":"Tool 'unknown_tool' not found","_tag":"InvalidParams"}));
            let result:Value=call(&denied.authorization_header,"device_list",json!({})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["isError"],true);assert_eq!(result["result"]["content"][0]["text"],"Agent device access is turned off for this environment.");assert!(fixture.service.current_readiness(None).is_none());
            fixture.service.configure(serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true})).unwrap()).await.unwrap();
            let result:Value=call(&first.authorization_header,"device_open",json!({"deviceId":"fixture-ios","threadId":"spoofed-thread","providerInstanceId":"spoofed-provider"})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["isError"],false,"{result}");let opened:DeviceToolOpenResult=serde_json::from_value(result["result"]["structuredContent"].clone()).unwrap();assert_eq!(opened.device.id.as_str(),"fixture-ios");assert_eq!(fixture.service.sessions_for_thread(&thread).len(),1);assert!(fixture.service.sessions_for_thread(&"spoofed-thread".parse().unwrap()).is_empty());
            // HTTP session identity does not cache the previous bearer scope.
            let result:Value=call(&sibling.authorization_header,"device_list",json!({})).send().await.unwrap().json().await.unwrap();let listed:DeviceToolListResult=serde_json::from_value(result["result"]["structuredContent"].clone()).unwrap();assert!(listed.open.is_empty());
            let result:Value=call(&switched.authorization_header,"device_close",json!({})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["content"][0]["text"],"The calling provider no longer owns an active thread run.");
            mutate(&|value|value["thread"]["archivedAt"]=json!("2026-01-01T00:00:00Z"));
            assert!(!crate::thread::shell(&store.projection("thread",thread.as_str()).unwrap().unwrap())["archivedAt"].is_null());
            let result:Value=call(&first.authorization_header,"device_close",json!({})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["isError"],true);
            let result:Value=call(&first.authorization_header,"device_screenshot",json!({"deviceId":"fixture-ios"})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["content"].as_array().unwrap().len(),2);assert_eq!(result["result"]["content"][1]["type"],"image");assert!(result["result"]["structuredContent"]["screenshot"].get("data").is_none());
            mutate(&|value|{value["thread"]["archivedAt"]=Value::Null;value["thread"]["deletedAt"]=json!("2026-01-01T00:00:00Z");});
            assert!(!crate::thread::shell(&store.projection("thread",thread.as_str()).unwrap().unwrap())["deletedAt"].is_null());
            let result:Value=call(&first.authorization_header,"device_close",json!({})).send().await.unwrap().json().await.unwrap();assert_eq!(result["result"]["content"][0]["text"],"The calling thread was not found.");
            let result:Value=call(&first.authorization_header,"device_open",json!([])).send().await.unwrap().json().await.unwrap();assert_eq!(result["error"]["code"],-32602);
            registry.revoke_provider_session(&first.provider_session_id);
            assert_eq!(call(&first.authorization_header,"device_list",json!({})).send().await.unwrap().status(),401);
            assert_eq!(call(&sibling.authorization_header,"device_list",json!({})).send().await.unwrap().status(),200);
            clock.store(1002,std::sync::atomic::Ordering::SeqCst);assert_eq!(call(&sibling.authorization_header,"device_list",json!({})).send().await.unwrap().status(),401);
            service.shutdown().await;fixture.service.shutdown().await;fixture.settings.shutdown().await;
            let _=stop.send(());while let Some(result)=server.join_next().await{result.unwrap();}
        }).await.expect("authenticated MCP fixture timed out");
    }
}
