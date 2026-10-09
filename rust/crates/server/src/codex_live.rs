//! The Codex half of the live provider session (`provider_live.rs`): MCP Apps operations and
//! feedback upload, which go through Codex's own app-server (`CodexAdapterV2`'s `mcpApps` and
//! `uploadFeedback`). MCP Apps reach their server through Codex's MCP client, so any server the user
//! configured for Codex works without T3 connecting to it.
use crate::{
    provider_live::{FeedbackUpload, LiveSession, McpAppsHost},
    provider_process::ProviderProcess,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The native thread a stored provider thread names; the request has nowhere to go without one.
fn native_thread(provider_thread: &Value) -> Result<String, String> {
    provider_thread["nativeThreadRef"]["nativeId"].as_str().map(str::to_owned).ok_or_else(|| "The provider thread has no native Codex thread.".to_owned())
}

struct Host(ProviderProcess);

impl McpAppsHost for Host {
    fn list_tools(&self, provider_thread: Value, server: String) -> BoxFuture<'static, Result<Vec<Value>, String>> {
        let process = self.0.clone();
        Box::pin(async move {
            let thread = native_thread(&provider_thread)?;
            let response = process.request("mcpServerStatus/list", json!({"threadId":thread,"serverName":server,"detail":"toolsAndAuthOnly"}), REQUEST_TIMEOUT).await.map_err(|error| format!("Failed to list Codex MCP tools: {error}"))?;
            let tools = response["data"].as_array().into_iter().flatten().find(|entry| entry["name"] == server.as_str()).and_then(|entry| entry["tools"].as_object());
            Ok(tools.map(|tools| tools.values().cloned().collect()).unwrap_or_default())
        })
    }
    fn call_tool(&self, provider_thread: Value, server: String, tool: String, arguments: Value) -> BoxFuture<'static, Result<Value, String>> {
        let process = self.0.clone();
        Box::pin(async move {
            let thread = native_thread(&provider_thread)?;
            let response = process.request("mcpServer/tool/call", json!({"threadId":thread,"server":server,"tool":tool,"arguments":arguments}), REQUEST_TIMEOUT).await.map_err(|error| format!("Codex MCP tool call failed: {error}"))?;
            let mut result = json!({"content":response["content"]});
            if let Some(structured) = response.get("structuredContent").filter(|value| !value.is_null()) {
                result["structuredContent"] = structured.clone();
            }
            if response["isError"] == true {
                result["isError"] = json!(true);
            }
            if let Some(meta) = response.get("_meta").filter(|value| !value.is_null()) {
                result["_meta"] = meta.clone();
            }
            Ok(result)
        })
    }
    fn read_resource(&self, provider_thread: Value, server: String, uri: String) -> BoxFuture<'static, Result<Value, String>> {
        let process = self.0.clone();
        Box::pin(async move {
            let thread = native_thread(&provider_thread)?;
            let response = process.request("mcpServer/resource/read", json!({"threadId":thread,"server":server,"uri":uri}), REQUEST_TIMEOUT).await.map_err(|error| format!("Codex MCP resource read failed: {error}"))?;
            Ok(json!({"contents":response["contents"]}))
        })
    }
}

/// What a running Codex process offers outside a turn.
pub(crate) fn live_session(process: &ProviderProcess) -> LiveSession {
    let feedback_process = process.clone();
    let upload_feedback: FeedbackUpload = Arc::new(move |provider_thread, reason| {
        let process = feedback_process.clone();
        Box::pin(async move {
            let thread = native_thread(&provider_thread)?;
            let mut params = json!({"classification":"bug","includeLogs":true,"threadId":thread});
            if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
                params["reason"] = json!(reason);
            }
            let response = process.request("feedback/upload", params, REQUEST_TIMEOUT).await.map_err(|error| format!("Failed to upload Codex thread feedback: {error}"))?;
            response["threadId"].as_str().map(str::to_owned).ok_or_else(|| "Codex did not name the uploaded feedback.".to_owned())
        })
    });
    LiveSession { driver: "codex".into(), upload_feedback: Some(upload_feedback), mcp_apps: Some(Arc::new(Host(process.clone()))) }
}
