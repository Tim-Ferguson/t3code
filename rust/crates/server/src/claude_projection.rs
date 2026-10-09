//! Claude SDK messages to persisted orchestration events. One [`Projector`] lives for one provider
//! turn. It is pure with respect to I/O: the adapter hands it each message and the current
//! projection and commits the events it returns. Mirrors the message handling of
//! `ClaudeAdapterV2.ts` (assistant text, reasoning, tool calls, plans, retries, results).
use crate::{claude_models, execution::at, persistence::StoreError, provider_turn_context::TurnContext};
use chrono::Utc;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub(crate) type Events = Vec<(&'static str, Value)>;
const DRIVER: &str = "claudeAgent";
const WEB_FETCH_SNIPPET_MAX_CHARS: usize = 8_000;

fn err(message: impl Into<String>) -> StoreError {
    StoreError::InvalidCommand(message.into())
}
/// `encodeURIComponent`, which the source id allocator applies to every id part.
pub(crate) fn enc(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
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

/// Normalized tool name (`classifyClaudeNativeTool`).
pub(crate) struct Classification {
    pub normalized: String,
    pub item_type: &'static str,
    pub request_kind: &'static str,
}
pub(crate) fn classify(tool_name: &str) -> Classification {
    let normalized: String = tool_name.to_lowercase().chars().filter(|c| !(c.is_whitespace() || *c == '_' || *c == '-')).collect();
    let (item_type, request_kind) = match normalized.as_str() {
        "bash" => ("command_execution", "command"),
        "edit" | "multiedit" | "notebookedit" | "write" => ("file_change", "file-change"),
        "glob" | "grep" | "ls" | "read" => ("dynamic_tool", "file-read"),
        "webfetch" | "websearch" => ("web_search", "command"),
        _ => ("dynamic_tool", "command"),
    };
    Classification { normalized, item_type, request_kind }
}

fn first_string(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| input.get(*key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned))
}
pub(crate) fn js_stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
/// `summarizeClaudeToolRequest`: the prompt of an approval without a native title.
pub(crate) fn summarize_request(tool_name: &str, input: &Value) -> String {
    if let Some(command) = first_string(input, &["command", "cmd", "script"]) {
        return format!("{tool_name}: {}", truncate_chars(&command, 400));
    }
    if let Some(path) = first_string(input, &["file_path", "path", "filename", "fileName"]) {
        return format!("{tool_name}: {}", truncate_chars(&path, 400));
    }
    let serialized = js_stringify(input);
    if serialized.chars().count() <= 400 { format!("{tool_name}: {serialized}") } else { format!("{tool_name}: {}...", truncate_chars(&serialized, 397)) }
}
/// `claudeTodoSteps`
pub(crate) fn todo_steps(input: &Value) -> Vec<Value> {
    input["todos"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, todo)| {
            let text = todo["content"].as_str()?.trim();
            (!text.is_empty()).then(|| {
                let status = match todo["status"].as_str() {
                    Some("completed") => "completed",
                    Some("in_progress") => "running",
                    _ => "pending",
                };
                json!({"id":format!("todo-{index}"),"text":text,"status":status})
            })
        })
        .collect()
}
/// `claudeProposedPlan`
pub(crate) fn proposed_plan(input: &Value) -> Option<String> {
    input["plan"].as_str().map(str::trim).filter(|plan| !plan.is_empty()).map(str::to_owned)
}
/// `claudeUserInputQuestions`
pub(crate) fn user_input_questions(input: &Value) -> Vec<Value> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, record)| {
            let question = record["question"].as_str()?.trim();
            if question.is_empty() {
                return None;
            }
            let header = record["header"].as_str().map(str::trim).filter(|header| !header.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("Question {}", index + 1));
            let options: Vec<Value> = record["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option["label"].as_str()?.trim();
                    (!label.is_empty()).then(|| json!({"label":label,"description":option["description"].as_str().map(str::trim).unwrap_or("")}))
                })
                .collect();
            Some(json!({"id":question,"header":header,"question":question,"options":options,"multiSelect":record["multiSelect"] == true}))
        })
        .collect()
}
/// `claudeSdkUserInputAnswers`: answers keyed by question text, multiple selections joined.
pub(crate) fn sdk_user_input_answers(answers: &Value) -> Value {
    let mut out = Map::new();
    for (question, answer) in answers.as_object().into_iter().flatten() {
        let text = match answer {
            Value::Array(values) => values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        out.insert(question.clone(), json!(text));
    }
    Value::Object(out)
}

/// `normalizeMcpText`
fn mcp_text(value: Option<&str>, max: usize) -> Option<String> {
    let text = value?.trim().split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.chars().count() <= max).then_some(text)
}
fn t3_mcp_tool(server: &str, tool: &str) -> bool {
    static NAMES: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(|| serde_json::from_str::<Vec<String>>(include_str!("claude-t3-mcp-tools.json")).expect("generated T3 MCP tool names").into_iter().collect());
    matches!(server.to_lowercase().as_str(), "t3-code" | "t3_code" | "t3code") && names.contains(tool)
}
/// `mcpToolPresentation` for a tool name without server-provided metadata.
pub(crate) fn mcp_presentation(tool_name: &str) -> Value {
    let (server, tool) = match tool_name.strip_prefix("mcp__").or_else(|| tool_name.strip_prefix("MCP__")) {
        Some(rest) => match rest.find("__") {
            Some(split) if split > 0 && split + 2 < rest.len() => (mcp_text(Some(&rest[..split]), 160), mcp_text(Some(&rest[split + 2..]), 160)),
            _ => (None, mcp_text(Some(tool_name), 160)),
        },
        None => (None, mcp_text(Some(tool_name), 160)),
    };
    let squash = |text: &str| mcp_text(Some(&text.split(['_', '-']).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ")), 160);
    if let (Some(server), Some(tool)) = (&server, &tool) {
        if t3_mcp_tool(server, tool) {
            return json!({});
        }
    }
    let title = match (&server, &tool) {
        (Some(_), Some(tool)) => squash(tool),
        _ => None,
    };
    let Some(server) = server else { return title.map_or(json!({}), |title| json!({"title":title})) };
    let name = squash(&server).unwrap_or_else(|| server.clone());
    let mut presentation = json!({"toolSource":{"key":format!("mcp:{}", server.to_lowercase()),"name":name,"kind":"integration"}});
    if let Some(title) = title {
        presentation["title"] = json!(title);
    }
    presentation
}

/// A tool call from `tool_use` until its result.
#[derive(Clone)]
pub(crate) struct ToolCall {
    pub name: String,
    pub input: Value,
    pub ordinal: u64,
    pub started_at: String,
    pub presentation: Value,
}
struct ReasoningBlock {
    started_at: String,
    ordinal: u64,
    text: String,
    dirty: bool,
    completed: bool,
}
/// What `result` messages make of a turn.
pub(crate) struct Terminal {
    pub status: &'static str,
    pub failure: Option<Value>,
}

pub(crate) struct Projector {
    thread_id: String,
    run_id: String,
    ctx: TurnContext,
    selection: Value,
    turn_native_id: String,
    tool_calls: indexmap::IndexMap<String, ToolCall>,
    emitted_text: HashSet<String>,
    fallback_text: String,
    fallback_id: String,
    ordinals: HashMap<String, u64>,
    next_ordinal: Option<u64>,
    reasoning: HashMap<String, ReasoningBlock>,
    stream_blocks: HashMap<u64, String>,
    stream_message: Option<String>,
    next_block_index: HashMap<String, u64>,
    snapshot_block_index: HashMap<String, u64>,
    snapshots: HashSet<String>,
    plan_ids: HashMap<String, String>,
    latest_plan: HashMap<&'static str, Value>,
    pub(crate) cursor: Option<String>,
    started_at: String,
    retry: Option<(Value, Value, String, u64)>,
    auth_failed: bool,
    pub(crate) permission_mode: Option<String>,
    /// Windows the CLI reported as rejected this turn, with the reset time each one named.
    rejected_limits: Vec<(String, Option<String>)>,
    announced_limits: HashSet<String>,
    assistant_rate_limited: bool,
    /// Bound to a subagent's child thread: no run, no provider thread, its own item positions.
    child: bool,
    /// Tool calls the adapter owns as subagents (`Agent`): they project as subagent rows, never as tools.
    pub(crate) skip_tools: HashSet<String>,
    /// A subagent registered or reported under this turn: its usage does not cover them.
    has_subagents: bool,
    /// The turn ingests output the CLI produced while the root was idle (a wake), not an answer to a prompt.
    pub(crate) continuation: bool,
    /// `Agent` calls seen since the adapter last took them: the call id and its input.
    launches: Vec<(String, Value)>,
}

impl Projector {
    /// A projector for a subagent's child thread, whose rows have no run and no provider turn.
    pub(crate) fn for_child(thread_id: &str, child_root_node_id: &str, instance_id: &Value, selection: Value) -> Self {
        let ctx = TurnContext { run_id: String::new(), attempt_id: Value::Null, root_node_id: json!(child_root_node_id), provider_thread_id: Value::Null, provider_turn_id: Value::Null, provider_instance_id: instance_id.clone() };
        let mut projector = Self::blank(thread_id, ctx, selection);
        projector.child = true;
        projector
    }
    /// The position a new item of a child thread takes: after everything the thread holds. An item
    /// already written keeps its own.
    pub(crate) fn arrival(view: &Value, item_id: &str) -> u64 {
        let items = view["turnItems"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        items.iter().find(|item| item["id"] == item_id).and_then(|item| item["ordinal"].as_u64()).unwrap_or_else(|| items.iter().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1))
    }
    /// The next position of the turn's items, for a row an adapter adds beside the projector's own.
    pub(crate) fn position(&mut self, view: &Value, native: &str) -> u64 {
        self.ordinal(view, native)
    }
    fn blank(thread_id: &str, ctx: TurnContext, selection: Value) -> Self {
        Self {
            thread_id: thread_id.into(),
            fallback_id: format!("assistant:{}", ctx.run_id),
            turn_native_id: format!("turn:{}", ctx.attempt_id.as_str().unwrap_or("")),
            run_id: ctx.run_id.clone(),
            ctx,
            selection,
            tool_calls: Default::default(),
            emitted_text: Default::default(),
            fallback_text: String::new(),
            ordinals: Default::default(),
            next_ordinal: None,
            reasoning: Default::default(),
            stream_blocks: Default::default(),
            stream_message: None,
            next_block_index: Default::default(),
            snapshot_block_index: Default::default(),
            snapshots: Default::default(),
            plan_ids: Default::default(),
            latest_plan: Default::default(),
            cursor: None,
            started_at: at(Utc::now()),
            retry: None,
            auth_failed: false,
            permission_mode: None,
            rejected_limits: vec![],
            announced_limits: Default::default(),
            assistant_rate_limited: false,
            child: false,
            skip_tools: Default::default(),
            has_subagents: false,
            continuation: false,
            launches: vec![],
        }
    }
    pub(crate) fn has_open_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
    pub(crate) fn has_tool(&self, id: &str) -> bool {
        self.tool_calls.contains_key(id)
    }
    /// Ends every call still open, as a stopped turn or subagent leaves them.
    pub(crate) fn end_tools(&mut self, view: &Value, status: &str) -> Result<Events, StoreError> {
        let provider_turn = self.provider_turn(view)?;
        let open: Vec<(String, ToolCall)> = self.tool_calls.drain(..).collect();
        let now = at(Utc::now());
        Ok(open.into_iter().flat_map(|(id, call)| self.tool_artifacts(&id, &call, None, status, &now, None, &provider_turn)).collect())
    }
    pub(crate) fn mark_subagents(&mut self) {
        self.has_subagents = true;
    }
    pub(crate) fn take_launches(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.launches)
    }
    pub(crate) fn new(thread_id: &str, ctx: TurnContext, view: &Value, selection: Value) -> Result<Self, StoreError> {
        crate::codex_runtime::find(view, "runs", &json!(ctx.run_id))?;
        Ok(Self::blank(thread_id, ctx, selection))
    }
    pub(crate) fn selection(&self) -> &Value {
        &self.selection
    }
    pub(crate) fn context(&self) -> &TurnContext {
        &self.ctx
    }
    /// Items are ordered by arrival: each native item takes the next ordinal once.
    fn ordinal(&mut self, view: &Value, native: &str) -> u64 {
        if let Some(existing) = self.ordinals.get(native) {
            return *existing;
        }
        let from_view = || view["turnItems"].as_array().into_iter().flatten().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1);
        // A subagent's thread has several writers, so each new item takes the position after what the thread holds.
        if self.child {
            let next = from_view();
            self.ordinals.insert(native.to_owned(), next);
            return next;
        }
        let next = self.next_ordinal.unwrap_or_else(from_view);
        self.next_ordinal = Some(next + 1);
        self.ordinals.insert(native.to_owned(), next);
        next
    }
    fn root_node(&self) -> &Value {
        &self.ctx.root_node_id
    }
    fn base_item(&self, id: &str, node: &Value, ordinal: u64, status: &str, title: Value, native: Option<&str>, started: &str, completed: Option<&str>, updated: &str, provider_turn: &Value) -> Value {
        json!({"id":id,"threadId":self.thread_id,"runId":self.run(),"nodeId":node,"providerThreadId":self.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":native.map(native_ref),"parentItemId":null,"ordinal":ordinal,"status":status,"title":title,"startedAt":started,"completedAt":completed,"updatedAt":updated})
    }
    fn node(&self, id: &str, kind: &str, status: &str, native: Option<&str>, parent: &Value, started: &str, completed: Option<&str>, provider_turn: &Value) -> Value {
        json!({"id":id,"threadId":self.thread_id,"runId":self.run(),"parentNodeId":parent,"rootNodeId":self.ctx.root_node_id,"kind":kind,"status":status,"countsForRun":false,"providerThreadId":self.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":native.map(native_ref),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":completed})
    }
    fn provider_turn(&self, view: &Value) -> Result<Value, StoreError> {
        if self.child {
            return Ok(Value::Null);
        }
        let (_, attempt) = self.ctx.rows(view)?;
        Ok(attempt["providerTurnId"].clone())
    }
    /// What rows of this projector name as their run: none in a subagent's thread.
    fn run(&self) -> Value {
        if self.child { Value::Null } else { json!(self.run_id) }
    }
    fn provider_thread(&self) -> Value {
        if self.child { Value::Null } else { self.ctx.provider_thread_id.clone() }
    }

    /// Handle one message from the CLI; the events describe how the projection changes.
    pub(crate) fn plan(&mut self, view: &Value, message: &Value) -> Result<Events, StoreError> {
        if !message["parent_tool_use_id"].is_null() {
            // Subagent frames belong to a child thread this port does not project.
            return Ok(vec![]);
        }
        match message["type"].as_str() {
            Some("stream_event") => self.stream_event(view, &message["event"]),
            Some("assistant") => self.assistant(view, message),
            Some("user") => self.user(view, message),
            Some("system") => self.system(view, message),
            _ => Ok(vec![]),
        }
    }

    fn stream_event(&mut self, view: &Value, event: &Value) -> Result<Events, StoreError> {
        match event["type"].as_str() {
            Some("message_start") => {
                self.stream_message = event["message"]["id"].as_str().map(str::to_owned);
                self.stream_blocks.clear();
            }
            Some("content_block_start") if event["content_block"]["type"] == "thinking" => {
                let Some(message) = self.stream_message.clone() else { return Ok(vec![]) };
                let index = self.next_block_index.entry(message.clone()).or_insert(0);
                let id = format!("{message}:thinking:{index}");
                *index += 1;
                self.stream_blocks.insert(event["index"].as_u64().unwrap_or(0), id.clone());
                self.ensure_reasoning(view, &id);
                self.append_reasoning(&id, event["content_block"]["thinking"].as_str().unwrap_or(""));
            }
            Some("content_block_delta") if event["delta"]["type"] == "thinking_delta" => {
                if let Some(id) = self.stream_blocks.get(&event["index"].as_u64().unwrap_or(0)).cloned() {
                    self.append_reasoning(&id, event["delta"]["thinking"].as_str().unwrap_or(""));
                }
            }
            Some("content_block_stop") => {
                if let Some(id) = self.stream_blocks.remove(&event["index"].as_u64().unwrap_or(0)) {
                    return self.complete_reasoning(view, &id, None);
                }
            }
            _ => {}
        }
        Ok(vec![])
    }
    fn ensure_reasoning(&mut self, view: &Value, id: &str) {
        if !self.reasoning.contains_key(id) {
            let ordinal = self.ordinal(view, id);
            self.reasoning.insert(id.to_owned(), ReasoningBlock { started_at: at(Utc::now()), ordinal, text: String::new(), dirty: false, completed: false });
        }
    }
    fn append_reasoning(&mut self, id: &str, delta: &str) {
        if let Some(block) = self.reasoning.get_mut(id) {
            block.text.push_str(delta);
            block.dirty = true;
        }
    }
    /// Whether streamed reasoning waits to be written (the adapter flushes on a short timer).
    pub(crate) fn has_pending_reasoning(&self) -> bool {
        self.reasoning.values().any(|block| block.dirty && !block.completed)
    }
    pub(crate) fn flush_reasoning(&mut self, view: &Value) -> Result<Events, StoreError> {
        let ids: Vec<String> = self.reasoning.iter().filter(|(_, block)| block.dirty && !block.completed).map(|(id, _)| id.clone()).collect();
        let mut events = vec![];
        for id in ids {
            events.extend(self.reasoning_item(view, &id, false)?);
        }
        Ok(events)
    }
    fn reasoning_item(&mut self, view: &Value, id: &str, completed: bool) -> Result<Events, StoreError> {
        let Some(block) = self.reasoning.get_mut(id) else { return Ok(vec![]) };
        block.dirty = false;
        if block.text.is_empty() {
            return Ok(vec![]);
        }
        block.completed |= completed;
        let (block_text, ordinal, started) = (block.text.clone(), block.ordinal, block.started_at.clone());
        let now = at(Utc::now());
        let provider_turn = self.provider_turn(view)?;
        let mut item = self.base_item(&item_id(id), &self.root_node().clone(), ordinal, if completed { "completed" } else { "running" }, json!("Thinking"), Some(id), &started, completed.then_some(now.as_str()), &now, &provider_turn);
        item["type"] = json!("reasoning");
        item["text"] = json!(block_text);
        item["streaming"] = json!(!completed);
        Ok(vec![("turn-item.updated", item)])
    }
    fn complete_reasoning(&mut self, view: &Value, id: &str, final_text: Option<&str>) -> Result<Events, StoreError> {
        self.ensure_reasoning(view, id);
        if let (Some(text), Some(block)) = (final_text.filter(|text| !text.is_empty()), self.reasoning.get_mut(id)) {
            block.text = text.to_owned();
        }
        self.reasoning_item(view, id, true)
    }

    fn assistant(&mut self, view: &Value, message: &Value) -> Result<Events, StoreError> {
        let mut events = vec![];
        let uuid = message["uuid"].as_str().unwrap_or("");
        let content = message["message"]["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if self.snapshots.insert(uuid.to_owned()) {
            let message_native = message["message"]["id"].as_str().unwrap_or("").to_owned();
            for block in content.iter().filter(|block| block["type"] == "thinking") {
                let index = self.snapshot_block_index.entry(message_native.clone()).or_insert(0);
                let id = format!("{message_native}:thinking:{index}");
                *index += 1;
                let final_text = block["thinking"].as_str().filter(|text| !text.is_empty());
                events.extend(self.complete_reasoning(view, &id, final_text)?);
            }
        }
        self.cursor = Some(uuid.to_owned());
        self.assistant_rate_limited = message["error"] == "rate_limit";
        if message["error"] == "authentication_failed" {
            self.auth_failed = true;
        }
        if let Some(retry) = self.retry.take() {
            events.extend(self.retry_item(view, retry, "completed")?);
        }
        if let Some(usage) = message["message"].get("usage").filter(|usage| usage.is_object()) {
            events.push(("provider-turn.updated", self.provider_turn_row(view, "running", None, Some(json!(provider_token_usage(usage, &self.selection, &at(Utc::now())))), None)?));
        }
        let mut tool_events = vec![];
        for block in content.iter().filter(|block| block["id"].is_string() && block["name"].is_string() && block.get("input").is_some()) {
            let (id, name, input) = (block["id"].as_str().unwrap(), block["name"].as_str().unwrap(), block["input"].clone());
            if name == "Agent" {
                if self.skip_tools.insert(id.to_owned()) {
                    self.launches.push((id.to_owned(), input));
                }
                continue;
            }
            if name == "TodoWrite" {
                tool_events.extend(self.plan_projection(view, id, "todo_list", Some(todo_steps(&input)), None)?);
            }
            tool_events.extend(self.tool_started(view, id, name, input, message.get("tool_use_meta"))?);
        }
        let text: String = content.iter().filter(|block| block["type"] == "text").filter_map(|block| block["text"].as_str()).collect();
        if !text.is_empty() {
            events.extend(self.assistant_text(view, uuid, &text)?);
        }
        events.extend(tool_events);
        for result in tool_results(message) {
            events.extend(self.tool_result(view, &result, None, message)?);
        }
        Ok(events)
    }
    fn assistant_text(&mut self, view: &Value, native: &str, text: &str) -> Result<Events, StoreError> {
        if !self.emitted_text.insert(native.to_owned()) {
            return Ok(vec![]);
        }
        let now = at(Utc::now());
        let ordinal = self.ordinal(view, native);
        let provider_turn = self.provider_turn(view)?;
        Ok(self.assistant_artifacts(native, text, ordinal, &now, &now, &provider_turn))
    }
    fn assistant_artifacts(&self, native: &str, text: &str, ordinal: u64, started: &str, completed: &str, provider_turn: &Value) -> Events {
        let node = node_id(native);
        let message = message_id(native);
        let node_row = self.node(&node, "assistant_message", "completed", Some(native), &self.ctx.root_node_id, started, Some(completed), provider_turn);
        let message_row = json!({"id":message,"threadId":self.thread_id,"runId":self.run_id,"nodeId":node,"role":"assistant","createdBy":"agent","creationSource":"provider","text":text,"attachments":[],"streaming":false,"createdAt":completed,"updatedAt":completed});
        let mut item = self.base_item(&item_id(native), &json!(node), ordinal, "completed", Value::Null, Some(native), started, Some(completed), completed, provider_turn);
        item["type"] = json!("assistant_message");
        item["messageId"] = json!(message);
        item["text"] = json!(text);
        item["streaming"] = json!(false);
        vec![("node.updated", node_row), ("message.updated", message_row), ("turn-item.updated", item)]
    }

    fn user(&mut self, view: &Value, message: &Value) -> Result<Events, StoreError> {
        let results = tool_results(message);
        let structured = (results.len() == 1).then(|| message.get("tool_use_result")).flatten().filter(|value| !value.is_null()).cloned();
        let mut events = vec![];
        for result in results {
            events.extend(self.tool_result(view, &result, structured.as_ref(), message)?);
        }
        Ok(events)
    }

    pub(crate) fn tool_started(&mut self, view: &Value, id: &str, name: &str, input: Value, meta: Option<&Value>) -> Result<Events, StoreError> {
        if self.skip_tools.contains(id) {
            return Ok(vec![]);
        }
        let presentation = meta
            .and_then(Value::as_array)
            .and_then(|meta| meta.iter().find(|entry| entry["id"] == id))
            .map(|entry| mcp_presentation_with_meta(name, entry))
            .unwrap_or_else(|| mcp_presentation(&name.replacen("mcp__claude_ai_", "mcp__", 1)));
        if let Some(existing) = self.tool_calls.get(id) {
            let _ = existing;
            return Ok(vec![]);
        }
        let now = at(Utc::now());
        let ordinal = self.ordinal(view, id);
        let call = ToolCall { name: name.to_owned(), input, ordinal, started_at: now.clone(), presentation };
        self.tool_calls.insert(id.to_owned(), call.clone());
        let provider_turn = self.provider_turn(view)?;
        Ok(self.tool_artifacts(id, &call, None, "running", &now, None, &provider_turn))
    }
    /// `buildToolCallArtifacts`
    pub(crate) fn tool_artifacts(&self, id: &str, call: &ToolCall, output: Option<&ToolOutput>, status: &str, updated: &str, non_execution: Option<&str>, provider_turn: &Value) -> Events {
        let classification = classify(&call.name);
        let completed = (status != "running").then_some(updated);
        let node = self.node(&node_id(id), "tool_call", status, Some(id), &self.ctx.root_node_id, &call.started_at, completed, provider_turn);
        let input = &call.input;
        let read_path = matches!(classification.normalized.as_str(), "read" | "readfile").then(|| first_string(input, &["file_path", "path"])).flatten();
        let search_title = matches!(classification.normalized.as_str(), "grep" | "glob" | "ls").then(|| search_label(input)).flatten();
        let title = read_path
            .as_deref()
            .map(|path| format!("Read {}", path.trim()))
            .or(search_title)
            .or_else(|| (call.name == "Skill").then(|| first_string(input, &["skill"]).map(|skill| format!("Skill: {skill}"))).flatten())
            .or_else(|| call.presentation["title"].as_str().map(str::to_owned));
        let mut item = self.base_item(&item_id(id), &json!(node_id(id)), call.ordinal, status, title.map_or(Value::Null, Value::from), Some(id), &call.started_at, completed, updated, provider_turn);
        if let Some(kind) = non_execution {
            item["toolNonExecutionKind"] = json!(kind);
        }
        let output_value = output.map(ToolOutput::value);
        let output_text = |command: bool| output.map(|output| if command { output.command_text() } else { output.text() }).unwrap_or_default();
        match classification.item_type {
            "command_execution" => {
                item["type"] = json!("command_execution");
                item["input"] = json!(first_string(input, &["command", "cmd", "script"]).unwrap_or_else(|| format!("{}: {}", call.name, js_stringify(input))));
                let text = output_text(true);
                if !text.is_empty() {
                    item["output"] = json!(text);
                }
            }
            "file_change" => {
                item["type"] = json!("file_change");
                item["fileName"] = json!(first_string(input, &["file_path", "path", "filename", "fileName"]).unwrap_or_else(|| format!("{} result", call.name)));
                let text = output_text(false);
                if !text.is_empty() {
                    item["diffStr"] = json!(text);
                }
            }
            "web_search" => {
                item["type"] = json!("web_search");
                let pattern = first_string(input, &["query", "url", "pattern"]).or_else(|| output_value.as_ref().and_then(|value| web_search_output(value)).map(|search| search["query"].as_str().unwrap_or("").to_owned()));
                if let Some(pattern) = pattern.filter(|pattern| !pattern.trim().is_empty()) {
                    item["patterns"] = json!([pattern]);
                }
                let results = output_value.as_ref().map(web_search_results).unwrap_or_default();
                if !results.is_empty() {
                    item["results"] = json!(results);
                } else if classification.normalized == "webfetch" && !output_text(false).trim().is_empty() {
                    let mut result = json!({"snippet":truncate_chars(&output_text(false), WEB_FETCH_SNIPPET_MAX_CHARS)});
                    if let Some(url) = first_string(input, &["url"]) {
                        result["url"] = json!(url);
                    }
                    item["results"] = json!([result]);
                }
            }
            _ => {
                item["type"] = json!("dynamic_tool");
                item["toolName"] = json!(call.name);
                if let Some(path) = read_path.filter(|path| path.len() <= 4096 && !path.contains(['\r', '\n']) && is_image_path(path)) {
                    item["viewedImagePath"] = json!(path);
                }
                item["input"] = input.clone();
                if let Some(value) = output_value {
                    item["output"] = value;
                }
                if let Some(source) = call.presentation.get("toolSource") {
                    item["toolSource"] = source.clone();
                }
                if let Some(icon) = call.presentation.get("toolIcon") {
                    item["toolIcon"] = icon.clone();
                }
            }
        }
        vec![("node.updated", node), ("turn-item.updated", item)]
    }
    pub(crate) fn tool_result(&mut self, view: &Value, result: &Value, structured: Option<&Value>, message: &Value) -> Result<Events, StoreError> {
        let Some(id) = result["tool_use_id"].as_str().map(str::to_owned) else { return Ok(vec![]) };
        if self.skip_tools.contains(&id) {
            return Ok(vec![]);
        }
        let mut events = vec![];
        if !self.tool_calls.contains_key(&id) {
            let name = tool_name_from_result(result);
            events.extend(self.tool_started(view, &id, name, json!({}), None)?);
        }
        let call = self.tool_calls.shift_remove(&id).ok_or_else(|| err("Tool call missing."))?;
        let output = ToolOutput::from_result(result, structured);
        // `claudeToolNonExecutionKind`: the CLI says why a tool never ran (a permission rule, a cancel).
        let non_execution = message["tool_result_meta"].as_array().and_then(|meta| meta.iter().find(|meta| meta["id"] == id.as_str())).and_then(|meta| meta["non_execution_kind"].as_str());
        let status = if non_execution == Some("cancelled") { "cancelled" } else if result_is_error(result) { "failed" } else { "completed" };
        let provider_turn = self.provider_turn(view)?;
        events.extend(self.tool_artifacts(&id, &call, Some(&output), status, &at(Utc::now()), non_execution, &provider_turn));
        Ok(events)
    }

    fn system(&mut self, view: &Value, message: &Value) -> Result<Events, StoreError> {
        match message["subtype"].as_str() {
            Some("init" | "status") => {
                if let Some(mode) = message["permissionMode"].as_str() {
                    self.permission_mode = Some(mode.to_owned());
                }
                Ok(vec![])
            }
            Some("api_retry") => {
                let now = at(Utc::now());
                let retry = json!({"attempt":message["attempt"].as_f64().unwrap_or(1.0).trunc().max(1.0),"maxAttempts":message["max_retries"].as_f64().unwrap_or(1.0).trunc().max(1.0),"retryDelayMs":message["retry_delay_ms"].as_f64().unwrap_or(0.0).trunc().max(0.0)});
                let failure = failure_from_api_retry(message);
                let (started, ordinal) = match self.retry.take() {
                    Some((_, _, started, ordinal)) => (started, ordinal),
                    None => (now.clone(), self.ordinal(view, &format!("terminal-failure:{}", self.turn_native_id))),
                };
                let state = (failure, retry, started, ordinal);
                let events = self.retry_item(view, state.clone(), "running")?;
                self.retry = Some(state);
                Ok(events)
            }
            Some("compact_boundary") => {
                let now = at(Utc::now());
                let native = message["uuid"].as_str().unwrap_or("compaction").to_owned();
                let ordinal = self.ordinal(view, &native);
                let provider_turn = self.provider_turn(view)?;
                let mut events = vec![];
                let post = message["compact_metadata"]["post_tokens"].as_f64().filter(|tokens| tokens.is_finite() && *tokens > 0.0);
                if let Some(post) = post {
                    let usage = json!({"usedTokens":post.round() as u64,"maxTokens":claude_models::context_window_tokens(&self.selection),"updatedAt":now});
                    events.push(("provider-turn.updated", self.provider_turn_row(view, "running", None, Some(usage), None)?));
                }
                let mut item = self.base_item(&item_id(&native), &self.root_node().clone(), ordinal, "completed", json!("Context compacted"), Some(&native), &now, Some(&now), &now, &provider_turn);
                item["type"] = json!("compaction");
                item["driver"] = json!(DRIVER);
                if let Some(before) = message["compact_metadata"]["pre_tokens"].as_f64().filter(|tokens| tokens.is_finite()) {
                    item["beforeTokenCount"] = json!(before.round() as u64);
                }
                if let Some(post) = post {
                    item["afterTokenCount"] = json!(post.round() as u64);
                }
                events.push(("turn-item.updated", item));
                Ok(events)
            }
            _ => Ok(vec![]),
        }
    }
    fn retry_item(&mut self, view: &Value, (failure, retry, started, ordinal): (Value, Value, String, u64), status: &str) -> Result<Events, StoreError> {
        let now = at(Utc::now());
        let provider_turn = self.provider_turn(view)?;
        let id = format!("{}:retry", self.turn_native_id);
        let mut item = self.base_item(&item_id(&id), &self.root_node().clone(), ordinal, status, json!("Provider error"), None, &started, (status != "running").then_some(now.as_str()), &now, &provider_turn);
        item["type"] = json!("error");
        item["failure"] = failure;
        item["retry"] = retry;
        Ok(vec![("turn-item.updated", item)])
    }

    /// `emitClaudePlanProjection`
    pub(crate) fn plan_projection(&mut self, view: &Value, native: &str, kind: &'static str, steps: Option<Vec<Value>>, markdown: Option<String>) -> Result<Events, StoreError> {
        let now = at(Utc::now());
        let projection_native = format!("{kind}:{native}");
        let plan_key = format!("{}:{projection_native}", self.thread_id);
        let plan_id = self.plan_ids.entry(plan_key).or_insert_with(|| format!("plan:provider:{DRIVER}:{}", uuid::Uuid::new_v4())).clone();
        let node = node_id(&projection_native);
        let ordinal = self.ordinal(view, &projection_native);
        let steps = steps.unwrap_or_default();
        let provider_turn = self.provider_turn(view)?;
        let plan = if kind == "todo_list" {
            let status = if steps.iter().all(|step| step["status"] == "completed") { "completed" } else { "active" };
            json!({"id":plan_id,"threadId":self.thread_id,"runId":self.run_id,"nodeId":node,"kind":"todo_list","status":status,"steps":steps})
        } else {
            json!({"id":plan_id,"threadId":self.thread_id,"runId":self.run_id,"nodeId":node,"kind":"proposed_plan","status":"active","markdown":markdown.clone().unwrap_or_default()})
        };
        let node_row = self.node(&node, if kind == "todo_list" { "todo_list" } else { "plan" }, "completed", Some(native), &self.ctx.root_node_id, &now, Some(&now), &provider_turn);
        let mut item = self.base_item(&item_id(&projection_native), &json!(node), ordinal, "completed", Value::Null, Some(native), &now, Some(&now), &now, &provider_turn);
        if kind == "todo_list" {
            item["type"] = json!("todo_list");
            item["planId"] = json!(plan_id);
            item["steps"] = json!(steps);
        } else {
            item["type"] = json!("proposed_plan");
            item["planId"] = json!(plan_id);
            item["markdown"] = json!(markdown.unwrap_or_default());
            item["streaming"] = json!(false);
        }
        let mut events = vec![];
        if let Some(previous) = self.latest_plan.get(kind) {
            if previous["id"] != plan["id"] && previous["status"] != "completed" {
                let mut superseded = previous.clone();
                superseded["status"] = json!("superseded");
                events.push(("plan.updated", superseded));
            }
        }
        self.latest_plan.insert(kind, plan.clone());
        events.extend([("node.updated", node_row), ("plan.updated", plan), ("turn-item.updated", item)]);
        Ok(events)
    }

    /// A tool call raised by a permission prompt before its `tool_use` frame was handled.
    pub(crate) fn ensure_tool(&mut self, view: &Value, id: &str, name: &str, input: &Value) -> Result<Events, StoreError> {
        self.tool_started(view, id, name, input.clone(), None)
    }
    /// An approval or user-input request: node, runtime request and item. Returns the request id too.
    pub(crate) fn request_artifacts(&mut self, view: &Value, native_item: &str, native_request: &str, kind: &str, prompt: Option<&str>, questions: Option<Vec<Value>>, session_id: &str) -> Result<(String, Events), StoreError> {
        let now = at(Utc::now());
        let provider_turn = self.provider_turn(view)?;
        let request_id = format!("runtime-request:provider:{DRIVER}:provider-turn:{}:native-request:{}:{}", enc(provider_turn.as_str().unwrap_or("")), enc(native_request), uuid::Uuid::new_v4().simple());
        let node = format!("node:runtime-request:{}", enc(&request_id));
        let ordinal = self.ordinal(view, &format!("{native_item}:approval:{native_request}"));
        let user_input = questions.is_some();
        let mut node_row = self.node(&node, if user_input { "user_input_request" } else { "approval_request" }, "waiting", Some(native_request), &json!(node_id(native_item)), &now, None, &provider_turn);
        node_row["runtimeRequestId"] = json!(request_id);
        let request = json!({"id":request_id,"nodeId":node,"providerTurnId":provider_turn,"nativeRequestRef":native_ref(native_request),"kind":kind,"status":"pending","responseCapability":{"type":"live","providerSessionId":session_id},"createdAt":now,"resolvedAt":null});
        let mut item = self.base_item(&format!("turn-item:runtime-request:{}", enc(&request_id)), &json!(node), ordinal, "waiting", Value::Null, Some(native_request), &now, None, &now, &provider_turn);
        item["requestId"] = json!(request_id);
        match questions {
            Some(questions) => {
                item["type"] = json!("user_input_request");
                item["questions"] = json!(questions);
            }
            None => {
                item["type"] = json!("approval_request");
                item["requestKind"] = json!(kind);
                if let Some(prompt) = prompt {
                    item["prompt"] = json!(prompt);
                }
            }
        }
        Ok((request_id, vec![("node.updated", node_row), ("runtime-request.updated", request), ("turn-item.updated", item)]))
    }

    /// A `rate_limit_event`: remember which windows are rejected (and when each resets) for the
    /// turn's failure, and announce a rejected window once with the wait that remains.
    pub(crate) fn rate_limit(&mut self, view: &Value, info: &Value, names: &crate::claude_usage_limits::ScopedLimitNames, now_ms: f64) -> Result<Events, StoreError> {
        let overage_allowed = matches!(info["overageStatus"].as_str(), Some("allowed" | "allowed_warning")) || info["isUsingOverage"] == true || info["overageInUse"] == true;
        let blocked = info["status"] == "rejected" && !overage_allowed;
        let limit_type = info["rateLimitType"].as_str().unwrap_or("unknown").to_owned();
        if blocked {
            let reset = info["resetsAt"].as_f64().map(|seconds| seconds * 1000.0).filter(|millis| millis.is_finite() && *millis > 0.0 && *millis < 8.64e15).and_then(|millis| chrono::DateTime::<Utc>::from_timestamp_millis(millis as i64)).map(|time| at(time));
            match self.rejected_limits.iter_mut().find(|(kind, _)| *kind == limit_type) {
                Some(existing) => existing.1 = reset,
                None => self.rejected_limits.push((limit_type.clone(), reset)),
            }
        } else if matches!(info["status"].as_str(), Some("allowed" | "allowed_warning")) || overage_allowed {
            self.rejected_limits.retain(|(kind, _)| *kind != limit_type);
        }
        if !blocked {
            return Ok(vec![]);
        }
        // Repeats of a window need only one notice.
        let key = format!("{limit_type}:{}", info["resetsAt"].as_f64().map(|seconds| if seconds.fract() == 0.0 { format!("{}", seconds as i64) } else { seconds.to_string() }).unwrap_or_else(|| "unknown".into()));
        if !self.announced_limits.insert(key.clone()) {
            return Ok(vec![]);
        }
        let provider_turn = self.provider_turn(view)?;
        let native = format!("usage-limit:{}:{key}", provider_turn.as_str().unwrap_or(""));
        let notice = crate::claude_usage_limits::describe_usage_limit(info, now_ms, names);
        let now = at(Utc::now());
        let ordinal = self.ordinal(view, &native);
        let mut item = self.base_item(&item_id(&native), &self.root_node().clone(), ordinal, "completed", json!(notice), Some(&native), &now, Some(&now), &now, &provider_turn);
        item["type"] = json!("system_notice");
        item["message"] = json!(notice);
        Ok(vec![("turn-item.updated", item)])
    }
    /// Whether this result is the end of a usage limit rather than another API failure.
    fn usage_limited(&self, result: &Value) -> bool {
        !self.auth_failed
            && (!self.rejected_limits.is_empty() || self.assistant_rate_limited)
            && (result["subtype"] != "success" || result["api_error_status"].is_null() || result["api_error_status"].as_i64() == Some(429))
            && (result["terminal_reason"].is_null() || matches!(result["terminal_reason"].as_str(), Some("api_error" | "blocking_limit")))
    }
    /// The latest reset any rejected window named; none when a window did not name one.
    fn reset_at(&self) -> Option<String> {
        if self.rejected_limits.is_empty() || self.rejected_limits.iter().any(|(_, reset)| reset.is_none()) {
            return None;
        }
        self.rejected_limits.iter().filter_map(|(_, reset)| reset.clone()).max()
    }

    fn provider_turn_row(&self, view: &Value, status: &str, completed_at: Option<&str>, token_usage: Option<Value>, turn_usage: Option<Value>) -> Result<Value, StoreError> {
        let (_, attempt) = self.ctx.rows(view)?;
        let mut turn = crate::codex_runtime::find(view, "providerTurns", &attempt["providerTurnId"])?.clone();
        turn["status"] = json!(status);
        turn["nativeTurnRef"] = json!({"driver":DRIVER,"nativeId":self.cursor.clone().unwrap_or_else(|| self.turn_native_id.clone()),"strength":"weak"});
        if let Some(completed) = completed_at {
            turn["completedAt"] = json!(completed);
        }
        if let Some(usage) = token_usage {
            turn["tokenUsage"] = usage;
        }
        if let Some(usage) = turn_usage {
            turn["turnTokenUsage"] = usage;
        }
        Ok(turn)
    }

    /// Decide how a `result` message ends the turn.
    pub(crate) fn terminal(&self, result: &Value, interrupted: bool, steered_abort: bool) -> Option<Terminal> {
        if steered_abort && !interrupted && matches!(result["terminal_reason"].as_str(), Some("aborted_streaming" | "aborted_tools")) {
            return None;
        }
        let usage_limited = self.usage_limited(result);
        let hint = self
            .auth_failed
            .then(|| "Claude could not authenticate. For subscription login, run `claude auth login` on this environment's machine, then start a new thread. For API-key authentication, check this instance's configured credentials.".to_owned())
            .or_else(|| usage_limited.then(|| "Claude usage limit reached. Send the message again once the limit resets.".to_owned()));
        let status = if interrupted { "interrupted" } else { terminal_status(result, hint.as_deref()) };
        let failure = if interrupted {
            None
        } else {
            failure_from_result(result, hint.as_deref(), usage_limited).map(|mut failure| {
                if failure["class"] == "usage_limit" {
                    if let Some(reset) = self.reset_at() {
                        failure["resetAt"] = json!(reset);
                    }
                }
                failure
            })
        };
        Some(Terminal { status, failure })
    }
    /// Events that settle open work before the adapter commits the terminal run state.
    pub(crate) fn finish(&mut self, view: &Value, result: Option<&Value>, status: &str) -> Result<Events, StoreError> {
        let mut events = vec![];
        let ids: Vec<String> = self.reasoning.iter().filter(|(_, block)| !block.completed && !block.text.is_empty()).map(|(id, _)| id.clone()).collect();
        for id in ids {
            events.extend(self.reasoning_item(view, &id, true)?);
        }
        let provider_turn = self.provider_turn(view)?;
        // A stopped turn cuts open tools short; only a turn that ended on its own leaves them failed.
        let open_status = if status == "completed" { "failed" } else { status };
        let open: Vec<(String, ToolCall)> = self.tool_calls.drain(..).collect();
        for (id, call) in open {
            events.extend(self.tool_artifacts(&id, &call, None, if open_status == "failed" || open_status == "interrupted" || open_status == "cancelled" { open_status } else { "failed" }, &at(Utc::now()), None, &provider_turn));
        }
        if self.emitted_text.is_empty() && self.fallback_from(result) {
            let ordinal = self.ordinal(view, &self.fallback_id.clone());
            let now = at(Utc::now());
            let text = self.fallback_text.clone();
            events.extend(self.assistant_artifacts(&self.fallback_id.clone(), &text, ordinal, &self.started_at.clone(), &now, &provider_turn));
        }
        if let Some(retry) = self.retry.take() {
            if status != "failed" {
                events.extend(self.retry_item(view, retry, status)?);
            }
        }
        let usage = turn_token_usage(result, self.has_subagents, status);
        events.push(("provider-turn.updated", self.provider_turn_row(view, "running", None, None, Some(usage))?));
        Ok(events)
    }
    fn fallback_from(&mut self, result: Option<&Value>) -> bool {
        if let Some(result) = result.filter(|result| result["type"] == "result" && result["subtype"] == "success" && result["is_error"] != true) {
            self.fallback_text = result["result"].as_str().unwrap_or("").to_owned();
        }
        !self.fallback_text.is_empty()
    }
}

pub(crate) fn mcp_presentation_with_meta(name: &str, meta: &Value) -> Value {
    let mut presentation = mcp_presentation(&name.replacen("mcp__claude_ai_", "mcp__", 1));
    if let Some(title) = mcp_text(meta["display_name"].as_str(), 160) {
        presentation["title"] = json!(title);
    }
    if let (Some(name), Some(source)) = (mcp_text(meta["server_display_name"].as_str(), 160), presentation.get_mut("toolSource")) {
        source["name"] = json!(name);
    }
    // `normalizeMcpHttpUrl`: only http(s) icons of bounded length.
    let icon = meta["icon_url"].as_str().filter(|url| url.len() <= 4096).and_then(|text| url::Url::parse(text).ok()).filter(|url| matches!(url.scheme(), "http" | "https") && url.as_str().len() <= 4096);
    if let (Some(icon), Some(source)) = (icon, presentation.get_mut("toolSource")) {
        let icon = json!({"_tag":"themed-logo","logoUrl":icon.as_str()});
        source["icon"] = icon.clone();
        presentation["toolIcon"] = icon;
    }
    presentation
}
fn is_image_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"].iter().any(|extension| lower.ends_with(extension))
}
fn search_label(input: &Value) -> Option<String> {
    let pick = |keys: &[&str]| keys.iter().find_map(|key| input.get(*key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned));
    let query = pick(&["pattern", "query", "searchTerm", "regex", "grep", "needle"]);
    let glob = pick(&["glob", "globPattern", "glob_pattern", "include", "filePattern", "file_pattern"]);
    let target = pick(&["path", "target_directory", "targetDirectory", "directory", "cwd", "root"]).and_then(|value| value.split(['/', '\\']).rev().find(|part| !part.is_empty() && *part != ".").map(str::to_owned));
    match (query, glob, target) {
        (Some(query), _, Some(target)) => Some(format!("Searched {query} in {target}")),
        (None, Some(glob), Some(target)) => Some(format!("Searched files {glob} in {target}")),
        (None, Some(glob), None) => Some(format!("Searched files {glob}")),
        (Some(query), _, None) => Some(format!("Searched {query}")),
        (None, None, Some(target)) => Some(format!("Searched in {target}")),
        _ => None,
    }
}
fn web_search_output(value: &Value) -> Option<&Value> {
    (value["query"].is_string() && value["results"].is_array() && value["durationSeconds"].is_number()).then_some(value)
}
fn web_search_results(value: &Value) -> Vec<Value> {
    let Some(search) = web_search_output(value) else { return vec![] };
    search["results"].as_array().into_iter().flatten().filter(|result| !result.is_string()).flat_map(|result| result["content"].as_array().into_iter().flatten().map(|content| json!({"title":content["title"],"url":content["url"]})).collect::<Vec<_>>()).collect()
}

/// The result blocks a message carries (`tool_result` and the typed `*_tool_result` blocks).
pub(crate) fn tool_results(message: &Value) -> Vec<Value> {
    message["message"]["content"].as_array().into_iter().flatten().filter(|block| block["tool_use_id"].is_string()).cloned().collect()
}
pub(crate) fn tool_name_from_result(result: &Value) -> &'static str {
    match result["type"].as_str() {
        Some("bash_code_execution_tool_result") => "bash_code_execution",
        Some("code_execution_tool_result") => "code_execution",
        Some("advisor_tool_result") => "advisor",
        Some("mcp_tool_result") => "mcp_tool",
        Some("text_editor_code_execution_tool_result") => "text_editor_code_execution",
        Some("tool_search_tool_result") => "tool_search",
        Some("web_fetch_tool_result") => "web_fetch",
        Some("web_search_tool_result") => "web_search",
        _ => "tool",
    }
}
pub(crate) fn result_is_error(result: &Value) -> bool {
    match result["type"].as_str() {
        Some("tool_result") => result["is_error"] == true,
        Some("mcp_tool_result") => result["is_error"] == true,
        _ => result["content"].get("type").and_then(Value::as_str).is_some_and(|kind| kind.ends_with("_tool_result_error")),
    }
}

