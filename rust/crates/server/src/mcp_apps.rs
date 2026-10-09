//! MCP Apps (port of `mcpApps/McpAppRequests.ts`, `McpAppModelContext.ts` and `McpAppSnapshot.ts`)
//! and `provider.uploadFeedback`, which finds its session the same way.
//!
//! An MCP App is an interactive view an MCP tool names with a `ui://` HTML resource. Requests an
//! app makes of its own server (`mcpApps.callTool`, `.toolInfo`, `.readResource`) name the tool
//! call that produced it; the app is resolved from that stored item, so it reaches only its own
//! server, and each request runs through the thread's live provider session, whose MCP client owns
//! the connection and its credentials. A stopped session is reported, never started. What the app
//! tells the agent (`.updateModelContext`) needs only the app, not a session.
use crate::{persistence::Store, provider_live};
use serde_json::{Map, Value, json};

pub const METHODS: [&str; 5] = ["mcpApps.callTool", "mcpApps.toolInfo", "mcpApps.readResource", "mcpApps.updateModelContext", "provider.uploadFeedback"];

/// Largest context one app may keep for the agent, in UTF-8 bytes.
pub const MODEL_CONTEXT_MAX_BYTES: usize = 16 * 1024;
const APP_RESOURCE_SCHEME: &str = "ui://";
const APP_MIME_TYPE: &str = "text/html;profile=mcp-app";
/// Largest app document stored.
pub const APP_MAX_HTML_BYTES: usize = 5 * 1024 * 1024;
const CONTEXT_KIND: &str = "mcp_app_context";

fn request_error(thread_id: &str, reason: &str, cause: Option<String>) -> Value {
    let mut wire = json!({"_tag":"McpAppRequestError","threadId":thread_id,"reason":reason});
    if let Some(cause) = cause {
        wire["cause"] = json!({"name":"Error","message":cause});
    }
    wire
}

fn decode_error(message: &str) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message})
}

fn required_text<'a>(payload: &'a Value, key: &str) -> Result<&'a str, Value> {
    payload[key].as_str().filter(|text| !t3_contracts::trim_wire_string(text).is_empty()).ok_or_else(|| decode_error(&format!("{key} must be a non-empty string.")))
}

// ---- reading the pieces of an app ------------------------------------------------------------

/// `mcpAppToolCallableByApp`: `_meta.ui.visibility` lets an app call the tool. The spec defaults
/// to visible to both the model and apps.
pub fn tool_callable_by_app(meta: &Value) -> bool {
    match meta.get("ui").and_then(|ui| ui.get("visibility")) {
        Some(Value::Array(visibility)) => visibility.iter().any(|entry| entry == "app"),
        _ => true,
    }
}

fn read_only_hint(tool: &Value) -> bool {
    tool["annotations"].as_object().is_some_and(|annotations| annotations.get("readOnlyHint") == Some(&json!(true)))
}
fn tool_title(tool: &Value) -> Option<&str> {
    tool["annotations"].as_object().and_then(|annotations| annotations.get("title")).and_then(Value::as_str)
}

struct App {
    server: String,
    tool: String,
    provider_thread_id: String,
}

/// The app a tool call produced, read from the stored item. The reference only counts when it
/// names the very server and tool the item records, so a server can only point at an app of its own.
fn resolve_app(store: &Store, thread_id: &str, item_id: &str) -> Result<App, Value> {
    let failed = |error: crate::persistence::StoreError| request_error(thread_id, "request-failed", Some(error.to_string()));
    let projection = store.projection("thread", thread_id).map_err(failed)?;
    let item = projection.as_ref().and_then(|projection| projection["turnItems"].as_array()).and_then(|items| items.iter().find(|item| item["id"] == item_id));
    let not_an_app = || request_error(thread_id, "not-an-app", None);
    let item = item.filter(|item| item["type"] == "dynamic_tool").ok_or_else(not_an_app)?;
    let wire = crate::wire_projection::item(item);
    let reference = &wire["output"]["t3McpApp"];
    let (server, tool) = (reference["server"].as_str(), reference["tool"].as_str());
    let named_by_item = matches!((server, tool, item["toolName"].as_str()), (Some(server), Some(tool), Some(name)) if name == format!("{server}.{tool}"));
    match (server, tool, item["providerThreadId"].as_str()) {
        (Some(server), Some(tool), Some(provider_thread)) if named_by_item => Ok(App { server: server.to_owned(), tool: tool.to_owned(), provider_thread_id: provider_thread.to_owned() }),
        _ => Err(not_an_app()),
    }
}

