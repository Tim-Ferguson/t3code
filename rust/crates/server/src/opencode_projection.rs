//! OpenCode 2 stream events to persisted orchestration events. One [`Projector`] lives for one provider turn (or one
//! execution of a subagent's session). It is pure with respect to I/O: the adapter hands it each event and the current
//! projection and commits what it returns. Mirrors the item-building half of `OpenCode2AdapterV2.ts`
//! (`emitText`, `emitTool`, `emitCompaction`, `finishTurn`, `showRequest`) and `OpenCodeToolItems.ts`.
use crate::{execution::at, persistence::StoreError, provider_turn_context::TurnContext};
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::HashMap;

pub(crate) type Events = Vec<(&'static str, Value)>;
pub(crate) const DRIVER: &str = "opencode";
/// Search results stay on the timeline wire, so their text is a preview.
const SEARCH_PREVIEW_MAX_CHARS: usize = 8_000;

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
pub(crate) fn native_ref(id: &str, strength: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":id,"strength":strength})
}

// ---- assistantStreaming.ts ----

/// Where the markdown of buffered assistant text last settled: the end of the last blank line, closing code fence or
/// list item start that is not inside an open fenced code block. Only fully terminated lines count; a list item start
/// is the one lookahead that may sit on the partial line. A section title holds the boundary until a content line
/// follows it, so a title never lands alone above a block that is still streaming (`splitBufferedAssistantText`).
pub(crate) fn split_buffered(text: &str) -> usize {
    static FENCE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static LIST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HEADING: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let fence_pattern = FENCE.get_or_init(|| regex::Regex::new(r"^( *)(`{3,}|~{3,})").unwrap());
    let list_pattern = LIST.get_or_init(|| regex::Regex::new(r"^[ \t]*(?:[-*+]|\d{1,9}[.)])[ \t]").unwrap());
    let heading_pattern = HEADING.get_or_init(|| regex::Regex::new(r"^#{1,6}(?:[ \t]|$)").unwrap());
    // `^ {0,3}(?:#{1,6}(?:[ \t]|$)|\*\*(?:[^*]|\*(?!\*))+\*\*:?$)`
    let section_title = |line: &str| {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return false;
        }
        if heading_pattern.is_match(trimmed) {
            return true;
        }
        let Some(body) = trimmed.strip_prefix("**") else { return false };
        let inner = body.strip_suffix("**:").or_else(|| body.strip_suffix("**"));
        inner.is_some_and(|inner| !inner.is_empty() && !inner.contains("**") && !inner.ends_with('*'))
    };
    let mut open_fence: Option<(char, usize, usize)> = None;
    let mut boundary: Option<usize> = None;
    let mut line_start = 0;
    let mut title_waits = false;
    loop {
        let newline = text[line_start..].find('\n').map(|at| at + line_start);
        let line = text[line_start..newline.unwrap_or(text.len())].trim_end_matches([' ', '\t', '\r']);
        if open_fence.is_none() && line_start > 0 && !title_waits && list_pattern.is_match(line) {
            boundary = Some(line_start);
        }
        let Some(newline) = newline else { break };
        if let Some(captures) = fence_pattern.captures(line) {
            let indent = captures[1].len();
            let marker = &captures[2];
            match open_fence {
                None => {
                    open_fence = Some((marker.chars().next().unwrap(), marker.len(), indent));
                    title_waits = false;
                }
                Some((character, length, opened)) => {
                    // A closing fence carries no info string.
                    if marker.starts_with(character) && marker.len() >= length && indent <= opened + 3 && line.len() == indent + marker.len() {
                        open_fence = None;
                        boundary = Some(newline + 1);
                    }
                }
            }
        } else if open_fence.is_none() && line.chars().all(|c| c == ' ' || c == '\t') && line_start > 0 {
            if !title_waits {
                boundary = Some(newline + 1);
            }
        } else if open_fence.is_none() {
            if line_start > 0 && !title_waits && heading_pattern.is_match(line) {
                boundary = Some(line_start);
            }
            title_waits = section_title(line);
        }
        line_start = newline + 1;
    }
    boundary.unwrap_or(0)
}

