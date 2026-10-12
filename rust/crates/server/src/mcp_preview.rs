//! The browser tools an agent calls (`preview_*`) and the two that list and close a thread's tabs (`t3_preview_*`)
//! (apps/server/src/mcp/toolkits/preview and previewControls, and the snapshot's registration in McpHttpServer.ts).
//!
//! A `preview_*` call is checked for access, handed to the broker as one operation for a browser host to run, and its answer is cut to the
//! shape the tool promises. When no host is registered the broker's answer is the original's "no preview automation host" failure.
use crate::{
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpCapability, McpFailure},
    preview_automation_broker::{Invocation, PreviewAutomationBroker},
    preview_manager::PreviewManager,
};
use serde_json::{Map, Value, json};
use std::path::Path;
use t3_contracts::{
    PREVIEW_RECORDING_STOP_TIMEOUT_MS, PreviewAutomationError, PreviewAutomationOperation, PreviewAutomationRecordingArtifact, PreviewAutomationRecordingStatus,
    PreviewAutomationResizeResult, PreviewAutomationSelectResult, PreviewAutomationSetColorSchemeResult, PreviewAutomationSnapshot, PreviewAutomationStatus,
    PreviewTabId, PROVIDER_SEND_TURN_MAX_FILE_BYTES,
};

type Out = Result<Value, McpFailure>;

/// How a tool is gated before it reaches the broker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    Reads,
    Acts,
}

/// What a tool's answer is cut to: the original encodes each answer with the tool's success schema, which drops what the schema does not name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Status,
    Resize,
    ColorScheme,
    Select,
    RecordingStatus,
    /// Only the `toolIcon`.
    Action,
}

/// Every `preview_*` tool: its gate, the operation a host runs for it, whether its own `timeoutMs` is the budget, and the shape of its answer.
/// `preview_snapshot`, `preview_evaluate` and `preview_recording_stop` have their own handling and are matched by name.
const TOOLS: &[(&str, Gate, PreviewAutomationOperation, bool, Shape)] = &[
    ("preview_dialog", Gate::Acts, PreviewAutomationOperation::Dialog, false, Shape::Status),
    ("preview_status", Gate::Reads, PreviewAutomationOperation::Status, false, Shape::Status),
    ("preview_open", Gate::Acts, PreviewAutomationOperation::Open, false, Shape::Status),
    ("preview_navigate", Gate::Acts, PreviewAutomationOperation::Navigate, true, Shape::Status),
    ("preview_resize", Gate::Acts, PreviewAutomationOperation::Resize, true, Shape::Resize),
    ("preview_set_appearance", Gate::Acts, PreviewAutomationOperation::SetColorScheme, false, Shape::ColorScheme),
    ("preview_click", Gate::Acts, PreviewAutomationOperation::Click, true, Shape::Action),
    ("preview_type", Gate::Acts, PreviewAutomationOperation::Type, true, Shape::Action),
    ("preview_hover", Gate::Acts, PreviewAutomationOperation::Hover, true, Shape::Action),
    ("preview_select", Gate::Acts, PreviewAutomationOperation::Select, true, Shape::Select),
    ("preview_drag", Gate::Acts, PreviewAutomationOperation::Drag, true, Shape::Action),
    ("preview_upload", Gate::Acts, PreviewAutomationOperation::Upload, true, Shape::Action),
    ("preview_press", Gate::Acts, PreviewAutomationOperation::Press, false, Shape::Action),
    ("preview_scroll", Gate::Acts, PreviewAutomationOperation::Scroll, false, Shape::Action),
    ("preview_wait_for", Gate::Reads, PreviewAutomationOperation::WaitFor, true, Shape::Action),
    ("preview_recording_start", Gate::Acts, PreviewAutomationOperation::RecordingStart, false, Shape::RecordingStatus),
];