struct Resolved {
    app: App,
    provider_thread: Value,
    host: std::sync::Arc<dyn provider_live::McpAppsHost>,
}

/// The app, its provider thread, and the live session's MCP Apps operations.
fn resolve(store: &Store, thread_id: &str, item_id: &str) -> Result<Resolved, Value> {
    let app = resolve_app(store, thread_id, item_id)?;
    let projection = store.projection("thread", thread_id).map_err(|error| request_error(thread_id, "request-failed", Some(error.to_string())))?;
    let provider_thread = projection.as_ref().and_then(|projection| projection["providerThreads"].as_array()).and_then(|threads| threads.iter().find(|thread| thread["id"] == app.provider_thread_id.as_str())).cloned();
    let stopped = || request_error(thread_id, "session-stopped", None);
    let provider_thread = provider_thread.filter(|thread| thread["providerSessionId"].is_string()).ok_or_else(stopped)?;
    let session = provider_live::get(provider_thread["providerSessionId"].as_str().unwrap_or_default()).ok_or_else(stopped)?;
    let host = session.mcp_apps.ok_or_else(|| request_error(thread_id, "provider-unsupported", None))?;
    Ok(Resolved { app, provider_thread, host })
}

async fn find_tool(thread_id: &str, resolved: &Resolved, name: &str) -> Result<Option<Value>, Value> {
    let tools = resolved.host.list_tools(resolved.provider_thread.clone(), resolved.app.server.clone()).await.map_err(|cause| request_error(thread_id, "request-failed", Some(cause)))?;
    Ok(tools.into_iter().find(|tool| tool["name"] == name))
}

// ---- model context ---------------------------------------------------------------------------

/// What each app told the agent through `ui/update-model-context`: one entry per app (its tool
/// call), kept under the thread whose next turn carries it.
fn set_context(store: &Store, thread_id: &str, item_id: &str, server: &str, tool: &str, text: &str) -> Result<(), crate::persistence::StoreError> {
    store.transaction(|transaction| {
        let mut state = crate::persistence::read_projection(transaction, CONTEXT_KIND, thread_id)?.unwrap_or_else(|| json!({"entries":{}}));
        let entries = state["entries"].as_object_mut().expect("entries are an object");
        if text.trim().is_empty() {
            entries.remove(item_id);
        } else {
            entries.insert(item_id.to_owned(), json!({"itemId":item_id,"server":server,"tool":tool,"text":text,"updatedAt":crate::execution::at(chrono::Utc::now())}));
        }
        crate::persistence::write_projection(transaction, CONTEXT_KIND, thread_id, &state)
    })
}

/// The entries the thread's next turn carries to the provider (`McpAppModelContext.forThread`),
/// oldest update first. Only apps still in the thread's history count: an app whose item is gone,
/// or whose run this thread rolled back, stops informing the agent. The item can live in a thread
/// this one was forked from, so it is matched by id alone.
pub fn model_context_for_thread(store: &Store, thread_id: &str) -> Result<Vec<Value>, crate::persistence::StoreError> {
    let Some(state) = store.projection(CONTEXT_KIND, thread_id)? else { return Ok(Vec::new()) };
    let threads = store.projections("thread")?;
    let mut entries: Vec<Value> = state["entries"].as_object().map(|entries| entries.values().cloned().collect()).unwrap_or_default();
    entries.retain(|entry| {
        threads.iter().any(|thread| {
            let Some(item) = thread["turnItems"].as_array().and_then(|items| items.iter().find(|item| item["id"] == entry["itemId"])) else { return false };
            let run = thread["runs"].as_array().and_then(|runs| runs.iter().find(|run| run["id"] == item["runId"]));
            run.is_none_or(|run| run["status"] != "rolled_back" || thread["thread"]["id"] != thread_id)
        })
    });
    entries.sort_by(|left, right| (left["updatedAt"].as_str(), left["itemId"].as_str()).cmp(&(right["updatedAt"].as_str(), right["itemId"].as_str())));
    Ok(entries.into_iter().map(|mut entry| { entry.as_object_mut().map(|map| map.remove("updatedAt")); entry }).collect())
}