/// A tool's result: the content block, or the CLI's structured `tool_use_result` with the block as fallback.
pub(crate) struct ToolOutput {
    value: Option<Value>,
    structured: bool,
    fallback: Option<Value>,
}
impl ToolOutput {
    pub(crate) fn from_result(result: &Value, structured: Option<&Value>) -> Self {
        let content = result.get("content").filter(|content| !content.is_null()).cloned();
        match structured {
            Some(structured) => Self { value: Some(structured.clone()), structured: true, fallback: content },
            None => Self { value: content, structured: false, fallback: None },
        }
    }
    pub(crate) fn value(&self) -> Value {
        self.value.clone().unwrap_or(Value::Null)
    }
    pub(crate) fn text(&self) -> String {
        match &self.value {
            None | Some(Value::Null) => String::new(),
            Some(value) => js_stringify(value),
        }
    }
    /// Bash results are `{stdout, stderr, ...}`: keep only the text.
    fn command_text(&self) -> String {
        if let Some(value) = self.value.as_ref().filter(|value| value.is_object()) {
            let (stdout, stderr) = (value["stdout"].as_str(), value["stderr"].as_str());
            if stdout.is_some() || stderr.is_some() {
                let text = [stdout, stderr].into_iter().flatten().filter(|part| !part.trim().is_empty()).collect::<Vec<_>>().join("\n");
                if !text.is_empty() {
                    return text;
                }
                return if self.structured { self.fallback.as_ref().map(subagent_result_text).unwrap_or_default() } else { String::new() };
            }
        }
        self.text()
    }
}
pub(crate) fn subagent_result_text(value: &Value) -> String {
    let content = if value.is_array() { Some(value) } else { value.get("content") };
    if let Some(parts) = content.and_then(Value::as_array) {
        let text: String = parts.iter().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n");
        if !text.is_empty() {
            return text;
        }
    }
    match value {
        Value::Null => String::new(),
        other => js_stringify(other),
    }
}