/// Every tool this module answers, by name.
pub(crate) const PREVIEW_TOOLS: &[&str] = &[
    "preview_dialog",
    "preview_status",
    "preview_open",
    "preview_navigate",
    "preview_resize",
    "preview_set_appearance",
    "preview_snapshot",
    "preview_click",
    "preview_type",
    "preview_hover",
    "preview_select",
    "preview_drag",
    "preview_upload",
    "preview_press",
    "preview_scroll",
    "preview_evaluate",
    "preview_wait_for",
    "preview_recording_start",
    "preview_recording_stop",
];

/// A failure of the tool as the agent reads it: the typed error, with the text its class computes.
fn automation_failure(error: &PreviewAutomationError) -> McpFailure {
    let mut value = serde_json::to_value(error).expect("an error is JSON");
    value["message"] = json!(error.message());
    McpFailure(value)
}

/// A host's answer, kept to the members a success schema names; one that does not fit the schema is a failure of the tool.
fn through<T: serde::Serialize + serde::de::DeserializeOwned>(result: Value) -> Result<Map<String, Value>, McpFailure> {
    let typed: T = serde_json::from_value(result).map_err(|_| malformed_answer())?;
    match serde_json::to_value(typed) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(malformed_answer()),
    }
}

fn malformed_answer() -> McpFailure {
    McpFailure(json!({"_tag":"AiError","message":"The browser host's answer did not match what the tool returns."}))
}

fn tab_of(args: &Value) -> Option<PreviewTabId> {
    args.get("tabId").and_then(Value::as_str).and_then(|id| PreviewTabId::new(id).ok())
}

/// `args` without the tab, which the broker routes by.
fn without_tab(args: &Value) -> Value {
    let mut input = args.clone();
    if let Some(object) = input.as_object_mut() {
        object.remove("tabId");
    }
    input
}

/// Folds the `show` alias into `open` and defaults tab reuse. An unstated `open` stays unstated: whether a preview the agent said nothing
/// about surfaces is the user's own preference, unreadable from here, so filling in `true` would override it for every `preview_open`.
pub(crate) fn normalize_open_input(mut input: Value) -> Value {
    let open = input.get("open").or_else(|| input.get("show")).and_then(Value::as_bool);
    let reuse = input.get("reuseExistingTab").and_then(Value::as_bool).unwrap_or(true);
    let object = input.as_object_mut().expect("tool input is an object");
    if let Some(open) = open {
        object.insert("open".into(), json!(open));
        object.insert("show".into(), json!(open));
    }
    object.insert("reuseExistingTab".into(), json!(reuse));
    input
}

/// UTF-16 length, as JavaScript's `.length` counts it.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.length > max ? `${text.slice(0, max)}…` : text`, without splitting a character.
pub(crate) fn cut_text(text: &str, max: usize) -> String {
    if js_len(text) <= max {
        return text.to_owned();
    }
    let mut units = 0;
    let mut cut: String = text
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect();
    cut.push('…');
    cut
}

// What a snapshot keeps. Claude Code moves an MCP result above its output limit to a file and hands the agent a notice instead, so a
// snapshot carrying the full accessibility tree and page text would lose its locators too. It also shows the model `structuredContent`
// in place of the text blocks when a result has both, so both carry the same bounded snapshot, near 20 KB.
pub(crate) const MAX_SNAPSHOT_TEXT_BYTES: usize = 20_000;
const MAX_SNAPSHOT_VISIBLE_TEXT_CHARS: usize = 8_000;
const MAX_SNAPSHOT_ELEMENT_NAME_CHARS: usize = 200;
const MAX_SNAPSHOT_LOG_ENTRIES: usize = 40;
const MAX_SNAPSHOT_LOG_TEXT_CHARS: usize = 500;
const MAX_SNAPSHOT_IDENTIFIER_CHARS: usize = 2_048;
const MAX_SNAPSHOT_TREE_CHARS: usize = 20_000;

