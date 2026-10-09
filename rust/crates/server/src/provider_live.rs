//! What a running provider session offers requests that arrive outside a turn: uploading product
//! feedback for a thread, and the MCP Apps operations that go through the provider's own MCP
//! client (`ProviderAdapterV2.uploadFeedback` and `.mcpApps`). A request finds the session by its
//! provider session id, as the original's session manager lookup does; a session that is not
//! registered counts as stopped, because an app view or a feedback form must never start one.
//!
//! An adapter registers its session while the provider process lives and drops the registration
//! when it ends. Only the adapter whose protocol exposes an operation fills it in; the rest leave
//! it `None`, which the callers report as the provider not supporting it.
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

/// `ProviderAdapterV2McpApps`: MCP Apps host operations through the provider's MCP client.
pub trait McpAppsHost: Send + Sync {
    /// The tools of one MCP server, as the server defines them (`name`, `annotations`, `_meta`, ...).
    fn list_tools(&self, provider_thread: Value, server: String) -> BoxFuture<'static, Result<Vec<Value>, String>>;
    /// An MCP `CallToolResult`.
    fn call_tool(&self, provider_thread: Value, server: String, tool: String, arguments: Value) -> BoxFuture<'static, Result<Value, String>>;
    /// An MCP `ReadResourceResult`.
    fn read_resource(&self, provider_thread: Value, server: String, uri: String) -> BoxFuture<'static, Result<Value, String>>;
}

/// Uploads feedback for a provider thread and returns the id the provider gave it.
pub type FeedbackUpload = Arc<dyn Fn(Value, Option<String>) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

#[derive(Clone)]
pub struct LiveSession {
    pub driver: String,
    pub upload_feedback: Option<FeedbackUpload>,
    pub mcp_apps: Option<Arc<dyn McpAppsHost>>,
}

fn sessions() -> &'static Mutex<HashMap<String, LiveSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, LiveSession>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

/// A session's presence in the registry; dropping it ends the registration.
pub struct Registration(String);

impl Drop for Registration {
    fn drop(&mut self) {
        sessions().lock().unwrap().remove(&self.0);
    }
}

/// Makes `session` findable by its provider session id for as long as the guard lives.
pub fn register(provider_session_id: &str, session: LiveSession) -> Registration {
    sessions().lock().unwrap().insert(provider_session_id.to_owned(), session);
    Registration(provider_session_id.to_owned())
}

pub fn get(provider_session_id: &str) -> Option<LiveSession> {
    sessions().lock().unwrap().get(provider_session_id).cloned()
}