/// `claudeProviderTurnTokenUsage`
pub(crate) fn provider_token_usage(usage: &Value, selection: &Value, updated_at: &str) -> Value {
    let number = |key: &str| usage[key].as_u64().unwrap_or(0);
    let input = number("input_tokens") + number("cache_creation_input_tokens") + number("cache_read_input_tokens");
    let output = number("output_tokens");
    let mut result = json!({"usedTokens":input + output,"inputTokens":input,"cachedInputTokens":number("cache_read_input_tokens"),"outputTokens":output,"reasoningOutputTokens":0,"updatedAt":updated_at});
    if let Some(max) = claude_models::context_window_tokens(selection) {
        result["maxTokens"] = json!(max);
    } else {
        result["maxTokens"] = Value::Null;
    }
    result
}
/// `normalizeClaudeTurnTokenUsage`
pub(crate) fn turn_token_usage(result: Option<&Value>, has_subagents: bool, status: &str) -> Value {
    let unavailable = || json!({"usageStatus":"unavailable","usageScope":"main_agent","hasSubagents":has_subagents});
    let Some(usage) = result.map(|result| &result["usage"]).filter(|usage| usage.is_object()) else { return unavailable() };
    let count = |key: &str| usage[key].as_f64().filter(|value| value.is_finite() && *value >= 0.0).map(|value| value.round() as u64);
    let (uncached, cached, creation, output) = (count("input_tokens"), count("cache_read_input_tokens"), count("cache_creation_input_tokens"), count("output_tokens"));
    let thinking = usage["output_tokens_details"]["thinking_tokens"].as_f64().filter(|value| value.is_finite() && *value >= 0.0).map(|value| value.round() as u64);
    let cached_contribution = if usage["cache_read_input_tokens"].is_null() { Some(0) } else { cached };
    let creation_contribution = if usage["cache_creation_input_tokens"].is_null() { Some(0) } else { creation };
    let input = match (uncached, cached_contribution, creation_contribution) {
        (Some(a), Some(b), Some(c)) => Some(a + b + c),
        _ => None,
    };
    let known = uncached.is_some() || cached.is_some() || creation.is_some() || output.is_some();
    let positive = uncached.unwrap_or(0) + cached.unwrap_or(0) + creation.unwrap_or(0) + output.unwrap_or(0) > 0;
    let success = result.is_some_and(|result| result["subtype"] == "success");
    if !known || (!success && !positive) {
        return unavailable();
    }
    let mut common = json!({"usageScope":"main_agent","hasSubagents":has_subagents});
    if let Some(cached) = cached {
        common["cachedInputTokens"] = json!(cached);
    }
    if let Some(creation) = creation {
        common["cacheCreationTokens"] = json!(creation);
    }
    if let (Some(thinking), Some(output)) = (thinking, output) {
        common["reasoningTokens"] = json!(output.min(thinking));
    }
    if status == "completed" && success && input.is_some() && output.is_some() {
        common["usageStatus"] = json!("complete");
        common["inputTokens"] = json!(input.unwrap());
        common["outputTokens"] = json!(output.unwrap());
    } else {
        common["usageStatus"] = json!("partial");
        if let Some(input) = input {
            common["inputTokens"] = json!(input);
        }
        if let Some(output) = output {
            common["outputTokens"] = json!(output);
        }
    }
    common
}