/// The snapshot bounded: server ARIA refs kept and legacy object trees dropped, page text, names, identifiers and log strings shortened,
/// only the newest log entries kept and finally interactive elements shed until the JSON fits. Returns the value, its text and notes on what
/// is missing, so the agent can reach for `preview_evaluate`.
pub(crate) fn bound_snapshot_metadata(metadata: &Map<String, Value>) -> (Map<String, Value>, String, Vec<String>) {
    let mut omitted: Vec<String> = Vec::new();
    let mut without_tree = metadata.clone();
    let tree = without_tree.remove("accessibilityTree");
    let aria_tree = tree.as_ref().and_then(Value::as_str).map(str::to_owned);
    if tree.is_some() && aria_tree.is_none() {
        omitted.push("accessibilityTree (use interactiveElements locators or preview_evaluate)".into());
    }
    let strings = |key: &str| metadata.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (url, title, visible_text) = (strings("url"), strings("title"), strings("visibleText"));
    let list = |key: &str| metadata.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let elements = list("interactiveElements");
    let has_long_string = |entry: &Value| entry.as_object().is_some_and(|object| object.values().any(|value| value.as_str().is_some_and(|text| js_len(text) > MAX_SNAPSHOT_LOG_TEXT_CHARS)));
    let cut_entry_strings = |entry: &Value| match entry {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().map_or_else(|| value.clone(), |text| json!(cut_text(text, MAX_SNAPSHOT_LOG_TEXT_CHARS)))))
                .collect(),
        ),
        other => other.clone(),
    };
    let tail = |omitted: &mut Vec<String>, entries: Vec<Value>, label: &str| {
        if entries.len() > MAX_SNAPSHOT_LOG_ENTRIES {
            omitted.push(format!("{} older {label}", entries.len() - MAX_SNAPSHOT_LOG_ENTRIES));
        }
        let kept: Vec<Value> = entries[entries.len().saturating_sub(MAX_SNAPSHOT_LOG_ENTRIES)..].to_vec();
        if kept.iter().any(&has_long_string) {
            omitted.push(format!("{label} text after {MAX_SNAPSHOT_LOG_TEXT_CHARS} characters"));
        }
        kept.iter().map(&cut_entry_strings).collect::<Vec<_>>()
    };
    if js_len(&url) > MAX_SNAPSHOT_IDENTIFIER_CHARS || js_len(&title) > MAX_SNAPSHOT_IDENTIFIER_CHARS {
        omitted.push(format!("url or title after {MAX_SNAPSHOT_IDENTIFIER_CHARS} characters"));
    }
    if elements.iter().any(|element| element.get("name").and_then(Value::as_str).is_some_and(|name| js_len(name) > MAX_SNAPSHOT_ELEMENT_NAME_CHARS)) {
        omitted.push(format!("element names longer than {MAX_SNAPSHOT_ELEMENT_NAME_CHARS} characters"));
    }
    let console = tail(&mut omitted, list("consoleEntries"), "console entries");
    let network = tail(&mut omitted, list("networkEntries"), "network entries");
    let timeline = tail(&mut omitted, list("actionTimeline"), "action timeline entries");
    let mut bounded = without_tree;
    bounded.insert("url".into(), json!(cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS)));
    bounded.insert("title".into(), json!(cut_text(&title, MAX_SNAPSHOT_IDENTIFIER_CHARS)));
    let named: Vec<Value> = elements
        .iter()
        .map(|element| {
            let mut element = element.clone();
            if let Some(name) = element.get("name").and_then(Value::as_str).map(|name| cut_text(name, MAX_SNAPSHOT_ELEMENT_NAME_CHARS)) {
                element["name"] = json!(name);
            }
            element
        })
        .collect();

    // Per-field caps do not sum below the ceiling: three log arrays of 40 capped entries alone can pass 60 KB, and the caps count
    // characters, not bytes. Halve one thing per round until the JSON fits: logs first, then page text, then the locators. The identifier
    // caps bound the rest, so this ends.
    const SHED_ORDER: [&str; 4] = ["actionTimeline", "networkEntries", "consoleEntries", "interactiveElements"];
    let mut lists: Vec<(&str, Vec<Value>, usize)> = vec![
        ("interactiveElements", named.clone(), named.len()),
        ("consoleEntries", console.clone(), console.len()),
        ("networkEntries", network.clone(), network.len()),
        ("actionTimeline", timeline.clone(), timeline.len()),
    ];
    let mut dropped: Vec<usize> = vec![0; 4];
    let mut visible_chars = js_len(&visible_text).min(MAX_SNAPSHOT_VISIBLE_TEXT_CHARS);
    let mut tree_chars = aria_tree.as_deref().map_or(0, |tree| js_len(tree).min(MAX_SNAPSHOT_TREE_CHARS));
    let value = |lists: &[(&str, Vec<Value>, usize)], visible_chars: usize, tree_chars: usize| {
        let mut value = bounded.clone();
        if let Some(tree) = &aria_tree {
            value.insert("accessibilityTree".into(), json!(cut_text(tree, tree_chars)));
        }
        value.insert("visibleText".into(), json!(cut_text(&visible_text, visible_chars)));
        for (key, entries, _) in lists {
            value.insert((*key).into(), Value::Array(entries.clone()));
        }
        value
    };
    let index_of = |key: &str| match key {
        "interactiveElements" => 0,
        "consoleEntries" => 1,
        "networkEntries" => 2,
        _ => 3,
    };
    let mut text = serde_json::to_string(&value(&lists, visible_chars, tree_chars)).expect("JSON");
    while text.len() > MAX_SNAPSHOT_TEXT_BYTES {
        // Elements carry the locators, so they go last; logs shed newest-last.
        let log = SHED_ORDER.iter().find(|key| **key != "interactiveElements" && !lists[index_of(key)].1.is_empty());
        if let Some(key) = log {
            shed(&mut lists, &mut dropped, index_of(key), false);
        } else if visible_chars > 0 {
            visible_chars /= 2;
        } else if !lists[index_of("interactiveElements")].1.is_empty() {
            shed(&mut lists, &mut dropped, index_of("interactiveElements"), true);
        } else if tree_chars > 0 {
            tree_chars /= 2;
        } else {
            break;
        }
        text = serde_json::to_string(&value(&lists, visible_chars, tree_chars)).expect("JSON");
    }
    if visible_chars < js_len(&visible_text) {
        omitted.push(format!("visibleText after {visible_chars} characters (use preview_evaluate for more)"));
    }
    if let Some(tree) = &aria_tree {
        if tree_chars < js_len(tree) {
            omitted.push(format!("accessibilityTree after {tree_chars} characters"));
        }
    }
    for key in SHED_ORDER {
        let at = index_of(key);
        if dropped[at] > 0 {
            omitted.push(format!("{} of {} {key}", dropped[at], lists[at].2));
        }
    }
    (value(&lists, visible_chars, tree_chars), text, omitted)
}

