//! Pi session events to persisted orchestration events. One [`Projector`] lives for one provider turn. It is
//! pure with respect to I/O: the adapter hands it each record and the current projection and commits the events
//! it returns. Mirrors the item handling of `PiAdapterV2.ts` (streamed text and reasoning, tools, compaction,
//! provider retries, extension notices and errors, extension dialogs, subagent-extension results).
use crate::{claude_projection::{enc, mcp_presentation}, execution::at, persistence::StoreError, pi_launch, provider_turn_context::TurnContext};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::HashMap;

pub(crate) type Events = Vec<(&'static str, Value)>;
pub(crate) const DRIVER: &str = "pi";

pub(crate) fn item_id(native: &str) -> String {
    format!("turn-item:provider:{DRIVER}:native-item:{}", enc(native))
}
pub(crate) fn node_id(native: &str) -> String {
    format!("node:provider:{DRIVER}:native-item:{}", enc(native))
}
pub(crate) fn message_id(native: &str) -> String {
    format!("message:provider:{DRIVER}:native-item:{}", enc(native))
}
pub(crate) fn native_ref(id: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":id,"strength":"strong"})
}

/// `contentText`: the `text` blocks of a content array, concatenated.
pub(crate) fn content_text(content: &Value) -> String {
    match content {
        Value::Array(blocks) => blocks.iter().filter(|block| block["type"] == "text").filter_map(|block| block["text"].as_str()).collect(),
        Value::String(text) => text.clone(),
        _ => String::new(),
    }
}
/// `nonNegativeInteger`.
pub(crate) fn non_negative(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_f64).filter(|number| number.is_finite()).map(|number| number.trunc().max(0.0) as u64)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StreamKind {
    Text,
    Reasoning,
}
struct Stream {
    kind: StreamKind,
    text: String,
    completed: bool,
    dirty: bool,
    started_at: String,
}
/// An item that exists once and is replaced as it progresses: a compaction or a provider retry.
#[derive(Clone)]
pub(crate) struct Compaction {
    pub native: String,
    pub started_at: String,
}
#[derive(Clone)]
pub(crate) struct Retry {
    pub attempt: u64,
    pub max_attempts: u64,
    pub delay_ms: Option<u64>,
    pub failure: Value,
    pub started_at: String,
    pub ordinal: u64,
}

pub(crate) struct Projector {
    thread_id: String,
    pub(crate) ctx: TurnContext,
    pub(crate) provider_turn: String,
    ordinals: HashMap<String, u64>,
    next_ordinal: Option<u64>,
    message_ordinal: u32,
    compactions: u32,
    stream: indexmap::IndexMap<String, Stream>,
    tool_args: HashMap<String, Value>,
    tool_started: HashMap<String, String>,
    instance_id: String,
}

impl Projector {
    pub(crate) fn new(thread_id: &str, ctx: TurnContext, view: &Value, provider_turn: &str) -> Result<Self, StoreError> {
        crate::codex_runtime::find(view, "runs", &json!(ctx.run_id))?;
        Ok(Self { thread_id: thread_id.into(), instance_id: ctx.provider_instance_id.as_str().unwrap_or_default().to_owned(), ctx, provider_turn: provider_turn.into(), ordinals: Default::default(), next_ordinal: None, message_ordinal: 0, compactions: 0, stream: Default::default(), tool_args: Default::default(), tool_started: Default::default() })
    }
    /// Items are ordered by arrival: each native item takes the next position once.
    pub(crate) fn ordinal(&mut self, view: &Value, native: &str) -> u64 {
        if let Some(existing) = self.ordinals.get(native) {
            return *existing;
        }
        let next = self.next_ordinal.unwrap_or_else(|| view["turnItems"].as_array().into_iter().flatten().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1));
        self.next_ordinal = Some(next + 1);
        self.ordinals.insert(native.to_owned(), next);
        next
    }
    fn base_item(&self, native: &str, node: &str, ordinal: u64, status: &str, started: &str, updated: &str, completed: Option<&str>) -> Value {
        json!({"id":item_id(native),"threadId":self.thread_id,"runId":self.ctx.run_id,"nodeId":node,"providerThreadId":self.ctx.provider_thread_id,"providerTurnId":self.provider_turn,"nativeItemRef":native_ref(native),"parentItemId":null,"ordinal":ordinal,"status":status,"title":null,"startedAt":started,"completedAt":completed,"updatedAt":updated})
    }
    fn node(&self, native: &str, kind: &str, status: &str, started: &str, completed: Option<&str>) -> Value {
        json!({"id":node_id(native),"threadId":self.thread_id,"runId":self.ctx.run_id,"parentNodeId":self.ctx.root_node_id,"rootNodeId":self.ctx.root_node_id,"kind":kind,"status":status,"countsForRun":false,"providerThreadId":self.ctx.provider_thread_id,"providerTurnId":self.provider_turn,"nativeItemRef":native_ref(native),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":completed})
    }

    // ---- streamed text and reasoning ----

    /// An assistant `message_start`: content indexes restart, so item ids stay unique per message.
    pub(crate) fn next_message(&mut self) {
        self.message_ordinal += 1;
    }
    fn stream_key(&self, content_index: u64) -> String {
        format!("{}:m{}:c{}", self.provider_turn, self.message_ordinal, content_index)
    }
    fn stream_item(&mut self, view: &Value, kind: StreamKind, content_index: u64) -> String {
        let native = self.stream_key(content_index);
        if !self.stream.contains_key(&native) {
            self.stream.insert(native.clone(), Stream { kind, text: String::new(), completed: false, dirty: false, started_at: at(Utc::now()) });
            // The position is reserved on the first delta so items appear in stream order.
            self.ordinal(view, &native);
        }
        native
    }
    pub(crate) fn delta(&mut self, view: &Value, kind: StreamKind, content_index: u64, delta: &str) {
        let native = self.stream_item(view, kind, content_index);
        let item = self.stream.get_mut(&native).unwrap();
        if !item.completed {
            item.text.push_str(delta);
            item.dirty = true;
        }
    }
    /// Whether streamed text waits to be written (the adapter flushes on a short timer).
    pub(crate) fn has_pending(&self) -> bool {
        self.stream.values().any(|item| item.dirty && !item.completed)
    }
    pub(crate) fn flush(&mut self, view: &Value) -> Events {
        let natives: Vec<String> = self.stream.iter().filter(|(_, item)| item.dirty && !item.completed).map(|(native, _)| native.clone()).collect();
        natives.iter().flat_map(|native| self.stream_events(view, native, true)).collect()
    }
    pub(crate) fn end(&mut self, view: &Value, kind: StreamKind, content_index: u64, final_text: Option<&str>) -> Events {
        let native = self.stream_item(view, kind, content_index);
        self.complete(view, &native, final_text)
    }
    fn complete(&mut self, view: &Value, native: &str, final_text: Option<&str>) -> Events {
        let item = self.stream.get_mut(native).unwrap();
        if item.completed {
            return vec![];
        }
        item.completed = true;
        item.dirty = false;
        if let Some(text) = final_text.filter(|text| !text.is_empty()) {
            item.text = text.to_owned();
        }
        if item.text.is_empty() { vec![] } else { self.stream_events(view, native, false) }
    }
    pub(crate) fn complete_open(&mut self, view: &Value) -> Events {
        let open: Vec<String> = self.stream.iter().filter(|(_, item)| !item.completed).map(|(native, _)| native.clone()).collect();
        open.iter().flat_map(|native| self.complete(view, native, None)).collect()
    }
    fn stream_events(&mut self, view: &Value, native: &str, streaming: bool) -> Events {
        let now = at(Utc::now());
        if let Some(item) = self.stream.get_mut(native) {
            item.dirty = false;
        }
        let ordinal = self.ordinal(view, native);
        let item = &self.stream[native];
        let (kind, text, started) = (item.kind, item.text.clone(), item.started_at.clone());
        let status = if streaming { "running" } else { "completed" };
        let completed = (!streaming).then_some(now.as_str());
        let node_kind = if kind == StreamKind::Text { "assistant_message" } else { "reasoning" };
        let node = self.node(native, node_kind, status, &started, completed);
        let mut row = self.base_item(native, &node_id(native), ordinal, status, &started, &now, completed);
        row["streaming"] = json!(streaming);
        row["text"] = json!(text);
        if kind == StreamKind::Reasoning {
            row["type"] = json!("reasoning");
            return vec![("node.updated", node), ("turn-item.updated", row)];
        }
        let message = message_id(native);
        row["type"] = json!("assistant_message");
        row["messageId"] = json!(message);
        let message_row = json!({"id":message,"threadId":self.thread_id,"runId":self.ctx.run_id,"nodeId":node_id(native),"role":"assistant","text":text,"attachments":[],"streaming":streaming,"createdBy":"agent","creationSource":"provider","createdAt":started,"updatedAt":now});
        vec![("node.updated", node), ("turn-item.updated", row), ("message.updated", message_row)]
    }

    // ---- tools ----

    /// `emitToolItem`. A Stop aborts in-flight tools, and Pi reports those as error ends: they present as
    /// interrupted (matching the run) rather than failed.
    pub(crate) fn tool(&mut self, view: &Value, event: &Value, phase: &str, interrupted: bool) -> Events {
        let Some(call_id) = event["toolCallId"].as_str().map(str::to_owned) else { return vec![] };
        let tool_name = event["toolName"].as_str().unwrap_or("tool").to_owned();
        if phase == "start" {
            self.tool_args.insert(call_id.clone(), event["args"].clone());
        }
        let args = if event.get("args").is_some_and(|args| !args.is_null()) { event["args"].clone() } else { self.tool_args.get(&call_id).cloned().unwrap_or(Value::Null) };
        let now = at(Utc::now());
        let started = self.tool_started.entry(call_id.clone()).or_insert_with(|| now.clone()).clone();
        let completed = phase == "end";
        let is_error = event["isError"] == true;
        let result = if completed { &event["result"] } else { &event["partialResult"] };
        let output = content_text(&result["content"]);
        let status = if !completed { "running" } else if is_error { if interrupted { "interrupted" } else { "failed" } } else { "completed" };
        let ordinal = self.ordinal(view, &call_id);
        let finished = completed.then_some(now.as_str());
        let node = self.node(&call_id, "tool_call", status, &started, finished);
        let mut item = self.base_item(&call_id, &node_id(&call_id), ordinal, status, &started, &now, finished);
        item["title"] = json!(tool_name);
        let mut events = vec![("node.updated", node)];
        let string_arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
        if tool_name == "bash" {
            item["type"] = json!("command_execution");
            item["input"] = json!(string_arg("command").unwrap_or_default());
            if !output.is_empty() {
                item["output"] = json!(output);
            }
            if let Some(code) = non_negative_signed(&result["details"], "exitCode") {
                item["exitCode"] = json!(code);
            }
            events.push(("turn-item.updated", item));
            return events;
        }
        if tool_name == "edit" || tool_name == "write" {
            if let Some(file_name) = string_arg("path").or_else(|| string_arg("file_path")) {
                // edit reports a unified patch in its result details; write only carries the new content in its args.
                // A failed call keeps its error.
                let diff = result["details"]["patch"].as_str().map(str::to_owned).or_else(|| (is_error && !output.trim().is_empty()).then(|| output.clone()));
                item["type"] = json!("file_change");
                item["fileName"] = json!(file_name);
                if let Some(diff) = diff {
                    item["diffStr"] = json!(diff);
                }
                if let Some(content) = (tool_name == "write").then(|| string_arg("content")).flatten() {
                    item["newStr"] = json!(content);
                }
                events.push(("turn-item.updated", item));
                return events;
            }
        }
        item["type"] = json!("dynamic_tool");
        item["toolName"] = json!(tool_name);
        item["input"] = if args.is_null() { json!({}) } else { args.clone() };
        if !output.is_empty() {
            item["output"] = json!(output);
        }
        let presentation = mcp_presentation(&tool_name);
        if let Some(title) = presentation["title"].as_str() {
            item["title"] = json!(title);
        }
        for key in ["toolSource", "toolIcon"] {
            if let Some(value) = presentation.get(key) {
                item[key] = value.clone();
            }
        }
        events.push(("turn-item.updated", item));
        if tool_name == "subagent" {
            events.extend(self.subagents(view, &call_id, result, completed));
        }
        events
    }
    /// The result shape of Pi's official example subagent extension. The extension runs children with
    /// `--no-session`, so these entries show in the shared subagent UI without a child thread. Unknown or
    /// changed result shapes stay ordinary dynamic tool output.
    fn subagents(&mut self, view: &Value, call_id: &str, result: &Value, completed: bool) -> Events {
        let Some(results) = result["details"]["results"].as_array() else { return vec![] };
        let now = at(Utc::now());
        let mut events = vec![];
        for (index, entry) in results.iter().enumerate() {
            let (Some(agent), Some(task)) = (entry["agent"].as_str(), entry["task"].as_str()) else { continue };
            let step = entry["step"].as_f64().map_or(index as u64, |step| step as u64);
            let native = format!("{call_id}:subagent:{step}");
            let started = self.tool_started.entry(native.clone()).or_insert_with(|| now.clone()).clone();
            let finished = completed || entry["finished"] == true;
            let stop_reason = entry["stopReason"].as_str();
            let interrupted = finished && stop_reason == Some("aborted");
            let failed = finished && !interrupted && (entry["exitCode"].as_f64().unwrap_or(0.0) != 0.0 || stop_reason == Some("error"));
            let status = if interrupted { "interrupted" } else if failed { "failed" } else if finished { "completed" } else { "running" };
            let text = subagent_output(entry);
            let progress = (!finished && !text.is_empty()).then(|| text.chars().take(200).collect::<String>());
            let result_text = (finished && !text.is_empty()).then(|| text.chars().take(10_000).collect::<String>());
            let id = node_id(&native);
            let mut row = json!({"id":id,"threadId":self.thread_id,"runId":self.ctx.run_id,"parentNodeId":node_id(call_id),"origin":"provider_native","createdBy":"agent","driver":DRIVER,"providerInstanceId":self.instance_id,"providerThreadId":self.ctx.provider_thread_id,"childThreadId":null,"nativeTaskRef":native_ref(&native),"prompt":task,"title":agent,"model":entry["model"].as_str(),"status":status,"result":result_text,"startedAt":started,"completedAt":finished.then_some(&now),"updatedAt":now});
            if let Some(progress) = &progress {
                row["progress"] = json!(progress);
            }
            let ordinal = self.ordinal(view, &native);
            let mut item = self.base_item(&native, &id, ordinal, status, &started, &now, finished.then_some(now.as_str()));
            item["title"] = json!(agent);
            item["type"] = json!("subagent");
            item["subagentId"] = json!(id);
            item["origin"] = json!("provider_native");
            item["driver"] = json!(DRIVER);
            item["providerInstanceId"] = json!(self.instance_id);
            item["childThreadId"] = Value::Null;
            item["prompt"] = json!(task);
            item["result"] = json!(result_text);
            if let Some(progress) = progress {
                item["progress"] = json!(progress);
            }
            events.push(("subagent.updated", row));
            events.push(("turn-item.updated", item));
        }
        events
    }

    // ---- compaction, retries, notices ----

    pub(crate) fn new_compaction(&mut self) -> Compaction {
        self.compactions += 1;
        Compaction { native: format!("compaction:{}:{}", self.provider_turn, self.compactions), started_at: at(Utc::now()) }
    }
    pub(crate) fn compaction(&mut self, view: &Value, compaction: &Compaction, status: &str, summary: Option<&str>, before: Option<u64>, after: Option<u64>) -> Events {
        let now = at(Utc::now());
        let finished = (status != "running").then_some(now.as_str());
        let ordinal = self.ordinal(view, &compaction.native);
        let node = self.node(&compaction.native, "system", status, &compaction.started_at, finished);
        let mut item = self.base_item(&compaction.native, &node_id(&compaction.native), ordinal, status, &compaction.started_at, &now, finished);
        item["title"] = json!(match status {
            "running" => "Compacting context...",
            "completed" => "Context compacted",
            "failed" => "Context compaction failed",
            _ => "Context compaction stopped",
        });
        item["type"] = json!("compaction");
        item["driver"] = json!(DRIVER);
        if let Some(summary) = summary {
            item["summary"] = json!(summary);
        }
        if let Some(before) = before {
            item["beforeTokenCount"] = json!(before);
        }
        if let Some(after) = after {
            item["afterTokenCount"] = json!(after);
        }
        vec![("node.updated", node), ("turn-item.updated", item)]
    }
    /// The position the turn's terminal-failure item takes, reserved by the first retry notice.
    pub(crate) fn failure_ordinal(&mut self, view: &Value) -> u64 {
        let native = format!("terminal-failure:{}", self.provider_turn);
        self.ordinal(view, &native)
    }
    /// `makeProviderRetryTurnItem`.
    pub(crate) fn retry_item(&self, retry: &Retry, status: &str) -> Events {
        let now = at(Utc::now());
        let title = match status {
            "running" => "Provider retry",
            "completed" => "Provider recovered",
            "failed" => if retry.failure["class"] == "usage_limit" { "Usage limit reached" } else { "Provider error" },
            _ => "Provider retry stopped",
        };
        let mut retry_value = json!({"attempt":retry.attempt,"maxAttempts":retry.max_attempts,"retryDelayMs":retry.delay_ms});
        if retry.delay_ms.is_none() {
            retry_value["retryDelayMs"] = Value::Null;
        }
        let native = format!("terminal-failure:{}", self.provider_turn);
        let mut item = self.base_item(&native, &self.ctx.root_node_id.as_str().unwrap_or_default().to_owned(), retry.ordinal, status, &retry.started_at, &now, (status != "running").then_some(now.as_str()));
        item["nativeItemRef"] = Value::Null;
        item["title"] = json!(title);
        item["type"] = json!("error");
        item["failure"] = retry.failure.clone();
        item["retry"] = retry_value;
        vec![("turn-item.updated", item)]
    }
    /// A `notify` extension dialog: a completed activity item.
    pub(crate) fn notify(&mut self, view: &Value, message: &str, notify_type: &str) -> Events {
        let now = at(Utc::now());
        self.compactions += 1;
        let native = format!("notify:{}:{}", self.provider_turn, self.compactions);
        let ordinal = self.ordinal(view, &native);
        let node = self.node(&native, "system", "completed", &now, Some(&now));
        let mut item = self.base_item(&native, &node_id(&native), ordinal, "completed", &now, &now, Some(&now));
        item["title"] = json!("notify");
        item["type"] = json!("dynamic_tool");
        item["toolName"] = json!("notify");
        item["input"] = json!({"message":message,"notifyType":notify_type});
        vec![("node.updated", node), ("turn-item.updated", item)]
    }
    /// An `extension_error`: a failed notice that does not end the turn.
    pub(crate) fn extension_error(&mut self, view: &Value, title: &str, failure: Value) -> Events {
        let now = at(Utc::now());
        self.compactions += 1;
        let native = format!("extension-error:{}:{}", self.provider_turn, self.compactions);
        let ordinal = self.ordinal(view, &native);
        let node = self.node(&native, "system", "failed", &now, Some(&now));
        let mut item = self.base_item(&native, &node_id(&native), ordinal, "failed", &now, &now, Some(&now));
        item["title"] = json!(title);
        item["type"] = json!("error");
        item["failure"] = failure;
        vec![("node.updated", node), ("turn-item.updated", item)]
    }

    // ---- extension dialogs ----

    /// The node, runtime request and item of an extension dialog the turn waits on.
    pub(crate) fn request(&mut self, view: &Value, native_request: &str, user_input: bool, title: &str, prompt: &str, request_kind: &str, questions: Vec<Value>, session_id: &str) -> Prompt {
        let now = at(Utc::now());
        let request_id = format!("runtime-request:provider:{DRIVER}:provider-turn:{}:native-request:{}", enc(&self.provider_turn), enc(native_request));
        let node_id = format!("node:runtime-request:{}", enc(&request_id));
        let ordinal = self.ordinal(view, native_request);
        let mut node = json!({"id":node_id,"threadId":self.thread_id,"runId":self.ctx.run_id,"parentNodeId":self.ctx.root_node_id,"rootNodeId":self.ctx.root_node_id,"kind":if user_input { "user_input_request" } else { "approval_request" },"status":"waiting","countsForRun":false,"providerThreadId":self.ctx.provider_thread_id,"providerTurnId":self.provider_turn,"nativeItemRef":native_ref(native_request),"runtimeRequestId":request_id,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        node["rootNodeId"] = self.ctx.root_node_id.clone();
        let request = json!({"id":request_id,"nodeId":node_id,"providerTurnId":self.provider_turn,"nativeRequestRef":native_ref(native_request),"kind":if user_input { "user_input" } else { request_kind },"status":"pending","responseCapability":{"type":"live","providerSessionId":session_id},"createdAt":now,"resolvedAt":null});
        let mut item = self.base_item(native_request, &node_id, ordinal, "waiting", &now, &now, None);
        item["id"] = json!(format!("turn-item:runtime-request:{}", enc(&request_id)));
        item["title"] = json!(title);
        item["requestId"] = json!(request_id);
        if user_input {
            item["type"] = json!("user_input_request");
            item["questions"] = json!(questions);
        } else {
            item["type"] = json!("approval_request");
            item["requestKind"] = json!(approval_kind(title));
            item["prompt"] = json!(prompt);
        }
        let events = vec![("node.updated", node.clone()), ("runtime-request.updated", request.clone()), ("turn-item.updated", item.clone())];
        Prompt { request_id, request, node, item, events }
    }
    /// Whether a native item already has its position, so an update to it needs no read of the projection.
    pub(crate) fn knows(&self, native: &str) -> bool {
        self.ordinals.contains_key(native)
    }
    pub(crate) fn has_stream(&self, kind: StreamKind, content_index: u64) -> bool {
        let _ = kind;
        self.stream.contains_key(&self.stream_key(content_index))
    }
}

/// An extension dialog as persisted, and the events that wrote it.
pub(crate) struct Prompt {
    pub request_id: String,
    pub request: Value,
    pub node: Value,
    pub item: Value,
    pub events: Events,
}
impl Prompt {
    /// The dialog ended without the user's answer (the turn ended, or it was withdrawn).
    pub(crate) fn cancelled(&self, resolved_at: &str) -> Events {
        let (mut request, mut node, mut item) = (self.request.clone(), self.node.clone(), self.item.clone());
        request["status"] = json!("cancelled");
        request["resolvedAt"] = json!(resolved_at);
        request["responseCapability"] = json!({"type":"not_resumable","reason":"The Pi extension request was cancelled."});
        node["status"] = json!("cancelled");
        node["completedAt"] = json!(resolved_at);
        item["status"] = json!("cancelled");
        item["completedAt"] = json!(resolved_at);
        item["updatedAt"] = json!(resolved_at);
        vec![("runtime-request.updated", request), ("node.updated", node), ("turn-item.updated", item)]
    }
}

/// `piApprovalRequestKind`: the T3 bridge confirms tool calls as `Allow <tool>?`. Edits surface as file-change
/// approvals so clients render them like other providers' edits; every other confirmation, including ones from
/// user extensions, is a command.
pub(crate) fn approval_kind(title: &str) -> &'static str {
    let tool = title.strip_prefix("Allow ").and_then(|rest| rest.strip_suffix('?')).filter(|tool| !tool.is_empty() && !tool.contains(char::is_whitespace));
    if tool.is_some_and(|tool| pi_launch::FILE_CHANGE_TOOLS.contains(&tool)) { "file-change" } else { "command" }
}

/// `piQuestion`: a dialog as a question. A `select` offers its options; the others take free text, which may be empty.
pub(crate) fn question(question_id: &str, method: &str, title: &str, event: &Value) -> Value {
    let options: Vec<Value> = if method == "select" && event["options"].is_array() {
        event["options"].as_array().unwrap().iter().filter_map(Value::as_str).map(|option| json!({"label":if option.is_empty() { "Empty value" } else { option },"description":option,"value":option})).collect()
    } else {
        vec![json!({"label":"Submit empty value","description":"Send an empty string to the extension.","value":""})]
    };
    // The user-input contract has no prefill field, so an editor dialog's prefill is shown inside the question text.
    let prefill = (method == "editor").then(|| event["prefill"].as_str()).flatten().filter(|prefill| !prefill.is_empty());
    let text = event["message"].as_str().or_else(|| event["placeholder"].as_str()).unwrap_or(title);
    let text = match prefill {
        Some(prefill) => format!("{text}\n\nCurrent value:\n{}", prefill.chars().take(2000).collect::<String>()),
        None => text.to_owned(),
    };
    json!({"id":question_id,"header":title,"question":text,"options":options})
}
/// `piUiResponse` over the persisted answer: what the extension receives.
pub(crate) fn ui_response(method: &str, question_id: &str, decision: Option<&str>, answers: &Value) -> Value {
    if method == "confirm" {
        return match decision {
            Some("accept" | "acceptForSession") => json!({"confirmed":true}),
            Some("decline") => json!({"confirmed":false}),
            _ => json!({"cancelled":true}),
        };
    }
    // An empty string is a valid dialog value (the extension receives ""), distinct from cancelling.
    match answers.get(question_id) {
        Some(Value::String(value)) => json!({"value":value}),
        Some(Value::Array(values)) if values.len() == 1 && values[0].is_string() => json!({"value":values[0]}),
        _ => json!({"cancelled":true}),
    }
}

/// `piSubagentOutput`: the last assistant text of a subagent-extension task's transcript, or its error when it failed.
fn subagent_output(result: &Value) -> String {
    let stop_reason = result["stopReason"].as_str();
    let failed = result["exitCode"].as_f64().unwrap_or(0.0) != 0.0 || stop_reason == Some("error") || stop_reason == Some("aborted");
    if failed {
        // Falsy fallback, not `??`: an empty `errorMessage` must not suppress a non-empty `stderr`.
        let failure = result["errorMessage"].as_str().filter(|text| !text.is_empty()).or_else(|| result["stderr"].as_str());
        if let Some(failure) = failure.filter(|text| !text.is_empty()) {
            return failure.to_owned();
        }
    }
    for message in result["messages"].as_array().into_iter().flatten().rev() {
        if message["role"] == "assistant" {
            let text = content_text(&message["content"]);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}
/// `piExtensionDisplayName`.
pub(crate) fn extension_display_name(path: Option<&str>) -> String {
    let Some(path) = path else { return "Pi extension".into() };
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let file = normalized.rsplit('/').next().unwrap_or("");
    let name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if name.is_empty() { "Pi extension".into() } else { name.into() }
}
fn non_negative_signed(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_f64).filter(|number| number.is_finite()).map(|number| number as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn projector() -> (Projector, Value) {
        let view = json!({"runs":[{"id":"run"}],"turnItems":[{"ordinal":4}]});
        let ctx = TurnContext { run_id: "run".into(), attempt_id: json!("attempt"), root_node_id: json!("root"), provider_thread_id: json!("provider-thread"), provider_turn_id: json!("turn"), provider_instance_id: json!("pi") };
        (Projector::new("thread", ctx, &view, "turn").unwrap(), view)
    }
    /// What the adapter commits is checked against the contracts when it is written; so is it here, for every kind of item.
    fn valid(events: &Events) {
        for (kind, payload) in events {
            t3_contracts::normalize_event_payload(kind, payload.clone()).unwrap_or_else(|error| panic!("{kind}: {error}\n{payload}"));
        }
    }
    #[test]
    fn every_kind_of_item_the_projector_writes_fits_the_contracts_and_follows_the_originals_normalisation() {
        let (mut projector, view) = projector();
        // Streamed text and reasoning take consecutive positions after what the thread holds, and finish with the text Pi ended on.
        projector.next_message();
        projector.delta(&view, StreamKind::Reasoning, 0, "think");
        projector.delta(&view, StreamKind::Text, 1, "tex");
        assert!(projector.has_pending());
        let flushed = projector.flush(&Value::Null);
        valid(&flushed);
        let streaming: Vec<&Value> = flushed.iter().filter(|(kind, _)| *kind == "turn-item.updated").map(|(_, item)| item).collect();
        assert_eq!(streaming.iter().map(|item| (item["type"].as_str().unwrap(), item["ordinal"].as_u64().unwrap(), item["streaming"].as_bool().unwrap())).collect::<Vec<_>>(), [("reasoning", 5, true), ("assistant_message", 6, true)]);
        assert!(!projector.has_pending());
        let ended = projector.end(&Value::Null, StreamKind::Text, 1, Some("text"));
        valid(&ended);
        assert_eq!((ended[1].1["text"].clone(), ended[1].1["status"].clone(), ended[2].1["text"].clone()), (json!("text"), json!("completed"), json!("text")));
        assert!(projector.end(&Value::Null, StreamKind::Text, 1, Some("again")).is_empty(), "an item ends once");
        // An empty item is never written.
        projector.next_message();
        projector.delta(&view, StreamKind::Text, 0, "");
        assert!(projector.end(&view, StreamKind::Text, 0, None).is_empty());

        // Tools: a command, an edit with its patch, a write with its content, an MCP tool with its source, and an aborted one.
        let bash = projector.tool(&view, &json!({"toolCallId":"c1","toolName":"bash","args":{"command":"ls"}}), "start", false);
        valid(&bash);
        let done = projector.tool(&view, &json!({"toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"a\nb"}],"details":{"exitCode":0}},"isError":false}), "end", false);
        valid(&done);
        assert_eq!((done[1].1["type"].clone(), done[1].1["input"].clone(), done[1].1["output"].clone(), done[1].1["exitCode"].clone(), done[1].1["status"].clone()), (json!("command_execution"), json!("ls"), json!("a\nb"), json!(0), json!("completed")));
        let edit = projector.tool(&view, &json!({"toolCallId":"c2","toolName":"edit","args":{"path":"a.txt"},"result":{"content":[],"details":{"patch":"--- a\n+++ b"}},"isError":false}), "end", false);
        valid(&edit);
        assert_eq!((edit[1].1["type"].clone(), edit[1].1["fileName"].clone(), edit[1].1["diffStr"].clone()), (json!("file_change"), json!("a.txt"), json!("--- a\n+++ b")));
        let write = projector.tool(&view, &json!({"toolCallId":"c3","toolName":"write","args":{"file_path":"b.txt","content":"new"},"result":{"content":[{"type":"text","text":"denied"}]},"isError":true}), "end", false);
        valid(&write);
        assert_eq!((write[1].1["newStr"].clone(), write[1].1["diffStr"].clone(), write[1].1["status"].clone()), (json!("new"), json!("denied"), json!("failed")));
        let mcp = projector.tool(&view, &json!({"toolCallId":"c4","toolName":"mcp__github__search_issues","args":{"q":"x"},"result":{"content":[{"type":"text","text":"[]"}]},"isError":false}), "end", false);
        valid(&mcp);
        assert_eq!((mcp[1].1["title"].clone(), mcp[1].1["toolSource"]["key"].clone(), mcp[1].1["input"].clone()), (json!("search issues"), json!("mcp:github"), json!({"q":"x"})));
        let plain = projector.tool(&view, &json!({"toolCallId":"c5","toolName":"read"}), "start", false);
        valid(&plain);
        assert_eq!((plain[1].1["toolName"].clone(), plain[1].1["title"].clone(), plain[1].1["input"].clone()), (json!("read"), json!("read"), json!({})));
        let aborted = projector.tool(&view, &json!({"toolCallId":"c6","toolName":"bash","result":{"content":[{"type":"text","text":"Command aborted"}]},"isError":true}), "end", true);
        assert_eq!(aborted[1].1["status"], "interrupted");
        assert!(projector.tool(&view, &json!({"toolName":"bash"}), "start", false).is_empty(), "a tool without an id is not an item");

        // Compaction in each state, a provider retry through its stages, a notice and an extension failure.
        let compaction = projector.new_compaction();
        for status in ["running", "completed", "failed", "cancelled"] {
            valid(&projector.compaction(&view, &compaction, status, Some("summary"), Some(10), Some(5)));
        }
        let compacted = projector.compaction(&view, &compaction, "completed", Some("summary"), Some(10), Some(5));
        assert_eq!((compacted[1].1["title"].clone(), compacted[1].1["summary"].clone(), compacted[1].1["beforeTokenCount"].clone(), compacted[1].1["afterTokenCount"].clone()), (json!("Context compacted"), json!("summary"), json!(10), json!(5)));
        let failure = crate::provider_failure::make(&json!({"message":"overloaded","class":"provider_error","retryable":true}));
        let retry = Retry { attempt: 1, max_attempts: 3, delay_ms: Some(50), failure, started_at: at(Utc::now()), ordinal: projector.failure_ordinal(&view) };
        for (status, title) in [("running", "Provider retry"), ("completed", "Provider recovered"), ("failed", "Provider error"), ("interrupted", "Provider retry stopped")] {
            let events = projector.retry_item(&retry, status);
            valid(&events);
            assert_eq!((events[0].1["title"].clone(), events[0].1["retry"]["maxAttempts"].clone()), (json!(title), json!(3)));
        }
        valid(&projector.notify(&view, "heads up", "warning"));
        valid(&projector.extension_error(&view, "guard", crate::provider_failure::make(&json!({"message":"guard failed.","class":"provider_error","retryable":false}))));

        // Dialogs: an approval and a question, and the cancelled form of each.
        let approval = projector.request(&view, "dialog-1", false, "Allow edit?", "edit a.txt", "command", vec![], "session");
        valid(&approval.events);
        assert_eq!((approval.item["requestKind"].clone(), approval.request["kind"].clone(), approval.item["status"].clone()), (json!("file-change"), json!("command"), json!("waiting")));
        let question = projector.request(&view, "dialog-2", true, "Pick", "Pick", "command", vec![question("dialog-2", "select", "Pick", &json!({"options":["a"]}))], "session");
        valid(&question.events);
        assert_eq!(question.request["kind"], "user_input");
        valid(&approval.cancelled("2026-01-01T00:00:00.000Z"));
        assert_eq!(question.cancelled("2026-01-01T00:00:00.000Z")[0].1["status"], "cancelled");
    }
    #[test]
    fn a_subagent_extension_result_becomes_a_provider_native_task_without_a_child_thread() {
        let (mut projector, view) = projector();
        let running = json!({"toolCallId":"call","toolName":"subagent","args":{},"partialResult":{"details":{"results":[
            {"agent":"worker","task":"survey","step":0,"messages":[{"role":"assistant","content":[{"type":"text","text":"looking"}]}]},
            {"agent":"critic","task":"review","step":1,"exitCode":1,"stderr":"boom","finished":true},
            {"agent":"stopped","task":"wait","finished":true,"stopReason":"aborted"},
            {"task":"nameless"}]}}});
        let events = projector.tool(&view, &running, "update", false);
        valid(&events);
        let tasks: Vec<&Value> = events.iter().filter(|(kind, _)| *kind == "subagent.updated").map(|(_, task)| task).collect();
        assert_eq!(tasks.iter().map(|task| (task["title"].as_str().unwrap(), task["status"].as_str().unwrap(), task["progress"].as_str(), task["result"].as_str())).collect::<Vec<_>>(), [("worker", "running", Some("looking"), None), ("critic", "failed", None, Some("boom")), ("stopped", "interrupted", None, None)]);
        assert!(tasks.iter().all(|task| task["origin"] == "provider_native" && task["childThreadId"].is_null() && task["driver"] == "pi"));
        let ended = projector.tool(&view, &json!({"toolCallId":"call","toolName":"subagent","result":{"details":{"results":[{"agent":"worker","task":"survey","step":0,"exitCode":0,"stopReason":"stop","messages":[{"role":"assistant","content":[{"type":"text","text":"done"}]}]}]}}}), "end", false);
        valid(&ended);
        let finished = ended.iter().find(|(kind, _)| *kind == "subagent.updated").unwrap();
        assert_eq!((finished.1["status"].clone(), finished.1["result"].clone(), finished.1["id"].clone()), (json!("completed"), json!("done"), tasks[0]["id"].clone()), "the same task, finished");
    }
    #[test]
    fn approvals_and_questions_take_the_shapes_the_original_gives_them() {
        assert_eq!((approval_kind("Allow edit?"), approval_kind("Allow write?"), approval_kind("Allow bash?"), approval_kind("Delete everything?"), approval_kind("Allow edit file?")), ("file-change", "file-change", "command", "command", "command"));
        let select = question("q1", "select", "Pick", &json!({"message":"Which one?","options":["a","","b",5]}));
        assert_eq!(select, json!({"id":"q1","header":"Pick","question":"Which one?","options":[{"label":"a","description":"a","value":"a"},{"label":"Empty value","description":"","value":""},{"label":"b","description":"b","value":"b"}]}));
        let editor = question("q2", "editor", "Edit", &json!({"prefill":"draft text"}));
        assert_eq!(editor["question"], "Edit\n\nCurrent value:\ndraft text");
        assert_eq!(editor["options"][0]["label"], "Submit empty value");
        let input = question("q3", "input", "Name", &json!({"placeholder":"your name"}));
        assert_eq!(input["question"], "your name");
        assert_eq!(ui_response("confirm", "x", Some("accept"), &json!({})), json!({"confirmed":true}));
        assert_eq!(ui_response("confirm", "x", Some("acceptForSession"), &json!({})), json!({"confirmed":true}));
        assert_eq!(ui_response("confirm", "x", Some("decline"), &json!({})), json!({"confirmed":false}));
        assert_eq!(ui_response("confirm", "x", Some("cancel"), &json!({})), json!({"cancelled":true}));
        assert_eq!(ui_response("select", "q1", None, &json!({"q1":"a"})), json!({"value":"a"}));
        assert_eq!(ui_response("input", "q1", None, &json!({"q1":""})), json!({"value":""}));
        assert_eq!(ui_response("input", "q1", None, &json!({"other":"a"})), json!({"cancelled":true}));
    }
    #[test]
    fn extension_names_and_subagent_output_follow_the_original() {
        assert_eq!(extension_display_name(None), "Pi extension");
        assert_eq!(extension_display_name(Some("/home/u/.pi/ext/guard.ts")), "guard");
        assert_eq!(extension_display_name(Some("C:\\ext\\a.b.ts\\")), "a.b");
        assert_eq!(extension_display_name(Some("/")), "Pi extension");
        let ok = json!({"messages":[{"role":"assistant","content":[{"type":"text","text":"first"}]},{"role":"user","content":"x"},{"role":"assistant","content":[{"type":"text","text":"last"}]}]});
        assert_eq!(subagent_output(&ok), "last");
        assert_eq!(subagent_output(&json!({"exitCode":1,"errorMessage":"","stderr":"boom"})), "boom");
        assert_eq!(subagent_output(&json!({"stopReason":"error","errorMessage":"bad","stderr":"boom"})), "bad");
    }
}
