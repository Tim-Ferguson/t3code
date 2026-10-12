//! The two HTML tools an agent calls, `html_preview` and `html_render` (apps/server/src/mcp/toolkits/html and the registration of the preview in
//! McpHttpServer.ts): the first renders a page in the headless browser and answers with a screenshot, the height the page needs and what it
//! logged; the second publishes a finished page into the calling thread above its reply.
use crate::{
    html_render::{self, HtmlRender, HtmlRenderError},
    html_render_shared::Appearance,
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpFailure, refusal},
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// The tools this module answers, which the dispatch of `mcp_extra.rs` and its list of handled tools name too.
#[cfg(test)]
pub(crate) const HTML_TOOLS: &[&str] = &["html_preview", "html_render"];

/// Every HTML render error message is built on the server and tells the agent what to do next.
fn failure_of(error: &HtmlRenderError) -> McpFailure {
    refusal(if error.invalid_page() { "invalid_request" } else { "orchestration_error" }, &error.message())
}

/// The result of an image tool that failed: the message as text, and as the structured error clients read.
fn image_tool_failure(operation: &str, failure: &McpFailure) -> Value {
    let message = failure.to_string();
    let tag = failure.0["_tag"].as_str().unwrap_or("html_previewError");
    json!({
        "isError": true,
        "structuredContent": {"error": {"_tag": tag, "operation": operation, "failureCount": 1, "message": message}},
        "content": [{"type": "text", "text": message}],
    })
}

impl McpControlTools {
    fn html_service(&self) -> Result<Arc<HtmlRender>, McpFailure> {
        html_render::for_store(&self.store).ok_or_else(mcp_access::unavailable)
    }

    /// `html_preview`: a page rendered in the headless browser. It answers with the whole MCP result, since the screenshot goes out as an
    /// image block and the rest as JSON text. The browser runs on the host and can open local files, so only an agent T3 launched, which
    /// already works on this machine, gets it.
    pub(crate) async fn html_preview(&self, scope: &InvocationScope, args: Value) -> Value {
        match self.html_preview_inner(scope, &args).await {
            Ok(result) => result,
            Err(failure) => image_tool_failure("preview", &failure),
        }
    }

    async fn html_preview_inner(&self, scope: &InvocationScope, args: &Value) -> Result<Value, McpFailure> {
        mcp_access::check_store(&self.store, scope, Access::ReadsAsCaller, &[])?;
        let service = self.html_service()?;
        let html = args.get("html").and_then(Value::as_str).unwrap_or_default();
        let appearance = match args.get("appearance").and_then(Value::as_str) {
            Some("light") => Some(Appearance::Light),
            Some("dark") => Some(Appearance::Dark),
            _ => None,
        };
        let preview = service.preview(html, args.get("width").and_then(Value::as_i64), appearance).await.map_err(|error| failure_of(&error))?;
        // The success schema's order: the screenshot's own data goes out as an image block, and its dimensions stay in the metadata.
        let mut metadata = Map::new();
        metadata.insert("width".into(), json!(preview.width));
        metadata.insert("contentHeight".into(), json!(preview.content_height));
        metadata.insert("capturedHeight".into(), json!(preview.captured_height));
        metadata.insert("consoleMessages".into(), Value::Array(preview.console_messages.iter().map(|message| json!({"level": message.level.as_str(), "text": message.text})).collect()));
        if let Some(missing) = &preview.missing_images {
            metadata.insert("missingImages".into(), json!(missing));
        }
        metadata.insert("screenshot".into(), json!({"mimeType": "image/png", "width": preview.width, "height": preview.captured_height}));
        let metadata = Value::Object(metadata);
        Ok(json!({
            "isError": false,
            "structuredContent": metadata,
            "content": [{"type": "text", "text": metadata.to_string()}, {"type": "image", "data": preview.png, "mimeType": "image/png"}],
        }))
    }

    /// `html_render`: publishes a finished page into the calling thread. The page is stored in that thread, so it needs the thread's live run.
    pub(crate) async fn html_render(&self, scope: &InvocationScope, args: Value) -> Result<Value, McpFailure> {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        let thread = scope.require_thread("html_render")?;
        let service = self.html_service()?;
        let html = args.get("html").and_then(Value::as_str).unwrap_or_default();
        let title = args.get("title").and_then(Value::as_str).unwrap_or_default();
        let height = args.get("height").and_then(Value::as_i64).unwrap_or_default();
        let reference = service.publish(thread.thread_id.as_str(), html, title, height).await.map_err(|error| failure_of(&error))?;
        Ok(json!({
            "htmlRender": reference,
            "message": "Shown to the reader above your reply. Don't mention or describe the page; reply with only what it doesn't already say.",
        }))
    }
}