/// Halves one list, keeping its first half (the elements) or its last (a log).
fn shed(lists: &mut [(&str, Vec<Value>, usize)], dropped: &mut [usize], at: usize, keep_head: bool) {
    let length = lists[at].1.len();
    let keep = length / 2;
    dropped[at] += length - keep;
    lists[at].1 = if keep == 0 {
        Vec::new()
    } else if keep_head {
        lists[at].1[..keep].to_vec()
    } else {
        lists[at].1[length - keep..].to_vec()
    };
}

/// A hostname as a filename-safe slug, matching the desktop's own screenshot names.
fn screenshot_site_slug(raw_url: &str) -> String {
    let host = url::Url::parse(raw_url).ok().and_then(|url| url.host_str().map(str::to_lowercase));
    let Some(host) = host else { return "site".into() };
    let mut slug = String::new();
    for c in host.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed: String = slug.trim_matches('-').chars().take(40).collect();
    let trimmed = trimmed.trim_end_matches('-');
    if trimmed.is_empty() { "site".into() } else { trimmed.into() }
}

fn base36(mut n: u128) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit((n % 36) as u32, 36).expect("a base 36 digit"));
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// Writes the snapshot PNG under the browser artifacts directory and returns its path.
async fn save_screenshot(artifacts_dir: &Path, page_url: &str, png: &[u8], millis: i64) -> std::io::Result<String> {
    // Two saves in the same millisecond must not overwrite each other.
    let unique: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
    let path = artifacts_dir.join(format!("browser-screenshot-{}-{}-{unique}.png", screenshot_site_slug(page_url), base36(millis.max(0) as u128)));
    tokio::fs::create_dir_all(artifacts_dir).await?;
    tokio::fs::write(&path, png).await?;
    Ok(path.to_string_lossy().into_owned())
}