/// Whether `thread_id` is `ancestor_id` or forked from it, however many forks deep. A thread seen
/// twice means a corrupt cycle, which proves nothing.
fn descends_from(store: &Store, thread_id: &str, ancestor_id: &str, reply_thread: &str) -> Result<bool, Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut current = Some(thread_id.to_owned());
    while let Some(id) = current.take() {
        if id == ancestor_id {
            return Ok(true);
        }
        if seen.contains(&id) {
            break;
        }
        seen.push(id.clone());
        let projection = store.projection("thread", &id).map_err(|error| request_error(reply_thread, "request-failed", Some(error.to_string())))?
            .ok_or_else(|| request_error(reply_thread, "request-failed", Some("Thread not found.".into())))?;
        let lineage = &projection["thread"]["lineage"];
        current = (lineage["relationshipToParent"] == "fork").then(|| lineage["parentThreadId"].as_str().map(str::to_owned)).flatten();
    }
    Ok(false)
}

fn update_model_context(store: &Store, payload: &Value) -> Result<Value, Value> {
    let (thread_id, item_id, conversation) = (required_text(payload, "threadId")?, required_text(payload, "itemId")?, required_text(payload, "conversationThreadId")?);
    if payload.get("content").is_some_and(|content| !content.is_array()) || payload.get("structuredContent").is_some_and(|content| !content.is_object()) {
        return Err(decode_error("content must be an array and structuredContent an object."));
    }
    let app = resolve_app(store, thread_id, item_id)?;
    // Context belongs to the conversation on screen, which only shows this app when it is the
    // app's own thread or one forked from it.
    if !descends_from(store, conversation, thread_id, thread_id)? {
        return Err(request_error(thread_id, "not-an-app", None));
    }
    let mut texts: Vec<String> = Vec::new();
    for block in payload["content"].as_array().into_iter().flatten() {
        match (block.as_object(), block["type"].as_str(), block["text"].as_str()) {
            (Some(_), Some("text"), Some(text)) => texts.push(text.to_owned()),
            _ => return Err(request_error(thread_id, "unsupported-content", None)),
        }
    }
    if let Some(structured) = payload.get("structuredContent") {
        texts.push(serde_json::to_string(structured).expect("JSON serializes"));
    }
    // Kept as sent; blank text clears the app's context in the store.
    let text = texts.join("\n");
    if text.len() > MODEL_CONTEXT_MAX_BYTES {
        return Err(request_error(thread_id, "request-failed", Some("Model context is too large.".into())));
    }
    set_context(store, conversation, item_id, &app.server, &app.tool, &text).map_err(|error| request_error(thread_id, "request-failed", Some(error.to_string())))?;
    Ok(Value::Null)
}

// ---- the methods -----------------------------------------------------------------------------

async fn upload_feedback(store: &Store, payload: &Value) -> Result<Value, Value> {
    let thread_id = required_text(payload, "threadId")?;
    let reason = match payload.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) if !t3_contracts::trim_wire_string(reason).is_empty() => Some(t3_contracts::trim_wire_string(reason).to_owned()),
        Some(_) => return Err(decode_error("reason must be a non-empty string.")),
    };
    let failure = |cause: String| json!({"_tag":"ProviderUploadFeedbackError","threadId":thread_id,"cause":{"name":"Error","message":cause}});
    let projection = store.projection("thread", thread_id).map_err(|error| failure(error.to_string()))?.ok_or_else(|| failure("Thread not found.".into()))?;
    let threads = projection["providerThreads"].as_array().cloned().unwrap_or_default();
    let provider_thread = threads.iter().find(|thread| thread["id"] == projection["thread"]["activeProviderThreadId"]).or(threads.last()).cloned();
    let Some((provider_thread, session_id)) = provider_thread.as_ref().and_then(|thread| thread["providerSessionId"].as_str().map(|id| (thread.clone(), id.to_owned()))) else {
        return Err(failure("No provider session has run in this thread yet.".into()));
    };
    let Some(session) = provider_live::get(&session_id) else {
        return Err(failure("The provider session is no longer running. Send a message first.".into()));
    };
    let Some(upload) = session.upload_feedback else {
        return Err(failure(format!("Provider '{}' does not support feedback uploads.", session.driver)));
    };
    match upload(provider_thread, reason).await {
        Ok(feedback_id) => Ok(json!({"feedbackId":feedback_id})),
        Err(cause) => Err(failure(cause)),
    }
}

pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    if tag == "provider.uploadFeedback" {
        return upload_feedback(store, &payload).await;
    }
    if tag == "mcpApps.updateModelContext" {
        return update_model_context(store, &payload);
    }
    let (thread_id, item_id) = (required_text(&payload, "threadId")?, required_text(&payload, "itemId")?);
    match tag {
        "mcpApps.toolInfo" => {
            let name = required_text(&payload, "name")?;
            let resolved = resolve(store, thread_id, item_id)?;
            let tool = find_tool(thread_id, &resolved, name).await?;
            let mut info = Map::new();
            info.insert("callable".into(), json!(tool.as_ref().is_some_and(|tool| tool_callable_by_app(&tool["_meta"]))));
            info.insert("readOnly".into(), json!(tool.as_ref().is_some_and(read_only_hint)));
            if let Some(title) = tool.as_ref().and_then(tool_title) {
                info.insert("title".into(), json!(title));
            }
            if let Some(tool) = tool {
                info.insert("tool".into(), tool);
            }
            Ok(Value::Object(info))
        }
        "mcpApps.callTool" => {
            let name = required_text(&payload, "name")?;
            if !payload["arguments"].is_object() {
                return Err(decode_error("arguments must be an object."));
            }
            let resolved = resolve(store, thread_id, item_id)?;
            // The spec forbids apps calling tools hidden from them; checked here too, since a
            // client's own check is only a courtesy to its user.
            let tool = find_tool(thread_id, &resolved, name).await?;
            if !tool.as_ref().is_some_and(|tool| tool_callable_by_app(&tool["_meta"])) {
                return Err(request_error(thread_id, "tool-not-callable", None));
            }
            resolved.host.call_tool(resolved.provider_thread.clone(), resolved.app.server.clone(), name.to_owned(), payload["arguments"].clone()).await.map_err(|cause| request_error(thread_id, "request-failed", Some(cause)))
        }
        "mcpApps.readResource" => {
            let uri = required_text(&payload, "uri")?;
            let resolved = resolve(store, thread_id, item_id)?;
            resolved.host.read_resource(resolved.provider_thread.clone(), resolved.app.server.clone(), uri.to_owned()).await.map_err(|cause| request_error(thread_id, "request-failed", Some(cause)))
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}

// ---- documents (McpAppSnapshot.ts and the helpers of shared/mcpApp.ts) ----------------------

fn read_domains(value: &Value) -> Option<Vec<String>> {
    use regex::Regex;
    use std::sync::OnceLock;
    static DOMAIN: OnceLock<Regex> = OnceLock::new();
    // Only scheme-qualified origins, optionally with a leading `*.` subdomain wildcard, survive.
    let domain = DOMAIN.get_or_init(|| Regex::new(r"(?i)^(?:https?|wss?)://(?:\*\.)?[a-z0-9-]+(?:\.[a-z0-9-]+)*(?::\d{1,5})?$").unwrap());
    let domains: Vec<String> = value.as_array()?.iter().filter_map(Value::as_str).filter(|text| text.encode_utf16().count() <= 256 && domain.is_match(text)).take(32).map(str::to_owned).collect();
    (!domains.is_empty()).then_some(domains)
}

/// The well-formed part of a resource's declared `_meta.ui.csp`.
pub fn read_csp(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut csp = Map::new();
    for key in ["connectDomains", "resourceDomains", "frameDomains", "baseUriDomains"] {
        if let Some(domains) = read_domains(&value[key]) {
            csp.insert(key.into(), json!(domains));
        }
    }
    (!csp.is_empty()).then_some(Value::Object(csp))
}

pub fn read_permissions(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut permissions = Map::new();
    for key in ["camera", "microphone", "geolocation", "clipboardWrite"] {
        if value[key].is_object() {
            permissions.insert(key.into(), json!({}));
        }
    }
    (!permissions.is_empty()).then_some(Value::Object(permissions))
}

/// The Content-Security-Policy for an app document: undeclared origins stay blocked, and nothing
/// loads from the host's own origin (`'self'` would reach T3's API).
pub fn content_security_policy(csp: Option<&Value>) -> String {
    let join = |key: &str| csp.and_then(|csp| csp[key].as_array()).map(|domains| domains.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default();
    let resources = join("resourceDomains");
    let list = |key: &str, fallback: &str| { let joined = join(key); if joined.is_empty() { fallback.to_owned() } else { joined } };
    [
        "default-src 'none'".to_owned(),
        format!("script-src 'unsafe-inline' {resources}").trim().to_owned(),
        format!("style-src 'unsafe-inline' {resources}").trim().to_owned(),
        format!("img-src data: blob: {resources}").trim().to_owned(),
        format!("font-src data: {resources}").trim().to_owned(),
        format!("media-src data: blob: {resources}").trim().to_owned(),
        format!("connect-src {}", list("connectDomains", "'none'")),
        format!("frame-src {}", list("frameDomains", "'none'")),
        "object-src 'none'".to_owned(),
        format!("base-uri {}", list("baseUriDomains", "'none'")),
        "form-action 'none'".to_owned(),
    ]
    .join("; ")
}

/// Puts the app's policy at the top of its document as a CSP meta tag, ahead of any script, after a
/// leading doctype so the document stays in standards mode.
pub fn inject_csp(html: &str, csp: Option<&Value>) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static DOCTYPE: OnceLock<Regex> = OnceLock::new();
    let doctype = DOCTYPE.get_or_init(|| Regex::new(r"(?i)^\x{FEFF}?\s*<!doctype[^>]*>").unwrap());
    let policy = content_security_policy(csp).replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    let meta = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">");
    match doctype.find(html) {
        Some(found) => format!("{}{meta}{}", found.as_str(), &html[found.end()..]),
        None => format!("<!doctype html>{meta}{html}"),
    }
}

