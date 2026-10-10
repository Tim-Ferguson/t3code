//! The OpenCode 1.x HTTP surface, as `@opencode-ai/sdk/v2`'s generated client calls it: session, permission, question,
//! MCP and event routes on the server's root (2.x lives under `/api`). Every request is scoped to the session's
//! directory (`x-opencode-directory`, or the `directory` query of a GET). Transport, authentication, bounds and
//! redirects are [`Api`]'s; this module only knows the routes and bodies. The fake server checks each one against the
//! SDK's own types (`tools/generate_opencode1_openapi.mjs`).
use crate::opencode_api::{Api, ApiError, EventStream};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Client(Api);

fn segment(value: &str) -> String {
    crate::opencode_projection::enc(value)
}

impl Client {
    /// The server, scoped to the directory its sessions run in.
    pub(crate) fn new(api: &Api, directory: &str) -> Self {
        Self(api.for_directory(directory))
    }
    pub(crate) async fn subscribe(&self) -> Result<EventStream, ApiError> {
        self.0.subscribe("/event").await
    }
    async fn call(&self, method: Method, path: String, body: Option<Value>, limit: Option<Duration>) -> Result<Value, ApiError> {
        self.0.call(method, &path, &[], body.as_ref(), limit).await
    }

    pub(crate) async fn session_create(&self, permission: &Value) -> Result<Value, ApiError> {
        self.call(Method::POST, "/session".into(), Some(json!({"permission":permission})), None).await
    }
    pub(crate) async fn session_get(&self, session: &str) -> Result<Value, ApiError> {
        self.call(Method::GET, format!("/session/{}", segment(session)), None, None).await
    }
    pub(crate) async fn session_update(&self, session: &str, permission: &Value) -> Result<Value, ApiError> {
        self.call(Method::PATCH, format!("/session/{}", segment(session)), Some(json!({"permission":permission})), None).await
    }
    pub(crate) async fn session_children(&self, session: &str, limit: Option<Duration>) -> Result<Value, ApiError> {
        self.call(Method::GET, format!("/session/{}/children", segment(session)), None, limit).await
    }
    pub(crate) async fn session_messages(&self, session: &str) -> Result<Value, ApiError> {
        self.call(Method::GET, format!("/session/{}/message", segment(session)), None, None).await
    }
    pub(crate) async fn session_status(&self) -> Result<Value, ApiError> {
        self.call(Method::GET, "/session/status".into(), None, None).await
    }
    pub(crate) async fn prompt_async(&self, session: &str, body: &Value) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/session/{}/prompt_async", segment(session)), Some(body.clone()), None).await
    }
    pub(crate) async fn command(&self, session: &str, body: &Value) -> Result<Value, ApiError> {
        // The command route answers when the generation ends, which can be long.
        self.call(Method::POST, format!("/session/{}/command", segment(session)), Some(body.clone()), Some(Duration::from_secs(24 * 60 * 60))).await
    }
    pub(crate) async fn summarize(&self, session: &str, body: &Value) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/session/{}/summarize", segment(session)), Some(body.clone()), Some(Duration::from_secs(24 * 60 * 60))).await
    }
    pub(crate) async fn abort(&self, session: &str, limit: Option<Duration>) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/session/{}/abort", segment(session)), None, limit).await
    }
    pub(crate) async fn fork(&self, session: &str, message: Option<&str>) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/session/{}/fork", segment(session)), message.map(|message| json!({"messageID":message})), None).await
    }
    pub(crate) async fn permission_reply(&self, request: &str, reply: &str) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/permission/{}/reply", segment(request)), Some(json!({"reply":reply})), Some(Duration::from_secs(10))).await
    }
    pub(crate) async fn question_reply(&self, request: &str, answers: &Value) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/question/{}/reply", segment(request)), Some(json!({"answers":answers})), Some(Duration::from_secs(10))).await
    }
    pub(crate) async fn mcp_status(&self, limit: Duration) -> Result<Value, ApiError> {
        self.call(Method::GET, "/mcp".into(), None, Some(limit)).await
    }
    pub(crate) async fn mcp_add(&self, name: &str, config: &Value) -> Result<Value, ApiError> {
        self.call(Method::POST, "/mcp".into(), Some(json!({"name":name,"config":config})), Some(Duration::from_secs(20))).await
    }
    pub(crate) async fn mcp_disconnect(&self, name: &str) -> Result<Value, ApiError> {
        self.call(Method::POST, format!("/mcp/{}/disconnect", segment(name)), None, Some(Duration::from_secs(10))).await
    }
    pub(crate) async fn command_list(&self, limit: Duration) -> Result<Value, ApiError> {
        self.call(Method::GET, "/command".into(), None, Some(limit)).await
    }
}

/// `isOpenCodeNotFound`: the session or request is gone.
pub(crate) fn is_not_found(error: &ApiError) -> bool {
    error.status() == Some(404) || error.tag().is_some_and(|tag| tag.eq_ignore_ascii_case("NotFoundError"))
}