/// Whether `path` is the file the host said it uploaded: a regular file of that size, within what a message may carry. An error is the
/// file system's, or none when the file is not what was described.
async fn check_recording_file(path: &Path, size: u64) -> Result<(), Option<std::io::Error>> {
    let meta = tokio::fs::metadata(path).await.map_err(Some)?;
    if meta.is_file() && meta.len() == size && size > 0 && size <= PROVIDER_SEND_TURN_MAX_FILE_BYTES { Ok(()) } else { Err(None) }
}

fn snapshot_failure(tag: &str, message: Option<&str>) -> Value {
    let mut error = json!({"_tag":tag,"operation":"snapshot","failureCount":1});
    if let Some(message) = message {
        error["message"] = json!(message);
    }
    json!({"isError":true,"structuredContent":{"error":error},"content":[{"type":"text","text":format!("Preview snapshot failed: {}", message.map_or_else(|| format!("{tag}."), str::to_owned))}]})
}

impl McpControlTools {
    fn preview_broker(&self) -> std::sync::Arc<PreviewAutomationBroker> {
        self.store.extension(PreviewAutomationBroker::default)
    }
    fn preview_manager(&self) -> std::sync::Arc<PreviewManager> {
        self.store.extension(PreviewManager::default)
    }

    /// The gate every `preview_*` tool starts with, in the original's order: the caller's access, the preview capability, a thread.
    fn preview_gate(&self, scope: &InvocationScope, gate: Gate) -> Result<(), McpFailure> {
        mcp_access::check_store(&self.store, scope, if gate == Gate::Reads { Access::ReadsAsCaller } else { Access::ActsAsCaller }, &[])?;
        scope.require_capability(McpCapability::Preview, true)?;
        scope.require_thread("This tool")?;
        Ok(())
    }

    /// Runs `operation` for the calling thread and, for an operation that changes the page, reads the page's URL afterwards so the tool's row
    /// can show the site's icon. That read is best-effort and short, and never changes the session's current tab.
    async fn preview_invoke(&self, scope: &InvocationScope, operation: PreviewAutomationOperation, input: Value, timeout_ms: Option<u64>, tab_id: Option<PreviewTabId>) -> Result<(Value, Option<Value>), McpFailure> {
        let broker = self.preview_broker();
        let mut invocation = Invocation::new(scope, operation, input);
        invocation.timeout_ms = timeout_ms;
        invocation.tab_id = tab_id;
        let invoked = broker.invoke(invocation).await.map_err(|error| automation_failure(&error))?;
        if matches!(operation, PreviewAutomationOperation::Status | PreviewAutomationOperation::Open | PreviewAutomationOperation::Navigate | PreviewAutomationOperation::Snapshot) {
            return Ok((invoked.result, None));
        }
        let from_result = (operation != PreviewAutomationOperation::Evaluate)
            .then(|| invoked.result.get("tabId").and_then(Value::as_str).and_then(|id| PreviewTabId::new(id).ok()))
            .flatten();
        let status_tab = from_result.or(invoked.target_tab);
        let mut status = Invocation::new(scope, PreviewAutomationOperation::Status, json!({}));
        status.timeout_ms = Some(500);
        status.update_current_tab = false;
        status.tab_id = status_tab;
        let page = broker.invoke(status).await.ok();
        let icon = page.as_ref().and_then(|page| page.result.get("url")).and_then(Value::as_str).filter(|url| {
            let lower = url.to_ascii_lowercase();
            (lower.starts_with("http://") || lower.starts_with("https://")) && js_len(url) <= 4096
        });
        Ok((invoked.result, icon.map(|url| json!({"_tag":"website","pageUrl":url}))))
    }