// ---- OpenCodeToolItems.ts ----

/// `openCodeToolProjectionKind`.
fn tool_kind(name: &str) -> &'static str {
    let normalized = name.to_lowercase();
    if normalized == "todowrite" {
        "dynamic_tool"
    } else if normalized.contains("bash") || normalized.contains("shell") {
        "command_execution"
    } else if normalized.contains("edit") || normalized.contains("write") || normalized.contains("patch") {
        "file_change"
    } else if normalized.contains("web") || normalized == "codesearch" || normalized == "code_search" {
        "web_search"
    } else if normalized == "read" {
        "dynamic_tool"
    } else if normalized.contains("glob") || normalized.contains("grep") || normalized.contains("search") || normalized.contains("lsp") {
        "file_search"
    } else {
        "dynamic_tool"
    }
}
fn record_string(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| input.get(*key).and_then(Value::as_str).filter(|value| !value.trim().is_empty()).map(str::to_owned))
}
fn record_number(input: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| input.get(*key).filter(|value| value.is_number()).cloned())
}
/// `stableJson`: strings as they are, anything else pretty-printed.
fn stable_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
/// `openCodeToolTurnItem`: the item body for one tool call. `completed_metadata` is the tool's metadata once it
/// succeeded: exit codes and diffs are only trusted from a finished call.
pub(crate) fn tool_item(mut base: Value, name: &str, input: &Value, output: Option<&str>, completed_metadata: Option<&Value>) -> Value {
    match tool_kind(name) {
        "command_execution" => {
            base["type"] = json!("command_execution");
            base["input"] = json!(record_string(input, &["command", "cmd"]).unwrap_or_else(|| stable_json(input)));
            if let Some(output) = output {
                base["output"] = json!(output);
            }
            if let Some(code) = completed_metadata.and_then(|metadata| record_number(metadata, &["exit", "exitCode"])) {
                base["exitCode"] = code;
            }
        }
        "file_change" => {
            base["type"] = json!("file_change");
            base["fileName"] = json!(record_string(input, &["filePath", "path", "file"]).unwrap_or_else(|| name.to_owned()));
            if let Some(old) = record_string(input, &["oldString", "oldText"]) {
                base["oldStr"] = json!(old);
            }
            if let Some(new) = record_string(input, &["newString", "content", "newText"]) {
                base["newStr"] = json!(new);
            }
            // A failed edit has no diff; keep its error where the diff would be.
            let diff = completed_metadata.and_then(|metadata| record_string(metadata, &["diff", "patch"])).or_else(|| output.filter(|output| base["status"] == "failed" && !output.trim().is_empty()).map(str::to_owned));
            if let Some(diff) = diff {
                base["diffStr"] = json!(diff);
            }
        }
        "file_search" => {
            let pattern = record_string(input, &["pattern", "query", "path", "filePath"]);
            // OpenCode reports matches as plain text, so they are kept as one result row under the searched path.
            let root = record_string(input, &["path", "filePath"]).or_else(|| pattern.clone()).map(|root| root.trim().to_owned());
            let label = crate::acp_tools::search_label(&json!({"input":input,"pattern":pattern}));
            if let Some(label) = label {
                base["title"] = json!(label);
            }
            base["type"] = json!("file_search");
            if let Some(pattern) = &pattern {
                base["pattern"] = json!(pattern);
            }
            if let (Some(output), Some(root)) = (output.filter(|output| !output.trim().is_empty()), root) {
                base["results"] = json!([{"fileName":root,"preview":truncate_chars(output, SEARCH_PREVIEW_MAX_CHARS)}]);
            }
        }
        "web_search" => {
            base["type"] = json!("web_search");
            if let Some(pattern) = record_string(input, &["query", "url", "pattern"]) {
                base["patterns"] = json!([pattern]);
            }
            if let Some(output) = output.filter(|output| !output.trim().is_empty()) {
                let mut result = json!({"snippet":truncate_chars(output, SEARCH_PREVIEW_MAX_CHARS)});
                if let Some(url) = record_string(input, &["url"]).map(|url| url.trim().to_owned()) {
                    result["url"] = json!(url);
                }
                base["results"] = json!([result]);
            }
        }
        _ => {
            if name.to_lowercase() == "read" {
                if let Some(path) = record_string(input, &["filePath", "path", "file"]) {
                    base["title"] = json!(format!("Read {}", path.trim()));
                }
            }
            base["type"] = json!("dynamic_tool");
            base["toolName"] = json!(name);
            base["input"] = input.clone();
            if let Some(output) = output {
                base["output"] = json!(output);
            }
        }
    }
    base
}