/// The HTML document in a `resources/read` result, with the `_meta.ui` the resource declared.
/// `Err(())` is a document too large to store; `Ok(None)` is no app document at all.
fn read_document(contents: &[Value], uri: &str) -> Result<Option<(String, Value)>, ()> {
    use base64::Engine;
    for content in contents {
        if !content.is_object() || content.get("uri").is_some_and(|content_uri| !content_uri.is_null() && content_uri != uri) {
            continue;
        }
        let mime = content["mimeType"].as_str().unwrap_or_default();
        if mime.split_whitespace().collect::<String>().to_lowercase() != APP_MIME_TYPE {
            continue;
        }
        // Sizes are checked before decoding: a string's UTF-8 form is at least its length, and
        // base64 decodes to three quarters of its own.
        let html = match (content["text"].as_str(), content["blob"].as_str()) {
            (Some(text), _) => (text.encode_utf16().count() <= APP_MAX_HTML_BYTES).then(|| text.to_owned()).ok_or(())?,
            (None, Some(blob)) => {
                if blob.encode_utf16().count() > APP_MAX_HTML_BYTES.div_ceil(3) * 4 {
                    return Err(());
                }
                let cleaned: String = blob.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_')).map(|c| match c { '-' => '+', '_' => '/', c => c }).collect();
                String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned).unwrap_or_default()).into_owned()
            }
            (None, None) => continue,
        };
        if html.trim().is_empty() {
            continue;
        }
        return Ok(Some((html, content["_meta"].get("ui").cloned().unwrap_or(Value::Null))));
    }
    Ok(None)
}

/// Stores an app's document as a thread attachment and returns the reference clients host it from
/// (`snapshotMcpApp`). Saving a snapshot keeps the app viewable after the provider exits;
/// interaction still goes through a live provider session. `None` when the resource is not an app
/// document or is too large. An adapter calls this when a tool call that names an app completes.
pub fn snapshot_app(attachments_dir: &std::path::Path, thread_id: &str, server: &str, tool: &str, resource_uri: &str, contents: &[Value]) -> Option<Value> {
    if !resource_uri.starts_with(APP_RESOURCE_SCHEME) {
        return None;
    }
    let (html, meta) = match read_document(contents, resource_uri) {
        Ok(found) => found?,
        Err(()) => {
            tracing::warn!(server, resource_uri, "MCP app document exceeds the size limit.");
            return None;
        }
    };
    if html.len() > APP_MAX_HTML_BYTES {
        tracing::warn!(server, resource_uri, "MCP app document exceeds the size limit.");
        return None;
    }
    // The `-html` suffix makes the asset route serve it as a sandboxed document.
    let attachment_id = crate::attachment_store::create_attachment_id(thread_id, Some("html"))?;
    let path = crate::attachment_store::resolve_relative_path(attachments_dir, &format!("{attachment_id}.html"))?;
    let ui = if meta.is_object() { meta } else { json!({}) };
    let csp = read_csp(&ui["csp"]);
    std::fs::create_dir_all(path.parent()?).ok()?;
    // Only the returned reference lets thread deletion find the document, so a failed write removes it.
    if std::fs::write(&path, inject_csp(&html, csp.as_ref())).is_err() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let mut reference = Map::new();
    reference.insert("attachmentId".into(), json!(attachment_id));
    reference.insert("server".into(), json!(server));
    reference.insert("tool".into(), json!(tool));
    reference.insert("resourceUri".into(), json!(resource_uri));
    if let Some(csp) = csp {
        reference.insert("csp".into(), csp);
    }
    if let Some(permissions) = read_permissions(&ui["permissions"]) {
        reference.insert("permissions".into(), permissions);
    }
    if let Some(prefers_border) = ui["prefersBorder"].as_bool() {
        reference.insert("prefersBorder".into(), json!(prefers_border));
    }
    Some(Value::Object(reference))
}