    /// Any `preview_*` tool but the snapshot, which `preview_snapshot` handles.
    pub(crate) async fn preview_tool(&self, scope: &InvocationScope, name: &str, args: Value) -> Out {
        match name {
            "preview_evaluate" => {
                self.preview_gate(scope, Gate::Acts)?;
                let (result, icon) = self.preview_invoke(scope, PreviewAutomationOperation::Evaluate, without_tab(&args), None, tab_of(&args)).await?;
                let mut out = Map::new();
                out.insert("value".into(), if result.is_null() { Value::Null } else { result });
                if let Some(icon) = icon {
                    out.insert("toolIcon".into(), icon);
                }
                return Ok(Value::Object(out));
            }
            "preview_recording_stop" => {
                self.preview_gate(scope, Gate::Acts)?;
                let mut input = without_tab(&args);
                input["transferToEnvironment"] = json!(true);
                let (result, icon) = self.preview_invoke(scope, PreviewAutomationOperation::RecordingStop, input, Some(PREVIEW_RECORDING_STOP_TIMEOUT_MS), tab_of(&args)).await?;
                let thread = scope.require_thread("This tool")?;
                let mut artifact = self.claim_recording(thread.thread_id.as_str(), result).await?;
                if let Some(icon) = icon {
                    artifact.insert("toolIcon".into(), icon);
                }
                return Ok(Value::Object(artifact));
            }
            _ => {}
        }
        let Some((_, gate, operation, own_timeout, shape)) = TOOLS.iter().find(|(tool, ..)| *tool == name) else { return Err(mcp_access::unavailable()) };
        self.preview_gate(scope, *gate)?;
        let mut input = without_tab(&args);
        if *operation == PreviewAutomationOperation::Open {
            input = normalize_open_input(input);
        }
        if input.is_null() {
            input = json!({});
        }
        let timeout = own_timeout.then(|| args.get("timeoutMs").and_then(Value::as_u64)).flatten();
        let (result, icon) = self.preview_invoke(scope, *operation, input, timeout, tab_of(&args)).await?;
        let mut out = match shape {
            Shape::Status => through::<PreviewAutomationStatus>(result)?,
            Shape::Resize => through::<PreviewAutomationResizeResult>(result)?,
            Shape::ColorScheme => through::<PreviewAutomationSetColorSchemeResult>(result)?,
            Shape::Select => through::<PreviewAutomationSelectResult>(result)?,
            Shape::RecordingStatus => through::<PreviewAutomationRecordingStatus>(result)?,
            Shape::Action => Map::new(),
        };
        if let Some(icon) = icon {
            out.insert("toolIcon".into(), icon);
        }
        Ok(Value::Object(out))
    }