fn terminal_result_error(reason: Option<&str>, hint: Option<&str>) -> Option<String> {
    Some(match reason? {
        "api_error" => hint.unwrap_or("Claude gave up after repeated API errors.").to_owned(),
        "malformed_tool_use_exhausted" => "Claude gave up after repeated malformed tool calls.".into(),
        "budget_exhausted" => "Claude stopped: the turn's token budget was exhausted.".into(),
        "structured_output_retry_exhausted" => "Claude could not produce the requested structured output.".into(),
        "tool_deferred_unavailable" => "Claude could not resume a deferred tool call: the tool is no longer available.".into(),
        "turn_setup_failed" => "Claude could not start the turn.".into(),
        "blocking_limit" => "Claude stopped: a usage limit blocked the request.".into(),
        "rapid_refill_breaker" => "Claude stopped: the context refilled too quickly after compaction.".into(),
        "prompt_too_long" => "Claude stopped: the prompt exceeds the model's context window.".into(),
        "image_error" => "Claude stopped: an image in the conversation could not be processed.".into(),
        "model_error" => "Claude stopped: the model returned an error.".into(),
        _ => return None,
    })
}
/// `terminalStatusFromResult`
pub(crate) fn terminal_status(result: &Value, hint: Option<&str>) -> &'static str {
    if matches!(result["terminal_reason"].as_str(), Some("aborted_tools" | "aborted_streaming")) {
        return "interrupted";
    }
    if result["subtype"] == "success" {
        let status = result["api_error_status"].as_i64();
        return if status == Some(529) || status == Some(429) || terminal_result_error(result["terminal_reason"].as_str(), hint).is_some() || (result["is_error"] == true && hint.is_some()) { "failed" } else { "completed" };
    }
    let errors = result["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("\n").to_lowercase();
    if errors.contains("interrupt") {
        "interrupted"
    } else if errors.contains("cancel") {
        "cancelled"
    } else {
        "failed"
    }
}
/// `providerFailureFromResult`
pub(crate) fn failure_from_result(result: &Value, hint: Option<&str>, usage_limited: bool) -> Option<Value> {
    let api_status = result["api_error_status"].as_i64();
    let success = result["subtype"] == "success";
    let class = if result["terminal_reason"] == "blocking_limit" || (success && api_status == Some(429)) || usage_limited { "usage_limit" } else { "provider_error" };
    let listed = if success && result["is_error"] != true { None } else { result["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|error| !error.starts_with("[ede_diagnostic]")).map(str::to_owned) };
    let structured = if success && api_status == Some(529) {
        Some("Claude API is overloaded (529). Try again shortly.".to_owned())
    } else if success && api_status == Some(429) {
        Some("Claude API rate limit reached. Try again later.".to_owned())
    } else {
        terminal_result_error(result["terminal_reason"].as_str(), hint)
    };
    if !success {
        let errors = result["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
        return Some(crate::provider_failure::make(&json!({"message":listed.or(structured).unwrap_or(errors),"code":result["subtype"],"class":class})));
    }
    if result["is_error"] != true && structured.is_none() {
        return None;
    }
    let code = match api_status {
        None => result["terminal_reason"].as_str().unwrap_or("sdk_result_error").to_owned(),
        Some(status) => format!("api_error_{status}"),
    };
    let message = listed.or(structured).or_else(|| hint.map(str::to_owned)).unwrap_or_else(|| result["result"].as_str().unwrap_or("").to_owned());
    let mut input = json!({"message":message,"code":code,"class":class});
    if matches!(api_status, Some(429 | 529)) {
        input["retryable"] = json!(true);
    }
    Some(crate::provider_failure::make(&input))
}
fn failure_from_api_retry(message: &Value) -> Value {
    let error = message["error"].as_str().unwrap_or("unknown");
    let status = message["error_status"].as_f64();
    crate::provider_failure::make(&json!({
        "message":format!("Claude API {}.", error.replace('_', " ")),
        "code":match status { Some(status) => format!("api_error_{}", status.trunc() as i64), None => error.to_owned() },
        "class":match status { Some(429.0) => "usage_limit", None => "transport_error", _ => "provider_error" },
        "retryable":true
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_classification_titles_and_prompts_follow_the_source() {
        assert_eq!(classify("Bash").item_type, "command_execution");
        assert_eq!(classify("MultiEdit").request_kind, "file-change");
        assert_eq!(classify("web_search").item_type, "web_search");
        assert_eq!(classify("mcp__x__y").item_type, "dynamic_tool");
        assert_eq!(summarize_request("Bash", &json!({"command":"ls -la"})), "Bash: ls -la");
        assert_eq!(summarize_request("Read", &json!({"file_path":"/a/b"})), "Read: /a/b");
        assert_eq!(search_label(&json!({"pattern":"foo","path":"/repo/src/"})).as_deref(), Some("Searched foo in src"));
        assert_eq!(mcp_presentation("Bash"), json!({}));
        assert_eq!(mcp_presentation("mcp__linear__create_issue")["toolSource"]["key"], "mcp:linear");
        assert_eq!(todo_steps(&json!({"todos":[{"content":" a ","status":"in_progress"},{"content":"","status":"completed"}]})), vec![json!({"id":"todo-0","text":"a","status":"running"})]);
    }
    #[test]
    fn pure_helpers_match_the_unchanged_source_functions() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-adapter-functions.json")).unwrap();
        let mut seen = 0;
        for row in &rows {
            match row["operation"].as_str().unwrap() {
                "classify" => {
                    let got = classify(row["name"].as_str().unwrap());
                    assert_eq!((got.normalized.as_str(), got.item_type, got.request_kind), (row["output"]["normalizedName"].as_str().unwrap(), row["output"]["itemType"].as_str().unwrap(), row["output"]["requestKind"].as_str().unwrap()), "{row}");
                }
                "todos" => assert_eq!(json!(todo_steps(&row["input"])), row["output"], "{row}"),
                "plan" => assert_eq!(proposed_plan(&row["input"]).map(Value::from).unwrap_or(Value::Null), row["output"], "{row}"),
                "questions" => assert_eq!(json!(user_input_questions(&row["input"])), row["output"], "{row}"),
                "answers" => assert_eq!(sdk_user_input_answers(&row["answers"]), row["output"], "{row}"),
                "presentation" => assert_eq!(mcp_presentation(&row["toolName"].as_str().unwrap().replacen("mcp__claude_ai_", "mcp__", 1)), row["output"], "{row}"),
                _ => continue,
            }
            seen += 1;
        }
        assert!(seen > 30);
    }
}