/// `openCodePermissionRequestKind`.
pub(crate) fn permission_request_kind(permission: &str, tool_name: Option<&str>) -> &'static str {
    let permission = permission.to_lowercase();
    let tool = tool_name.unwrap_or("").to_lowercase();
    if ["edit", "write", "patch", "apply_patch"].contains(&permission.as_str()) || ["edit", "write", "patch", "apply_patch"].contains(&tool.as_str()) {
        return "file-change";
    }
    if ["read", "glob", "grep", "lsp", "external_directory"].contains(&permission.as_str()) || tool == "read" || tool.contains("glob") || tool.contains("grep") || tool.contains("search") {
        return "file-read";
    }
    "command"
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    Reasoning,
}
/// How a text event changes its block.
pub(crate) enum Change<'a> {
    Started,
    Delta(&'a str),
    Ended(&'a str),
}
struct Block {
    kind: Kind,
    started_at: String,
    text: String,
    /// What the last running write carried and when: only whole paragraphs are written while a block streams.
    delivered: Option<(String, i64)>,
}
#[derive(Clone)]
struct Tool {
    name: String,
    input: Value,
    started_at: String,
}
/// What the usage events of a turn add up to.
#[derive(Default)]
struct Usage {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
}
pub(crate) struct Projector {
    thread_id: String,
    ctx: TurnContext,
    selection: Value,
    /// Prefixes a subagent session's tool ids in native item ids: the model names tool calls, so a child's could repeat its parent's.
    scope: String,
    context_window: Option<u64>,
    /// `paragraph` streams whole paragraphs while a block grows; `turn` writes a block only when it ends.
    paragraphs: bool,
    texts: IndexMap<String, Block>,
    tools: IndexMap<String, Tool>,
    ordinals: HashMap<String, u64>,
    next_ordinal: Option<u64>,
    usage: Usage,
    steps: u64,
    last_step: Option<Value>,
    compaction: Option<(String, String)>,
    compactions: u64,
    pub(crate) used_subagents: bool,
    /// Bound to a subagent's child thread: no run, its own item positions.
    child: bool,
    /// The native turn id used for the provider turn row of a child thread (unused by a root turn).
    provider_turn_override: Option<Value>,
    provider_thread_override: Option<Value>,
    run_id: Value,
}

impl Projector {
    pub(crate) fn new(thread_id: &str, ctx: TurnContext, view: &Value, selection: Value, scope: &str, context_window: Option<u64>) -> Result<Self, StoreError> {
        crate::codex_runtime::find(view, "runs", &json!(ctx.run_id))?;
        let run_id = json!(ctx.run_id);
        Ok(Self::blank(thread_id, ctx, selection, scope, context_window, false, run_id))
    }
    /// A projector for a subagent session's turn: its rows have no run and name the child provider thread and turn.
    pub(crate) fn for_child(thread_id: &str, root_node_id: &Value, instance_id: &Value, provider_thread: Value, provider_turn: Value, selection: Value, scope: &str, context_window: Option<u64>) -> Self {
        let ctx = TurnContext { run_id: String::new(), attempt_id: Value::Null, root_node_id: root_node_id.clone(), provider_thread_id: provider_thread.clone(), provider_turn_id: provider_turn.clone(), provider_instance_id: instance_id.clone() };
        let mut projector = Self::blank(thread_id, ctx, selection, scope, context_window, true, Value::Null);
        projector.provider_turn_override = Some(provider_turn);
        projector.provider_thread_override = Some(provider_thread);
        projector
    }
    fn blank(thread_id: &str, ctx: TurnContext, selection: Value, scope: &str, context_window: Option<u64>, child: bool, run_id: Value) -> Self {
        Self {
            thread_id: thread_id.into(),
            ctx,
            selection,
            scope: scope.into(),
            context_window,
            paragraphs: false,
            texts: Default::default(),
            tools: Default::default(),
            ordinals: Default::default(),
            next_ordinal: None,
            usage: Usage::default(),
            steps: 0,
            last_step: None,
            compaction: None,
            compactions: 0,
            used_subagents: false,
            child,
            provider_turn_override: None,
            provider_thread_override: None,
            run_id,
        }
    }
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }
    pub(crate) fn context(&self) -> &TurnContext {
        &self.ctx
    }
    pub(crate) fn selection(&self) -> &Value {
        &self.selection
    }
    pub(crate) fn stream_paragraphs(&mut self, on: bool) {
        self.paragraphs = on;
    }
    pub(crate) fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window = window;
    }
    pub(crate) fn has_open_tools(&self) -> bool {
        !self.tools.is_empty()
    }
    pub(crate) fn tool_input(&self, id: &str) -> Option<(&str, &Value)> {
        self.tools.get(id).map(|tool| (tool.name.as_str(), &tool.input))
    }
    pub(crate) fn knows_tool(&self, id: &str) -> bool {
        self.tools.contains_key(id)
    }

    /// Items are ordered by arrival: each native item takes the next position once, after everything the thread holds
    /// (a Stop or a steer adds its own item between two of the turn's) and after what this projector already placed.
    pub(crate) fn ordinal(&mut self, view: &Value, native: &str) -> u64 {
        if let Some(existing) = self.ordinals.get(native) {
            return *existing;
        }
        let held = view["turnItems"].as_array().into_iter().flatten().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1);
        let next = held.max(self.next_ordinal.unwrap_or(0));
        self.next_ordinal = Some(next + 1);
        self.ordinals.insert(native.to_owned(), next);
        next
    }
    fn provider_turn(&self, view: &Value) -> Result<Value, StoreError> {
        if let Some(turn) = &self.provider_turn_override {
            return Ok(turn.clone());
        }
        let (_, attempt) = self.ctx.rows(view)?;
        Ok(attempt["providerTurnId"].clone())
    }
    fn provider_thread(&self) -> Value {
        self.provider_thread_override.clone().unwrap_or_else(|| self.ctx.provider_thread_id.clone())
    }
    fn base_item(&self, id: &str, node: &Value, ordinal: u64, status: &str, native: &str, started: &str, completed: Option<&str>, updated: &str, provider_turn: &Value) -> Value {
        json!({"id":id,"threadId":self.thread_id,"runId":self.run_id,"nodeId":node,"providerThreadId":self.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":native_ref(native, "strong"),"parentItemId":null,"ordinal":ordinal,"status":status,"title":null,"startedAt":started,"completedAt":completed,"updatedAt":updated})
    }
    fn node(&self, native: &str, kind: &str, status: &str, started: &str, completed: Option<&str>, provider_turn: &Value) -> Value {
        json!({"id":node_id(native),"threadId":self.thread_id,"runId":self.run_id,"parentNodeId":self.ctx.root_node_id,"rootNodeId":self.ctx.root_node_id,"kind":kind,"status":status,"countsForRun":false,"providerThreadId":self.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":native_ref(native, "strong"),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":completed})
    }

    // ---- text and reasoning ----

    /// Applies one text or reasoning event to its block. The answer says what to write now: the block's name, its text
    /// and whether it is final. A block that streams is written only up to its last whole paragraph, at most every
    /// 400 ms (`makeAssistantStreamingFilter`), so unfinished markdown stays out of the read model; the ended block is
    /// written whole.
    pub(crate) fn apply_text(&mut self, data: &Value, kind: Kind, change: Change<'_>, now_ms: i64) -> Option<(String, String, bool)> {
        let native = format!("{}:{}:{}", data["assistantMessageID"].as_str().unwrap_or(""), if kind == Kind::Text { "text" } else { "reasoning" }, data["ordinal"].as_u64().unwrap_or(0));
        let started = at(Utc::now());
        let block = self.texts.entry(native.clone()).or_insert_with(|| Block { kind, started_at: started, text: String::new(), delivered: None });
        match change {
            Change::Started => None,
            Change::Ended(text) => {
                block.text = text.to_owned();
                Some((native, text.to_owned(), true))
            }
            Change::Delta(delta) => {
                block.text.push_str(delta);
                if !self.paragraphs {
                    return None;
                }
                if block.delivered.as_ref().is_some_and(|(_, at)| now_ms - at < 400) {
                    return None;
                }
                let ready = block.text[..split_buffered(&block.text)].to_owned();
                if ready.is_empty() || block.delivered.as_ref().is_some_and(|(text, _)| *text == ready) {
                    return None;
                }
                block.delivered = Some((ready.clone(), now_ms));
                Some((native, ready, false))
            }
        }
    }
    /// Writes a block as `text`: running, or final and no longer open.
    pub(crate) fn write_text(&mut self, view: &Value, native: &str, text: &str, completed: bool) -> Result<Events, StoreError> {
        let (kind, started) = match self.texts.get(native) {
            Some(block) => (block.kind, block.started_at.clone()),
            None => return Ok(vec![]),
        };
        if completed {
            self.texts.shift_remove(native);
        }
        if text.is_empty() {
            return Ok(vec![]);
        }
        let now = at(Utc::now());
        let status = if completed { "completed" } else { "running" };
        let done = completed.then_some(now.as_str());
        let provider_turn = self.provider_turn(view)?;
        let ordinal = self.ordinal(view, native);
        let node_kind = if kind == Kind::Text { "assistant_message" } else { "reasoning" };
        let node = self.node(native, node_kind, status, &started, done, &provider_turn);
        let mut item = self.base_item(&item_id(native), &json!(node_id(native)), ordinal, status, native, &started, done, &now, &provider_turn);
        if kind == Kind::Reasoning {
            item["type"] = json!("reasoning");
            item["text"] = json!(text);
            item["streaming"] = json!(!completed);
            return Ok(vec![("node.updated", node), ("turn-item.updated", item)]);
        }
        let message = message_id(native);
        let message_row = json!({"id":message,"threadId":self.thread_id,"runId":self.run_id,"nodeId":node_id(native),"role":"assistant","createdBy":"agent","creationSource":"provider","text":text,"attachments":[],"streaming":!completed,"createdAt":started,"updatedAt":now});
        item["type"] = json!("assistant_message");
        item["messageId"] = json!(message);
        item["text"] = json!(text);
        item["streaming"] = json!(!completed);
        // A running assistant node is never written: the message and its item carry the streaming text.
        let mut events = if completed { vec![("node.updated", node)] } else { vec![] };
        events.extend([("message.updated", message_row), ("turn-item.updated", item)]);
        Ok(events)
    }

    // ---- tools ----

    pub(crate) fn tool_started(&mut self, view: &Value, id: &str, name: &str) -> Result<Events, StoreError> {
        self.tools.insert(id.to_owned(), Tool { name: name.to_owned(), input: json!({}), started_at: at(Utc::now()) });
        self.tool(view, id, "running", None, None)
    }
    /// The model finished writing a call's input.
    pub(crate) fn tool_called(&mut self, view: &Value, id: &str, input: &Value) -> Result<Events, StoreError> {
        if let Some(tool) = self.tools.get_mut(id) {
            tool.input = input.clone();
        }
        self.tool(view, id, "running", None, None)
    }
    /// A tool the adapter learned of from the session's history: it was missed while the stream was down.
    pub(crate) fn tool_adopt(&mut self, id: &str, name: &str, input: Option<&Value>) {
        let tool = self.tools.entry(id.to_owned()).or_insert_with(|| Tool { name: name.to_owned(), input: json!({}), started_at: at(Utc::now()) });
        if let Some(input) = input.filter(|input| input.is_object()) {
            tool.input = input.clone();
        }
    }
    /// A tool ended; it is no longer open.
    pub(crate) fn tool_finished(&mut self, view: &Value, id: &str, status: &str, output: Option<&str>, metadata: Option<&Value>) -> Result<Events, StoreError> {
        let events = self.tool(view, id, status, output, metadata)?;
        self.tools.shift_remove(id);
        Ok(events)
    }
    fn tool(&mut self, view: &Value, id: &str, status: &str, output: Option<&str>, metadata: Option<&Value>) -> Result<Events, StoreError> {
        let Some(tool) = self.tools.get(id).cloned() else { return Ok(vec![]) };
        let now = at(Utc::now());
        let done = (status != "running").then_some(now.as_str());
        let native = format!("{}{id}", self.scope);
        let provider_turn = self.provider_turn(view)?;
        let ordinal = self.ordinal(view, &native);
        let node = self.node(&native, "tool_call", status, &tool.started_at, done, &provider_turn);
        let base = self.base_item(&item_id(&native), &json!(node_id(&native)), ordinal, status, &native, &tool.started_at, done, &now, &provider_turn);
        let item = tool_item(base, &tool.name, &tool.input, output, if status == "completed" { metadata } else { None });
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }

    // ---- compaction ----

    pub(crate) fn compaction_started(&mut self, view: &Value, provider_turn_native: &str) -> Result<Events, StoreError> {
        self.compaction = Some((format!("{provider_turn_native}:compaction:{}", self.compactions), at(Utc::now())));
        self.compactions += 1;
        self.compaction_item(view, "running", None)
    }
    pub(crate) fn compaction_ended(&mut self, view: &Value, status: &str, summary: Option<&str>) -> Result<Events, StoreError> {
        self.compaction_item(view, status, summary)
    }
    fn compaction_item(&mut self, view: &Value, status: &str, summary: Option<&str>) -> Result<Events, StoreError> {
        let Some((native, started)) = self.compaction.clone() else { return Ok(vec![]) };
        let now = at(Utc::now());
        let done = (status != "running").then_some(now.as_str());
        let provider_turn = self.provider_turn(view)?;
        let ordinal = self.ordinal(view, &native);
        let mut item = self.base_item(&item_id(&native), &self.ctx.root_node_id.clone(), ordinal, status, &native, &started, done, &now, &provider_turn);
        item["type"] = json!("compaction");
        item["driver"] = json!(DRIVER);
        item["title"] = json!(match status {
            "running" => "Compacting context",
            "completed" => "Context compacted",
            "failed" => "Compaction failed",
            _ => "Compaction interrupted",
        });
        if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
            item["summary"] = json!(summary);
        }
        if status != "running" {
            self.compaction = None;
        }
        Ok(vec![("turn-item.updated", item)])
    }

    // ---- steps and the turn's end ----

    /// A step's tokens: steps add up, and the last step's input is the live context size.
    pub(crate) fn step(&mut self, tokens: &Value) {
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        let (input, output, reasoning) = (count(&tokens["input"]), count(&tokens["output"]), count(&tokens["reasoning"]));
        let (read, write) = (count(&tokens["cache"]["read"]), count(&tokens["cache"]["write"]));
        self.steps += 1;
        self.last_step = Some(tokens.clone());
        self.usage.input += input + read + write;
        self.usage.cached += read;
        self.usage.cache_write += write;
        self.usage.output += output + reasoning;
        self.usage.reasoning += reasoning;
    }
    fn turn_token_usage(&self, status: &str) -> Value {
        if self.steps == 0 {
            return json!({"usageScope":"main_agent","usageStatus":"unavailable","hasSubagents":self.used_subagents});
        }
        json!({"usageScope":"main_agent","usageStatus":if status == "completed" { "complete" } else { "partial" },"inputTokens":self.usage.input,"cachedInputTokens":self.usage.cached,"cacheCreationTokens":self.usage.cache_write,"outputTokens":self.usage.output,"reasoningTokens":self.usage.reasoning,"hasSubagents":self.used_subagents})
    }
    fn context_usage(&self, now: &str) -> Option<Value> {
        let last = self.last_step.as_ref()?;
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        let (input, output, reasoning) = (count(&last["input"]), count(&last["output"]), count(&last["reasoning"]));
        let (read, write) = (count(&last["cache"]["read"]), count(&last["cache"]["write"]));
        Some(json!({"usedTokens":input + read + write + output,"maxTokens":self.context_window,"inputTokens":input + read + write,"cachedInputTokens":read,"outputTokens":output,"reasoningOutputTokens":reasoning,"updatedAt":now}))
    }
    /// The provider turn's row as the turn left it, at `status`.
    pub(crate) fn provider_turn_row(&self, view: &Value, status: &str, completed_at: Option<&str>, native_turn: Option<&str>, usage_status: &str) -> Result<Value, StoreError> {
        let turn_id = self.provider_turn(view)?;
        let mut turn = crate::codex_runtime::find(view, "providerTurns", &turn_id)?.clone();
        turn["status"] = json!(status);
        if let Some(native) = native_turn {
            turn["nativeTurnRef"] = native_ref(native, "weak");
        }
        if let Some(completed) = completed_at {
            turn["completedAt"] = json!(completed);
        }
        turn["turnTokenUsage"] = self.turn_token_usage(usage_status);
        if let Some(usage) = self.context_usage(&at(Utc::now())) {
            turn["tokenUsage"] = usage;
        }
        Ok(turn)
    }
    /// Events that settle open work before the adapter commits the turn's terminal state: blocks still open when the
    /// execution ends are final as they stand, and calls left open end `completed` after a completed turn and
    /// `interrupted` otherwise.
    pub(crate) fn finish(&mut self, view: &Value, status: &str, failed_compaction: bool) -> Result<Events, StoreError> {
        let mut events = vec![];
        let open: Vec<(String, String)> = self.texts.iter().map(|(id, block)| (id.clone(), block.text.clone())).collect();
        for (id, text) in open {
            events.extend(self.write_text(view, &id, &text, true)?);
        }
        let ids: Vec<String> = self.tools.keys().cloned().collect();
        for id in ids {
            events.extend(self.tool_finished(view, &id, if status == "completed" { "completed" } else { "interrupted" }, None, None)?);
        }
        events.extend(self.compaction_item(view, if failed_compaction { "failed" } else { "interrupted" }, None)?);
        if !self.child {
            events.push(("provider-turn.updated", self.provider_turn_row(view, "running", None, None, status)?));
        }
        Ok(events)
    }

    // ---- requests ----

    /// An approval or user-input request: node, runtime request and item. Returns the request id.
    pub(crate) fn request_artifacts(&mut self, view: &Value, native_request: &str, request: &RequestBody, session_id: &str, parent_node: &Value) -> Result<(String, Events), StoreError> {
        let now = at(Utc::now());
        let provider_turn = self.provider_turn(view)?;
        let request_id = format!("runtime-request:provider:{DRIVER}:provider-turn:{}:native-request:{}:{}", enc(provider_turn.as_str().unwrap_or("")), enc(native_request), uuid::Uuid::new_v4().simple());
        let node = format!("node:runtime-request:{}", enc(&request_id));
        let ordinal = self.ordinal(view, native_request);
        let user_input = matches!(request, RequestBody::Questions(_));
        let kind = match request {
            RequestBody::Approval { kind, .. } => *kind,
            RequestBody::Questions(_) => "user_input",
        };
        let mut node_row = json!({"id":node,"threadId":self.thread_id,"runId":self.run_id,"parentNodeId":parent_node,"rootNodeId":self.ctx.root_node_id,"kind":if user_input { "user_input_request" } else { "approval_request" },"status":"waiting","countsForRun":false,"providerThreadId":self.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":native_ref(native_request, "strong"),"runtimeRequestId":request_id,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        node_row["runtimeRequestId"] = json!(request_id);
        let request_row = json!({"id":request_id,"nodeId":node,"providerTurnId":provider_turn,"nativeRequestRef":native_ref(native_request, "strong"),"kind":kind,"status":"pending","responseCapability":{"type":"live","providerSessionId":session_id},"createdAt":now,"resolvedAt":null});
        let mut item = self.base_item(&format!("turn-item:runtime-request:{}", enc(&request_id)), &json!(node), ordinal, "waiting", native_request, &now, None, &now, &provider_turn);
        item["requestId"] = json!(request_id);
        match request {
            RequestBody::Approval { title, kind, prompt, options } => {
                item["type"] = json!("approval_request");
                item["title"] = json!(title);
                item["requestKind"] = json!(kind);
                item["prompt"] = json!(prompt);
                item["options"] = options.clone();
            }
            RequestBody::Questions(questions) => {
                item["type"] = json!("user_input_request");
                item["title"] = json!("User input");
                item["questions"] = questions.clone();
            }
        }
        Ok((request_id, vec![("node.updated", node_row), ("runtime-request.updated", request_row), ("turn-item.updated", item)]))
    }
}
/// A request OpenCode answered or dropped on its own: the request, its node and its item settle.
pub(crate) fn settle_request_events(view: &Value, request_id: &str, status: &str) -> Result<Events, StoreError> {
    let mut request = crate::codex_runtime::find(view, "runtimeRequests", &json!(request_id))?.clone();
    if request["status"] != "pending" {
        return Ok(vec![]);
    }
    let now = at(Utc::now());
    let item_status = if status == "resolved" { "completed" } else { "cancelled" };
    request["status"] = json!(status);
    request["resolvedAt"] = json!(now);
    let mut events = vec![("runtime-request.updated", request.clone())];
    if let Some(node) = view["nodes"].as_array().into_iter().flatten().find(|node| node["id"] == request["nodeId"]) {
        let mut node = node.clone();
        node["status"] = json!(item_status);
        node["completedAt"] = json!(now);
        events.push(("node.updated", node));
    }
    if let Some(item) = view["turnItems"].as_array().into_iter().flatten().find(|item| item["requestId"] == request_id) {
        let mut item = item.clone();
        item["status"] = json!(item_status);
        item["completedAt"] = json!(now);
        item["updatedAt"] = json!(now);
        events.push(("turn-item.updated", item));
    }
    Ok(events)
}
/// What a request shows.
pub(crate) enum RequestBody {
    Approval { title: String, kind: &'static str, prompt: String, options: Value },
    Questions(Value),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tools_are_classified_by_name() {
        for (name, kind) in [("bash", "command_execution"), ("shell", "command_execution"), ("edit", "file_change"), ("apply_patch", "file_change"), ("webfetch", "web_search"), ("websearch", "web_search"), ("codesearch", "web_search"), ("read", "dynamic_tool"), ("glob", "file_search"), ("grep", "file_search"), ("lsp", "file_search"), ("todowrite", "dynamic_tool"), ("skill", "dynamic_tool")] {
            assert_eq!(tool_kind(name), kind, "{name}");
        }
    }
    #[test]
    fn a_shell_call_keeps_its_command_output_and_exit_code() {
        let base = json!({"status":"completed"});
        let item = tool_item(base, "shell", &json!({"command":"echo A","timeout":30000}), Some("A\n"), Some(&json!({"exit":0})));
        assert_eq!((item["type"].as_str(), item["input"].as_str(), item["output"].as_str(), item["exitCode"].as_i64()), (Some("command_execution"), Some("echo A"), Some("A\n"), Some(0)));
        // The exit code is only trusted from a finished call.
        let item = tool_item(json!({"status":"failed"}), "shell", &json!({"command":"false"}), Some("boom"), None);
        assert!(item.get("exitCode").is_none());
    }
    #[test]
    fn a_failed_edit_keeps_its_error_where_the_diff_would_be() {
        let item = tool_item(json!({"status":"failed"}), "edit", &json!({"filePath":"a.txt"}), Some("no match"), None);
        assert_eq!((item["fileName"].as_str(), item["diffStr"].as_str()), (Some("a.txt"), Some("no match")));
    }
    #[test]
    fn permission_kinds_follow_the_permission_and_the_tool() {
        assert_eq!(permission_request_kind("shell", Some("shell")), "command");
        assert_eq!(permission_request_kind("edit", None), "file-change");
        assert_eq!(permission_request_kind("external_directory", None), "file-read");
        assert_eq!(permission_request_kind("anything", Some("grep")), "file-read");
    }
}