    /// Takes the recording a host uploaded (a pending attachment) as this thread's own: the file is checked against what the host said, then
    /// renamed from the pending name to the thread's. A second stop that returns the same completed upload finds it already claimed.
    async fn claim_recording(&self, thread_id: &str, response: Value) -> Result<Map<String, Value>, McpFailure> {
        let transfer = |cause: Option<&str>| -> McpFailure {
            let mut error = json!({"_tag":"PreviewAutomationRecordingTransferError","threadId":thread_id});
            if let Some(cause) = cause {
                error["cause"] = json!({"name":"Error","message":cause});
            }
            let typed: PreviewAutomationError = serde_json::from_value(error).expect("a recording error");
            automation_failure(&typed)
        };
        // The host's description of the recording, and the pending attachment it uploaded the file as (which must be text if it is there).
        let uploaded = match response.get("uploadedAttachmentId") {
            None => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return Err(transfer(Some("The recording's description did not match."))),
        };
        let artifact = through::<PreviewAutomationRecordingArtifact>(response).map_err(|_| transfer(Some("The recording's description did not match.")))?;
        let Some(uploaded_id) = uploaded.filter(|id| !id.is_empty()) else {
            let typed: PreviewAutomationError = serde_json::from_value(json!({"_tag":"PreviewAutomationRecordingDesktopUpdateRequiredError","threadId":thread_id})).expect("a recording error");
            return Err(automation_failure(&typed));
        };
        let attachments = crate::attachment_service::AttachmentService::for_store(&self.store).ok_or_else(|| transfer(None))?;
        let dir = attachments.attachments_dir().to_path_buf();
        let uuid = crate::attachment_store::parse_attachment_uuid(&uploaded_id);
        let extension = crate::attachment_store::parse_attachment_file_extension(&uploaded_id);
        let segment = crate::attachment_store::safe_thread_segment(thread_id);
        let (Some(uuid), Some(extension), Some(segment)) = (uuid, extension, segment) else { return Err(transfer(None)) };
        let pending_id = format!("{}-{uuid}-{extension}", crate::attachment_store::PENDING_ATTACHMENT_THREAD_SEGMENT);
        if uploaded_id != pending_id {
            return Err(transfer(None));
        }
        // The same completed upload can be returned to overlapping stop requests.
        let final_id = format!("{segment}-{uuid}-{extension}");
        let current = crate::attachment_store::resolve_relative_path(&dir, &format!("{pending_id}.{extension}"));
        let target = crate::attachment_store::resolve_relative_path(&dir, &format!("{final_id}.{extension}"));
        let (Some(current), Some(target)) = (current, target) else { return Err(transfer(None)) };
        let size = artifact.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0);
        match check_recording_file(&current, size).await {
            Ok(()) => tokio::fs::rename(&current, &target).await.map_err(|error| transfer(Some(&error.to_string())))?,
            // Another stop may already have claimed this exact upload for this thread.
            Err(Some(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                check_recording_file(&target, size).await.map_err(|error| transfer(error.map(|e| e.to_string()).as_deref()))?;
            }
            Err(error) => return Err(transfer(error.map(|e| e.to_string()).as_deref())),
        }
        let mut claimed = artifact;
        claimed.remove("uploadedAttachmentId");
        claimed.insert("id".into(), json!(final_id));
        claimed.insert("path".into(), json!(target.to_string_lossy()));
        Ok(claimed)
    }

    /// `preview_snapshot`: the page's state and a screenshot, bounded for the model and optionally saved to disk. It answers with the whole MCP
    /// result, since the image goes out as an image block and the metadata as JSON text.
    pub(crate) async fn preview_snapshot(&self, scope: &InvocationScope, args: Value) -> Value {
        let outcome = self.preview_snapshot_inner(scope, &args).await;
        match outcome {
            Ok(result) => result,
            Err(error) => {
                let tag = error.0["_tag"].as_str().unwrap_or("PreviewSnapshotError").to_owned();
                // Only the errors the contract names carry text, built by the server and never from page output.
                let typed = serde_json::from_value::<PreviewAutomationError>(error.0.clone()).ok().map(|error| error.message());
                snapshot_failure(&tag, typed.as_deref())
            }
        }
    }

    async fn preview_snapshot_inner(&self, scope: &InvocationScope, args: &Value) -> Out {
        self.preview_gate(scope, Gate::Reads)?;
        // Output selection and saving are for the tool alone; the browser still produces a complete snapshot.
        let mut input = without_tab(args);
        if let Some(object) = input.as_object_mut() {
            object.remove("includeImage");
            object.remove("save");
        }
        let (result, _) = self.preview_invoke(scope, PreviewAutomationOperation::Snapshot, input, None, tab_of(args)).await?;
        let snapshot = serde_json::from_value::<PreviewAutomationSnapshot>(result.clone()).map_err(|_| malformed_answer())?;
        let snapshot = serde_json::to_value(&snapshot).expect("a snapshot is JSON");
        let Value::Object(mut page) = snapshot else { return Err(malformed_answer()) };
        let screenshot = page.remove("screenshot").unwrap_or(Value::Null);
        let url = page["url"].as_str().unwrap_or_default().to_owned();
        let include_image = args.get("includeImage") != Some(&Value::Bool(false));
        let screenshot_path = if args.get("save") == Some(&Value::Bool(true)) {
            use base64::Engine;
            let png = base64::engine::general_purpose::STANDARD.decode(screenshot["data"].as_str().unwrap_or_default()).map_err(|_| malformed_answer())?;
            let state_dir = self.store.state_dir().ok_or_else(|| McpFailure(json!({"_tag":"PreviewScreenshotSaveError","screenshotPath":""})))?;
            let millis = (self.clock)().timestamp_millis();
            let saved = save_screenshot(&state_dir.join("browser-artifacts"), &url, &png, millis).await;
            Some(saved.map_err(|_| McpFailure(json!({"_tag":"PreviewScreenshotSaveError"})))?)
        } else {
            None
        };
        if let (Some(path), false) = (&screenshot_path, include_image) {
            // The agent only wants a file to show the user. The url keeps the site icon on the tool row.
            let saved = json!({"url":cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS),"screenshotPath":path});
            return Ok(json!({"isError":false,"structuredContent":saved,"content":[{"type":"text","text":saved.to_string()}]}));
        }
        let mut metadata = page;
        metadata.insert("screenshot".into(), json!({"mimeType":screenshot["mimeType"],"width":screenshot["width"],"height":screenshot["height"]}));
        if let Some(path) = &screenshot_path {
            metadata.insert("screenshotPath".into(), json!(path));
        }
        let (value, text, omitted) = bound_snapshot_metadata(&metadata);
        let mut structured = value;
        if !omitted.is_empty() {
            structured.insert("omitted".into(), json!(omitted));
        }
        let mut content = vec![json!({"type":"text","text":json!({"url":cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS)}).to_string()}), json!({"type":"text","text":text})];
        if !omitted.is_empty() {
            content.push(json!({"type":"text","text":format!("Snapshot text was bounded. Omitted: {}.", omitted.join("; "))}));
        }
        if include_image {
            content.push(json!({"type":"image","data":screenshot["data"],"mimeType":screenshot["mimeType"]}));
        }
        Ok(json!({"isError":false,"structuredContent":structured,"content":content}))
    }

    /// `t3_preview_list`: this thread's tabs, a page at a time.
    pub(crate) fn preview_list(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ReadsAsCaller, &[])?;
        scope.require_capability(McpCapability::Preview, true)?;
        let thread = scope.require_thread("This tool")?;
        let listed = self.preview_manager().list(serde_json::from_value(json!({"threadId":thread.thread_id})).expect("a list input"));
        let start = args.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize;
        let end = start + args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let total = listed.sessions.len();
        let mut result = serde_json::to_value(&listed).expect("a list is JSON");
        result["sessions"] = Value::Array(result["sessions"].as_array().map(|sessions| sessions.iter().skip(start).take(end - start).cloned().collect()).unwrap_or_default());
        result["nextCursor"] = if end < total { json!(end) } else { Value::Null };
        Ok(result)
    }

    /// `t3_preview_close`: closes one tab of this thread. A tab of the server's browser is closed through that browser; any other tab is dropped
    /// from the sessions, and its renderer cleans up when it hears of it. A tab that is not there is not an error.
    pub(crate) async fn preview_close(&self, scope: &InvocationScope, args: Value) -> Out {
        mcp_access::check_store(&self.store, scope, Access::ActsAsCaller, &[])?;
        scope.require_capability(McpCapability::Preview, true)?;
        let thread = scope.require_thread("This tool")?;
        let Some(tab_id) = args.get("tabId").and_then(Value::as_str).and_then(|id| PreviewTabId::new(id).ok()) else { return Err(mcp_access::unavailable()) };
        let manager = self.preview_manager();
        let server_tab = manager
            .session(thread.thread_id.as_str(), tab_id.as_str())
            .is_some_and(|session| session.runtime == Some(Some(t3_contracts::PreviewRuntime::Server)));
        if server_tab {
            let mut invocation = Invocation::new(scope, PreviewAutomationOperation::Close, json!({}));
            invocation.tab_id = Some(tab_id);
            self.preview_broker().invoke(invocation).await.map_err(|_| mcp_access::unavailable())?;
            return Ok(json!({}));
        }
        manager.close(serde_json::from_value(json!({"threadId":thread.thread_id,"tabId":tab_id})).expect("a close input"));
        Ok(json!({}))
    }
}
