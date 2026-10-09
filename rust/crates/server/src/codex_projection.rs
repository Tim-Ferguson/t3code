//! Codex app-server notifications and requests to persisted orchestration events, the way
//! `CodexAdapterV2.ts` projects them. One [`Projector`] lives as long as the Codex actor and is
//! pure with respect to I/O: the actor hands it each notification and the current projection and
//! commits the events it returns. Items take their ordinal when they are first persisted, which is
//! how the original's position store orders them.
use crate::{
    claude_projection::enc,
    codex_approvals::Described,
    codex_tool_presentation as presentation,
    codex_usage::{self, TurnTokenUsage},
    execution::at,
    persistence::StoreError,
    provider_failure,
    provider_turn_context::TurnContext,
};
use chrono::{DateTime, Utc};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub(crate) type Events = Vec<(&'static str, Value)>;
const DRIVER: &str = "codex";

fn err(message: impl Into<String>) -> StoreError {
    StoreError::InvalidCommand(message.into())
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
/// `trimText`: JavaScript's trim, so an all-whitespace string is no text.
fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().map(t3_contracts::trim_wire_string).filter(|text| !text.is_empty())
}
/// `codexItemStatus`: the node status, the item status and whether the item is finished.
fn item_status(native: &str) -> (&'static str, bool) {
    match native {
        "completed" => ("completed", true),
        "failed" => ("failed", true),
        "declined" => ("cancelled", true),
        _ => ("running", false),
    }
}
/// A native millisecond timestamp, or now when Codex did not name one.
fn native_time(millis: &Value) -> String {
    millis
        .as_i64()
        .filter(|millis| *millis > 0)
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map_or_else(|| at(Utc::now()), at)
}

#[derive(Clone)]
enum Stream {
    Agent,
    Reasoning { native_item: String, key: String },
}
struct Buffer {
    stream: Stream,
    native: String,
    text: String,
    dirty: bool,
    started_at: String,
}
struct Retry {
    native_message: String,
    failure: Value,
    retry: Value,
    started_at: String,
}

pub(crate) struct Projector {
    thread_id: String,
    session_id: String,
    ctx: Option<TurnContext>,
    native_turn_id: String,
    allocated: HashMap<String, u64>,
    next: u64,
    started: HashMap<String, String>,
    buffers: IndexMap<String, Buffer>,
    command_output: HashMap<String, (String, bool)>,
    /// Terminals of the commands still running, which an interrupt must stop.
    processes: IndexMap<String, String>,
    final_answers: IndexSet<String>,
    completed_finals: HashSet<String>,
    plan_deltas: IndexMap<String, (String, bool)>,
    plan_ids: HashMap<String, String>,
    retry: Option<Retry>,
    /// The turn's last non-retried error: what a failed turn is classified by.
    pub(crate) latest_failure: Option<(String, Value)>,
    usage: TurnTokenUsage,
    rate_limits: Option<Value>,
    /// A subagent's projector: its rows belong to a child thread, which has no runs.
    child: bool,
    /// The text of the last final answer this turn showed, which is the subagent's result.
    final_result: Option<String>,
}

impl Projector {
    pub(crate) fn new(thread_id: &str, session_id: &str) -> Self {
        Self {
            thread_id: thread_id.into(),
            session_id: session_id.into(),
            ctx: None,
            native_turn_id: String::new(),
            allocated: Default::default(),
            next: 0,
            started: Default::default(),
            buffers: Default::default(),
            command_output: Default::default(),
            processes: Default::default(),
            final_answers: Default::default(),
            completed_finals: Default::default(),
            plan_deltas: Default::default(),
            plan_ids: Default::default(),
            retry: None,
            latest_failure: None,
            usage: Default::default(),
            rate_limits: None,
            child: false,
            final_result: None,
        }
    }
    /// The projector of a subagent's native thread; its rows have no run.
    pub(crate) fn for_child(thread_id: &str, session_id: &str) -> Self {
        Self { child: true, ..Self::new(thread_id, session_id) }
    }
    /// The run a row belongs to: a child thread has none.
    fn run_value(&self) -> Value {
        match (&self.ctx, self.child) {
            (Some(ctx), false) => json!(ctx.run_id),
            _ => Value::Null,
        }
    }
    /// `markSubagentUsage`: the turn used subagents, which its usage record says.
    pub(crate) fn mark_subagents(&mut self) {
        let turn = self.native_turn_id.clone();
        self.usage.mark_subagents(&turn);
    }
    /// Usage is thread-wide: an update for a turn that is not running only moves the baseline.
    pub(crate) fn accumulate_usage(&mut self, turn: &str, usage: &Value) {
        self.usage.accumulate(turn, usage);
    }
    pub(crate) fn take_final_result(&mut self) -> Option<String> {
        self.final_result.take()
    }
    /// A turn started on the native thread; per-turn bookkeeping starts over.
    pub(crate) fn begin(&mut self, ctx: TurnContext, native_turn_id: &str) {
        self.ctx = Some(ctx);
        self.native_turn_id = native_turn_id.into();
        self.allocated.clear();
        self.next = 0;
        self.started.clear();
        self.buffers.clear();
        self.command_output.clear();
        self.processes.clear();
        self.final_answers.clear();
        self.completed_finals.clear();
        self.plan_deltas.clear();
        self.plan_ids.clear();
        self.retry = None;
        self.latest_failure = None;
        self.usage.begin(native_turn_id);
    }
    /// Terminal process ids of the commands that have not finished.
    pub(crate) fn running_processes(&self) -> Vec<String> {
        self.processes.values().cloned().collect()
    }
    pub(crate) fn native_turn(&self) -> String {
        self.native_turn_id.clone()
    }
    /// Whether a command of this turn, or a tool meant to outlive it, is still shown as running:
    /// the turn's late events still project (`turnHasRetainedBackgroundWork`).
    pub(crate) fn has_running_command(&self, view: &Value) -> bool {
        let Some(turn) = self.ctx.as_ref().map(|ctx| &ctx.provider_turn_id) else { return false };
        view["turnItems"].as_array().into_iter().flatten().any(|item| (item["type"] == "command_execution" || crate::background_work::is_persistent_dynamic_tool(item)) && item["providerTurnId"] == *turn && item["status"] == "running")
    }
    pub(crate) fn retry_failure(&self) -> Option<(String, Value)> {
        self.retry.as_ref().map(|retry| (retry.native_message.clone(), retry.failure.clone()))
    }
    fn ctx(&self) -> Result<&TurnContext, StoreError> {
        self.ctx.as_ref().ok_or_else(|| err("Native turn context is missing."))
    }
    /// Streaming text, command output and plan text wait for the actor's short flush timer.
    pub(crate) fn has_pending(&self) -> bool {
        self.buffers.values().any(|buffer| buffer.dirty) || self.command_output.values().any(|(_, dirty)| *dirty) || self.plan_deltas.values().any(|(_, dirty)| *dirty)
    }

    // --- shared row builders ------------------------------------------------------------------
    fn ordinal(&mut self, view: &Value, id: &str) -> u64 {
        if let Some(existing) = view["turnItems"].as_array().into_iter().flatten().find(|item| item["id"] == id).and_then(|item| item["ordinal"].as_u64()) {
            return existing;
        }
        if let Some(existing) = self.allocated.get(id) {
            return *existing;
        }
        let used = view["turnItems"].as_array().into_iter().flatten().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1);
        let ordinal = used.max(self.next);
        self.next = ordinal + 1;
        self.allocated.insert(id.to_owned(), ordinal);
        ordinal
    }
    fn node(&self, id: &str, kind: &str, status: &str, native: Option<&str>, parent: &Value, started: &str, completed: Option<&str>) -> Result<Value, StoreError> {
        let ctx = self.ctx()?;
        Ok(json!({"id":id,"threadId":self.thread_id,"runId":self.run_value(),"parentNodeId":parent,"rootNodeId":ctx.root_node_id,"kind":kind,"status":status,"countsForRun":false,"providerThreadId":ctx.provider_thread_id,"providerTurnId":ctx.provider_turn_id,"nativeItemRef":native.map(native_ref),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":completed}))
    }
    #[allow(clippy::too_many_arguments)]
    fn item(&self, id: &str, node: &str, ordinal: u64, status: &str, title: Value, native: Option<&str>, started: &str, completed: Option<&str>, updated: &str, kind: &str) -> Result<Value, StoreError> {
        let ctx = self.ctx()?;
        Ok(json!({"id":id,"threadId":self.thread_id,"runId":self.run_value(),"nodeId":node,"providerThreadId":ctx.provider_thread_id,"providerTurnId":ctx.provider_turn_id,"nativeItemRef":native.map(native_ref),"parentItemId":null,"ordinal":ordinal,"status":status,"title":title,"startedAt":started,"completedAt":completed,"updatedAt":updated,"type":kind}))
    }
    fn started_at(&mut self, native: &str, hint: Option<String>) -> String {
        self.started.entry(native.to_owned()).or_insert_with(|| hint.unwrap_or_else(|| at(Utc::now()))).clone()
    }
    fn root(&self) -> Result<Value, StoreError> {
        Ok(self.ctx()?.root_node_id.clone())
    }

    // --- notifications ------------------------------------------------------------------------
    /// A notification that only updates bookkeeping and persists nothing: the start of a message,
    /// reasoning or file change (they are shown when they have content), and an error Codex will
    /// not retry (it classifies the turn's failure when the turn ends). None needs the projection.
    pub(crate) fn is_bookkeeping(&self, method: &str, params: &Value) -> bool {
        if self.retry.is_some() {
            return false;
        }
        match method {
            "item/started" => matches!(params["item"]["type"].as_str(), Some("agentMessage" | "userMessage" | "reasoning" | "fileChange" | "subAgentActivity" | "collabAgentToolCall")),
            "error" => params["willRetry"] != true,
            _ => false,
        }
    }
    /// Streaming text, output and plan deltas only buffer; they need no projection unless a retried
    /// error is waiting to be marked recovered. Returns whether the delta was taken.
    pub(crate) fn buffer_delta(&mut self, method: &str, params: &Value) -> bool {
        if self.retry.is_some() {
            return false;
        }
        let (Some(item), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str()) else { return false };
        match method {
            "item/agentMessage/delta" => self.append(item, Stream::Agent, item.to_owned(), delta),
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let (stream, index) = if method == "item/reasoning/textDelta" { ("content", &params["contentIndex"]) } else { ("summary", &params["summaryIndex"]) };
                let key = reasoning_key(&self.native_turn_id, item, stream, index.as_u64().unwrap_or(0));
                self.append(item, Stream::Reasoning { native_item: item.to_owned(), key: key.clone() }, key, delta);
            }
            "item/commandExecution/outputDelta" => {
                let entry = self.command_output.entry(item.to_owned()).or_default();
                entry.0.push_str(delta);
                entry.1 = true;
            }
            "item/plan/delta" => {
                let entry = self.plan_deltas.entry(item.to_owned()).or_default();
                entry.0.push_str(delta);
                entry.1 = true;
            }
            _ => return false,
        }
        true
    }
    pub(crate) fn notification(&mut self, view: &Value, method: &str, params: &Value) -> Result<Events, StoreError> {
        match method {
            "item/started" => self.item_started(view, &params["item"], &params["startedAtMs"]),
            "item/completed" => self.item_completed(view, &params["item"]),
            "item/agentMessage/delta" => {
                let events = self.finish_retry(view, "completed")?;
                if let (Some(item), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                    self.append(item, Stream::Agent, item.to_owned(), delta);
                }
                Ok(events)
            }
            "item/reasoning/summaryTextDelta" => self.append_reasoning(view, params, "summary", &params["summaryIndex"]),
            "item/reasoning/textDelta" => self.append_reasoning(view, params, "content", &params["contentIndex"]),
            "item/commandExecution/outputDelta" => {
                if let (Some(item), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                    let base = view["turnItems"].as_array().into_iter().flatten().find(|row| row["id"] == item_id(item)).and_then(|row| row["output"].as_str()).unwrap_or("").to_owned();
                    let entry = self.command_output.entry(item.to_owned()).or_insert((base, false));
                    entry.0.push_str(delta);
                    entry.1 = true;
                }
                Ok(vec![])
            }
            "item/plan/delta" => {
                let events = self.finish_retry(view, "completed")?;
                if let (Some(item), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                    let entry = self.plan_deltas.entry(item.to_owned()).or_default();
                    entry.0.push_str(delta);
                    entry.1 = true;
                }
                Ok(events)
            }
            "turn/plan/updated" => self.turn_plan(view, params),
            "thread/tokenUsage/updated" => self.token_usage(view, params),
            "error" => self.error(view, params),
            _ => Ok(vec![]),
        }
    }

    fn append(&mut self, native: &str, stream: Stream, key: String, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let started_at = self.started_at(&key, None);
        let buffer = self.buffers.entry(key).or_insert(Buffer { stream, native: native.to_owned(), text: String::new(), dirty: false, started_at });
        buffer.text.push_str(delta);
        buffer.dirty = true;
    }
    fn append_reasoning(&mut self, view: &Value, params: &Value, stream: &str, index: &Value) -> Result<Events, StoreError> {
        let Some((item, delta)) = params["itemId"].as_str().zip(params["delta"].as_str()) else { return Ok(vec![]) };
        if delta.is_empty() {
            return Ok(vec![]);
        }
        let events = self.finish_retry(view, "completed")?;
        let key = reasoning_key(&self.native_turn_id, item, stream, index.as_u64().unwrap_or(0));
        self.append(item, Stream::Reasoning { native_item: item.to_owned(), key: key.clone() }, key, delta);
        Ok(events)
    }

    /// `item/started`: tools and searches show up running; text, reasoning and file changes wait
    /// for their content. Compaction shows immediately.
    fn item_started(&mut self, view: &Value, native: &Value, started_ms: &Value) -> Result<Events, StoreError> {
        let Some(id) = native["id"].as_str() else { return Ok(vec![]) };
        let started = native_time(started_ms);
        match native["type"].as_str() {
            Some("contextCompaction") => {
                self.started_at(id, Some(started));
                self.compaction(view, id, false)
            }
            Some("userMessage" | "subAgentActivity" | "collabAgentToolCall") => Ok(vec![]),
            Some("agentMessage") => {
                self.started_at(id, Some(started));
                let events = self.finish_retry(view, "completed")?;
                if native["phase"] != "commentary" {
                    self.final_answers.insert(id.to_owned());
                }
                Ok(events)
            }
            Some("reasoning") => self.finish_retry(view, "completed"),
            Some("commandExecution") => {
                self.started_at(id, Some(started));
                let mut events = self.finish_retry(view, "completed")?;
                events.extend(self.command(view, native)?);
                Ok(events)
            }
            Some("mcpToolCall" | "dynamicToolCall") => {
                self.started_at(id, Some(started));
                let mut events = self.finish_retry(view, "completed")?;
                events.extend(self.dynamic_tool(view, native)?);
                Ok(events)
            }
            Some("webSearch") => {
                self.started_at(id, Some(started));
                let mut events = self.finish_retry(view, "completed")?;
                events.extend(self.web_search(view, native, false)?);
                Ok(events)
            }
            _ => {
                self.started_at(id, Some(started));
                self.finish_retry(view, "completed")
            }
        }
    }

    fn item_completed(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let Some(id) = native["id"].as_str() else { return Ok(vec![]) };
        match native["type"].as_str() {
            Some("reasoning") => self.complete_reasoning(view, native),
            Some("contextCompaction") => self.compaction(view, id, true),
            Some("commandExecution") => {
                self.command_output.remove(id);
                self.command(view, native)
            }
            Some("mcpToolCall" | "dynamicToolCall") => self.dynamic_tool(view, native),
            Some("fileChange") => self.file_change(view, native),
            Some("webSearch") => self.web_search(view, native, true),
            Some("plan") => {
                let buffered = self.plan_deltas.shift_remove(id).map(|(text, _)| text).unwrap_or_default();
                let markdown = native["text"].as_str().filter(|text| !text.is_empty()).map_or(buffered, str::to_owned);
                self.proposed_plan(view, id, &markdown, true)
            }
            Some("agentMessage") => self.complete_agent_message(view, native),
            _ => Ok(vec![]),
        }
    }

    // --- subagents ----------------------------------------------------------------------------
    /// `emitSubagentUserMessage`: what continued a subagent in a later turn, as a message from the
    /// thread that sent it. Nothing for an empty message.
    pub(crate) fn subagent_user_message(&mut self, view: &Value, item: &Value, sender_thread: &str, started_ms: &Value) -> Result<Events, StoreError> {
        let Some(id) = item["id"].as_str() else { return Ok(vec![]) };
        let text = item["content"].as_array().into_iter().flatten().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n");
        let text = t3_contracts::trim_wire_string(&text).to_owned();
        if text.is_empty() {
            return Ok(vec![]);
        }
        let started = self.started_at(id, started_ms.as_i64().filter(|millis| *millis > 0).map(|_| native_time(started_ms)));
        let ordinal = self.ordinal(view, &item_id(id));
        let ctx = self.ctx()?;
        let (message, turn_item) = crate::subagents::conversation_artifacts(
            &json!({"senderThreadId":sender_thread,"messageId":message_id(id),"turnItemId":item_id(id),"threadId":self.thread_id,"rootNodeId":ctx.root_node_id,"providerThreadId":ctx.provider_thread_id,"providerTurnId":ctx.provider_turn_id,"nativeItemRef":native_ref(id),"role":"user","text":text,"ordinal":ordinal}),
            &started,
        );
        Ok(vec![("message.updated", message), ("turn-item.updated", turn_item)])
    }

    // --- text ---------------------------------------------------------------------------------
    /// Whether an assistant message is shown now: a later final answer waits for its own end, and a
    /// final answer repeating one already shown is dropped.
    fn assistant_hidden(&self, native: &str, text: &str, completed: bool) -> bool {
        if !self.final_answers.contains(native) {
            return false;
        }
        let duplicate = completed && !self.completed_finals.is_empty() && (text.is_empty() || self.completed_finals.contains(text));
        let deferred = !completed && (!self.completed_finals.is_empty() || self.final_answers.first().map(String::as_str) != Some(native));
        duplicate || deferred
    }
    fn assistant(&mut self, view: &Value, native: &str, text: &str, completed: bool) -> Result<Events, StoreError> {
        if self.assistant_hidden(native, text, completed) {
            return Ok(vec![]);
        }
        let started = self.started_at(native, None);
        let now = at(Utc::now());
        let (node_id, item_id, message_id) = (node_id(native), item_id(native), message_id(native));
        let ordinal = self.ordinal(view, &item_id);
        let status = if completed { "completed" } else { "running" };
        let completed_at = completed.then_some(now.as_str());
        let root = self.root()?;
        let node = self.node(&node_id, "assistant_message", status, Some(native), &root, &started, completed_at)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(native), &started, completed_at, &now, "assistant_message")?;
        item["messageId"] = json!(message_id);
        item["text"] = json!(text);
        item["streaming"] = json!(!completed);
        let mut events = vec![("node.updated", node)];
        // Clients render streaming text from the item, so the message only carries the final text.
        if completed {
            events.push(("message.updated", json!({"createdBy":"agent","creationSource":"provider","id":message_id,"threadId":self.thread_id,"runId":self.run_value(),"nodeId":node_id,"role":"assistant","text":text,"attachments":[],"streaming":false,"createdAt":started,"updatedAt":now})));
        }
        events.push(("turn-item.updated", item));
        if self.final_answers.contains(native) && completed {
            self.completed_finals.insert(text.to_owned());
        }
        Ok(events)
    }
    fn reasoning(&mut self, view: &Value, key: &str, native_item: &str, text: &str, completed: bool, interrupted: bool) -> Result<Events, StoreError> {
        let started = self.started_at(key, None);
        let now = at(Utc::now());
        let (node_id, item_id) = (node_id(key), item_id(key));
        let ordinal = self.ordinal(view, &item_id);
        let status = if interrupted { "interrupted" } else if completed { "completed" } else { "running" };
        let completed_at = completed.then_some(now.as_str());
        let root = self.root()?;
        let node = self.node(&node_id, "reasoning", status, Some(native_item), &root, &started, completed_at)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(native_item), &started, completed_at, &now, "reasoning")?;
        item["text"] = json!(text);
        item["streaming"] = json!(!completed);
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }
    fn complete_agent_message(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let mut events = vec![];
        if native["delivery"] == "async" && native["questions"].as_array().is_some_and(|questions| !questions.is_empty()) {
            let questions: Vec<Value> = native["questions"].as_array().into_iter().flatten().enumerate().map(|(index, question)| json!({"id":index.to_string(),"header":"Question","question":question["title"],"options":question["options"].as_array().into_iter().flatten().map(|label| json!({"label":label,"description":""})).collect::<Vec<_>>()})).collect();
            let (_, request_events) = self.user_input_artifacts(view, id, &format!("async:{id}"), &questions, Some("message"), None)?;
            self.final_answers.shift_remove(id);
            return Ok(request_events);
        }
        if native["phase"] != "commentary" {
            self.final_answers.insert(id.to_owned());
        }
        let buffered = self.buffers.shift_remove(id).map(|buffer| buffer.text).unwrap_or_default();
        let text = native["text"].as_str().map_or(buffered, str::to_owned);
        let final_answer = self.final_answers.contains(id);
        let shown = self.assistant(view, id, &text, true)?;
        // A subagent's final answer, when it is shown, is what the subagent reports.
        if final_answer && !shown.is_empty() {
            self.final_result = Some(text);
        }
        events.extend(shown);
        self.final_answers.shift_remove(id);
        Ok(events)
    }
    fn complete_reasoning(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let turn = self.native_turn_id.clone();
        for stream in ["summary", "content"] {
            for index in 0..native[stream].as_array().map_or(0, Vec::len) {
                let key = reasoning_key(&turn, id, stream, index as u64);
                self.buffers.entry(key.clone()).or_insert_with(|| Buffer { stream: Stream::Reasoning { native_item: id.to_owned(), key: key.clone() }, native: id.to_owned(), text: String::new(), dirty: false, started_at: at(Utc::now()) });
            }
        }
        let keys: Vec<String> = self.buffers.iter().filter(|(_, buffer)| matches!(&buffer.stream, Stream::Reasoning { native_item, .. } if native_item == id)).map(|(key, _)| key.clone()).collect();
        let mut events = vec![];
        for key in keys {
            let buffer = self.buffers.shift_remove(&key).unwrap();
            let Stream::Reasoning { .. } = &buffer.stream else { continue };
            let (stream, index) = reasoning_part(&key);
            let text = native[stream.as_str()][index].as_str().filter(|text| !text.is_empty()).map_or(buffer.text, str::to_owned);
            if !text.is_empty() {
                self.started.entry(key.clone()).or_insert(buffer.started_at);
                events.extend(self.reasoning(view, &key, id, &text, true, false)?);
            }
        }
        Ok(events)
    }

    /// The timer: persist what streamed since the last flush.
    pub(crate) fn flush(&mut self, view: &Value) -> Result<Events, StoreError> {
        let mut events = vec![];
        let keys: Vec<String> = self.buffers.iter().filter(|(_, buffer)| buffer.dirty).map(|(key, _)| key.clone()).collect();
        for key in keys {
            let (stream, native, text) = {
                let buffer = self.buffers.get_mut(&key).unwrap();
                buffer.dirty = false;
                (buffer.stream.clone(), buffer.native.clone(), buffer.text.clone())
            };
            events.extend(match stream {
                Stream::Agent => self.assistant(view, &native, &text, false)?,
                Stream::Reasoning { native_item, key } => self.reasoning(view, &key, &native_item, &text, false, false)?,
            });
        }
        let outputs: Vec<(String, String)> = self.command_output.iter_mut().filter(|(_, (_, dirty))| *dirty).map(|(id, entry)| {
            entry.1 = false;
            (id.clone(), entry.0.clone())
        }).collect();
        for (id, output) in outputs {
            if let Some(row) = view["turnItems"].as_array().into_iter().flatten().find(|row| row["id"] == item_id(&id)) {
                if row["status"] == "running" {
                    let mut row = row.clone();
                    row["output"] = json!(output);
                    row["updatedAt"] = json!(at(Utc::now()));
                    events.push(("turn-item.updated", row));
                }
            }
        }
        let plans: Vec<(String, String)> = self.plan_deltas.iter_mut().filter(|(_, (_, dirty))| *dirty).map(|(id, entry)| {
            entry.1 = false;
            (id.clone(), entry.0.clone())
        }).collect();
        for (id, markdown) in plans {
            events.extend(self.proposed_plan(view, &id, &markdown, false)?);
        }
        Ok(events)
    }

    // --- tools --------------------------------------------------------------------------------
    fn command(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let (status, finished) = item_status(native["status"].as_str().unwrap_or("inProgress"));
        match native["processId"].as_str() {
            Some(process) if !finished => {
                self.processes.insert(id.to_owned(), process.to_owned());
            }
            _ => {
                self.processes.shift_remove(id);
            }
        }
        let started = self.started_at(id, None);
        let now = at(Utc::now());
        let (node_id, item_id) = (node_id(id), item_id(id));
        let ordinal = self.ordinal(view, &item_id);
        let completed = finished.then_some(now.as_str());
        let root = self.root()?;
        let node = self.node(&node_id, "tool_call", status, Some(id), &root, &started, completed)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(id), &started, completed, &now, "command_execution")?;
        item["input"] = native["command"].clone();
        if let Some(output) = native["aggregatedOutput"].as_str() {
            item["output"] = json!(output);
        }
        if let Some(code) = native["exitCode"].as_i64() {
            item["exitCode"] = json!(code);
        }
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }
    fn file_change(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let Some(first) = native["changes"].as_array().and_then(|changes| changes.first()) else { return Ok(vec![]) };
        let (status, finished) = item_status(native["status"].as_str().unwrap_or("inProgress"));
        let started = self.started_at(id, None);
        let now = at(Utc::now());
        let (node_id, item_id) = (node_id(id), item_id(id));
        let ordinal = self.ordinal(view, &item_id);
        let completed = finished.then_some(now.as_str());
        let root = self.root()?;
        let node = self.node(&node_id, "tool_call", status, Some(id), &root, &started, completed)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(id), &started, completed, &now, "file_change")?;
        item["fileName"] = first["path"].clone();
        item["diffStr"] = first["diff"].clone();
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }
    fn web_search(&mut self, view: &Value, native: &Value, completed: bool) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let status = if completed { "completed" } else { "running" };
        let started = self.started_at(id, None);
        let now = at(Utc::now());
        let (node_id, item_id) = (node_id(id), item_id(id));
        let ordinal = self.ordinal(view, &item_id);
        let finished = completed.then_some(now.as_str());
        let root = self.root()?;
        let node = self.node(&node_id, "tool_call", status, Some(id), &root, &started, finished)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(id), &started, finished, &now, "web_search")?;
        let patterns = web_search_patterns(native);
        if !patterns.is_empty() {
            item["patterns"] = json!(patterns);
        }
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }
    fn dynamic_tool(&mut self, view: &Value, native: &Value) -> Result<Events, StoreError> {
        let id = native["id"].as_str().unwrap_or_default();
        let (status, finished) = item_status(native["status"].as_str().unwrap_or("inProgress"));
        let started = self.started_at(id, None);
        let now = at(Utc::now());
        let (node_id, item_id) = (node_id(id), item_id(id));
        let ordinal = self.ordinal(view, &item_id);
        let completed = finished.then_some(now.as_str());
        let mcp = native["type"] == "mcpToolCall";
        let tool_name = if mcp {
            format!("{}.{}", native["server"].as_str().unwrap_or(""), native["tool"].as_str().unwrap_or(""))
        } else {
            [nonempty(&native["namespace"]).unwrap_or(""), native["tool"].as_str().unwrap_or("")].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(".")
        };
        let presented = if mcp { presentation::mcp_presentation(native) } else { Default::default() };
        let title = presentation::dynamic_tool_title(&tool_name, &native["arguments"]).or_else(|| presented.get("title").and_then(Value::as_str).map(str::to_owned));
        let root = self.root()?;
        let node = self.node(&node_id, "tool_call", status, Some(id), &root, &started, completed)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, title.map_or(Value::Null, Value::from), Some(id), &started, completed, &now, "dynamic_tool")?;
        item["toolName"] = json!(tool_name);
        item["input"] = native["arguments"].clone();
        for (key, value) in presented {
            if key != "title" {
                item[key.as_str()] = value;
            }
        }
        if let Some(output) = if mcp { mcp_output(native) } else { dynamic_output(native) } {
            item["output"] = output;
        }
        // An MCP App the call named: the stored document's reference, beside the full result the app replays.
        if mcp && native["t3McpApp"].is_object() {
            item["output"] = json!({"t3McpApp":native["t3McpApp"],"result":native["result"]});
        }
        Ok(vec![("node.updated", node), ("turn-item.updated", item)])
    }
    fn compaction(&mut self, view: &Value, id: &str, completed: bool) -> Result<Events, StoreError> {
        let started = self.started_at(id, None);
        let now = at(Utc::now());
        let item_id = item_id(id);
        let ordinal = self.ordinal(view, &item_id);
        let root = self.root()?;
        let mut item = self.item(&item_id, root.as_str().unwrap_or(""), ordinal, if completed { "completed" } else { "running" }, json!(if completed { "Context compacted" } else { "Compacting context" }), Some(id), &started, completed.then_some(now.as_str()), &now, "compaction")?;
        item["driver"] = json!(DRIVER);
        Ok(vec![("turn-item.updated", item)])
    }

    // --- plans --------------------------------------------------------------------------------
    fn plan_id(&mut self, key: &str) -> String {
        let thread = self.thread_id.clone();
        self.plan_ids.entry(key.to_owned()).or_insert_with(|| format!("plan:provider:{DRIVER}:thread:{}:{}", enc(&thread), uuid::Uuid::new_v4())).clone()
    }
    fn proposed_plan(&mut self, view: &Value, native: &str, markdown: &str, completed: bool) -> Result<Events, StoreError> {
        let mut events = self.finish_retry(view, "completed")?;
        let started = self.started_at(native, None);
        let now = at(Utc::now());
        let plan_id = self.plan_id(native);
        let (node_id, item_id) = (node_id(native), item_id(native));
        let ordinal = self.ordinal(view, &item_id);
        let status = if completed { "completed" } else { "running" };
        let finished = completed.then_some(now.as_str());
        let run = self.run_value();
        let plan = json!({"id":plan_id,"threadId":self.thread_id,"runId":run,"nodeId":node_id,"kind":"proposed_plan","status":"active","markdown":markdown});
        let root = self.root()?;
        let node = self.node(&node_id, "plan", status, Some(native), &root, &started, finished)?;
        let mut item = self.item(&item_id, &node_id, ordinal, status, Value::Null, Some(native), &started, finished, &now, "proposed_plan")?;
        item["planId"] = json!(plan_id);
        item["markdown"] = json!(markdown);
        item["streaming"] = json!(!completed);
        events.extend([("node.updated", node), ("plan.updated", plan), ("turn-item.updated", item)]);
        Ok(events)
    }
    /// `turn/plan/updated`: the agent's todo list, rewritten whole each time.
    fn turn_plan(&mut self, view: &Value, params: &Value) -> Result<Events, StoreError> {
        let mut events = self.finish_retry(view, "completed")?;
        let steps: Vec<Value> = params["plan"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, step)| {
                let status = match step["status"].as_str() {
                    Some("completed") => "completed",
                    Some("inProgress") => "running",
                    _ => "pending",
                };
                json!({"id":format!("step-{}", index + 1),"text":nonempty(&step["step"]).map_or_else(|| format!("Step {}", index + 1), str::to_owned),"status":status})
            })
            .collect();
        let native = format!("turn-plan:{}", params["turnId"].as_str().unwrap_or(&self.native_turn_id));
        let started = self.started_at(&native, None);
        let now = at(Utc::now());
        let plan_id = self.plan_id(&native);
        let (node_id, item_id) = (node_id(&native), item_id(&native));
        let ordinal = self.ordinal(view, &item_id);
        let run = self.run_value();
        let mut plan = json!({"id":plan_id,"threadId":self.thread_id,"runId":run,"nodeId":node_id,"kind":"todo_list","status":"active","steps":steps});
        let root = self.root()?;
        let node = self.node(&node_id, "todo_list", "running", Some(&native), &root, &started, None)?;
        let mut item = self.item(&item_id, &node_id, ordinal, "running", Value::Null, Some(&native), &started, None, &now, "todo_list")?;
        item["planId"] = json!(plan_id);
        item["steps"] = json!(steps);
        if let Some(explanation) = nonempty(&params["explanation"]) {
            plan["explanation"] = json!(explanation);
            item["explanation"] = json!(explanation);
        }
        events.extend([("node.updated", node), ("plan.updated", plan), ("turn-item.updated", item)]);
        Ok(events)
    }

    // --- usage, errors, limits -----------------------------------------------------------------
    fn provider_turn(&self, view: &Value) -> Result<Value, StoreError> {
        let ctx = self.ctx()?;
        view["providerTurns"].as_array().into_iter().flatten().find(|turn| turn["id"] == ctx.provider_turn_id).cloned().ok_or_else(|| err("Provider turn not found."))
    }
    fn token_usage(&mut self, view: &Value, params: &Value) -> Result<Events, StoreError> {
        let turn = params["turnId"].as_str().unwrap_or_default();
        self.usage.accumulate(turn, &params["tokenUsage"]);
        if turn != self.native_turn_id {
            return Ok(vec![]);
        }
        let mut turn = self.provider_turn(view)?;
        turn["tokenUsage"] = codex_usage::provider_turn_token_usage(&params["tokenUsage"], &at(Utc::now()));
        Ok(vec![("provider-turn.updated", turn)])
    }
    /// `account/rateLimits/updated`: kept for the reset time of a usage-limit stop, which a late
    /// update fills in on the stop the run already recorded.
    pub(crate) fn rate_limits(&mut self, view: &Value, update: &Value) -> Events {
        self.rate_limits = codex_usage::merge_rate_limits(self.rate_limits.as_ref(), update);
        let Some(reset) = codex_usage::usage_limit_reset_at(self.rate_limits.as_ref()) else { return vec![] };
        let Ok(id) = self.error_item_id(view) else { return vec![] };
        match view["turnItems"].as_array().into_iter().flatten().find(|item| item["id"] == id && item["failure"]["class"] == "usage_limit" && item["failure"]["resetAt"].is_null()) {
            Some(item) => {
                let mut item = item.clone();
                item["failure"]["resetAt"] = json!(reset);
                item["updatedAt"] = json!(at(Utc::now()));
                vec![("turn-item.updated", item)]
            }
            None => vec![],
        }
    }
    /// Stamp a usage-limit failure with when the exhausted windows reset.
    pub(crate) fn with_reset(&self, failure: Value) -> Value {
        if failure["class"] != "usage_limit" {
            return failure;
        }
        match codex_usage::usage_limit_reset_at(self.rate_limits.as_ref()) {
            Some(reset) => provider_failure::make(&json!({"message":failure["message"],"code":failure["code"],"class":"usage_limit","retryable":failure["retryable"],"resetAt":reset})),
            None => failure,
        }
    }
    fn error_item_id(&self, view: &Value) -> Result<String, StoreError> {
        let ctx = self.ctx()?;
        if self.child {
            // A subagent turn has no run or attempt to name its failure after.
            return Ok(format!("{}:error", ctx.provider_turn_id.as_str().unwrap_or_default()));
        }
        let (run, attempt) = ctx.rows(view)?;
        let run_id = run["id"].as_str().unwrap_or_default();
        Ok(if attempt["attemptOrdinal"] == 1 { format!("{run_id}:error") } else { format!("{}:error", attempt["id"].as_str().unwrap_or_default()) })
    }
    fn error(&mut self, view: &Value, params: &Value) -> Result<Events, StoreError> {
        let will_retry = params["willRetry"] == true;
        let failure = provider_failure::codex_error(&params["error"], will_retry);
        let message = params["error"]["message"].as_str().unwrap_or("").to_owned();
        if !will_retry {
            self.latest_failure = Some((message, failure));
            return Ok(vec![]);
        }
        let progress = retry_progress(&message);
        let previous = self.retry.take();
        let attempt = progress.map(|(attempt, _)| attempt).unwrap_or_else(|| previous.as_ref().and_then(|retry| retry.retry["attempt"].as_u64()).unwrap_or(0) + 1);
        let max_attempts = progress.map(|(_, max)| json!(max)).unwrap_or_else(|| previous.as_ref().map_or(Value::Null, |retry| retry.retry["maxAttempts"].clone()));
        let started_at = previous.map_or_else(|| at(Utc::now()), |retry| retry.started_at);
        self.retry = Some(Retry { native_message: message, failure, retry: json!({"attempt":attempt,"maxAttempts":max_attempts,"retryDelayMs":null}), started_at });
        self.retry_item(view, "running", None)
    }
    fn retry_item(&mut self, view: &Value, status: &str, at_time: Option<&str>) -> Result<Events, StoreError> {
        let Some(retry) = &self.retry else { return Ok(vec![]) };
        let (failure, retry_value, started) = (retry.failure.clone(), retry.retry.clone(), retry.started_at.clone());
        let id = self.error_item_id(view)?;
        let ordinal = self.ordinal(view, &id);
        let now = at_time.map_or_else(|| at(Utc::now()), str::to_owned);
        let finished = status != "running";
        let title = match status {
            "completed" => "Provider recovered",
            "failed" => if failure["class"] == "usage_limit" { "Usage limit reached" } else { "Provider error" },
            "interrupted" | "cancelled" => "Provider retry stopped",
            _ => "Provider retry",
        };
        let root = self.root()?;
        let mut item = self.item(&id, root.as_str().unwrap_or(""), ordinal, status, json!(title), None, &started, finished.then_some(now.as_str()), &now, "error")?;
        item["failure"] = failure;
        item["retry"] = retry_value;
        Ok(vec![("turn-item.updated", item)])
    }
    /// New model output after a retried error: the retry worked.
    fn finish_retry(&mut self, view: &Value, status: &str) -> Result<Events, StoreError> {
        if self.retry.is_none() {
            return Ok(vec![]);
        }
        let events = self.retry_item(view, status, None)?;
        self.retry = None;
        Ok(events)
    }

    /// The turn is over: persist everything still buffered, then the retry and usage records.
    pub(crate) fn finish(&mut self, view: &Value, status: &str) -> Result<Events, StoreError> {
        let mut events = vec![];
        let interrupted = status != "completed";
        let keys: Vec<String> = self.buffers.keys().cloned().collect();
        for key in keys {
            let buffer = self.buffers.shift_remove(&key).unwrap();
            self.started.entry(key.clone()).or_insert(buffer.started_at.clone());
            events.extend(match buffer.stream {
                Stream::Agent => self.assistant(view, &buffer.native, &buffer.text, true)?,
                Stream::Reasoning { native_item, key } => self.reasoning(view, &key, &native_item, &buffer.text, true, interrupted)?,
            });
        }
        if let Some((_, _)) = self.command_output.iter().find(|(_, (_, dirty))| *dirty) {
            events.extend(self.flush(view)?);
        }
        for (id, (markdown, dirty)) in std::mem::take(&mut self.plan_deltas) {
            if dirty {
                events.extend(self.proposed_plan(view, &id, &markdown, false)?);
            }
        }
        // A retry that ended the turn is the failure the run records; otherwise it ended with the turn.
        // The failure of a turn that ends in one replaces this item under the run's error id.
        if self.retry.is_some() {
            events.extend(self.retry_item(view, status, None)?);
        }
        let mut turn = self.provider_turn(view)?;
        turn["turnTokenUsage"] = self.usage.complete(&self.native_turn_id.clone(), status == "completed");
        events.push(("provider-turn.updated", turn));
        Ok(events)
    }

    // --- requests -----------------------------------------------------------------------------
    /// An approval or question Codex asks mid-turn: the node, the runtime request and its item.
    /// A subagent's request is asked here, on the top-level turn, under the subagent that asked
    /// (`parent`), where the user can see and answer it.
    pub(crate) fn request(&mut self, view: &Value, described: &Described, parent: Option<&str>) -> Result<(String, Events), StoreError> {
        let session_id = self.session_id.clone();
        let ctx = self.ctx()?.clone();
        let created = at(Utc::now());
        let request_id = uuid::Uuid::new_v4().to_string();
        let node = format!("node:runtime-request:{}", enc(&request_id));
        let item_id = format!("turn-item:runtime-request:{}", enc(&request_id));
        let ordinal = self.ordinal(view, &item_id);
        let parent = json!(parent.map_or_else(|| node_id(&described.native_item_id), str::to_owned));
        let mut node_row = self.node(&node, "approval_request", "waiting", Some(&described.native_item_id), &parent, &created, None)?;
        node_row["runtimeRequestId"] = json!(request_id);
        let request = json!({"id":request_id,"nodeId":node,"providerTurnId":ctx.provider_turn_id,"nativeRequestRef":native_ref(&described.native_request_id),"kind":described.request_kind,"status":"pending","responseCapability":{"type":"live","providerSessionId":session_id},"createdAt":created,"resolvedAt":null});
        let mut item = self.item(&item_id, &node, ordinal, "waiting", Value::Null, Some(&described.native_item_id), &created, None, &created, "approval_request")?;
        item["requestId"] = json!(request_id);
        item["requestKind"] = json!(described.request_kind);
        if let Some(prompt) = &described.prompt {
            item["prompt"] = json!(prompt);
        }
        if let Some(app) = &described.app_name {
            item["appName"] = json!(app);
        }
        if let Some(options) = &described.options {
            item["options"] = json!(options);
        }
        let mut events = self.finish_retry(view, "completed")?;
        events.extend([("node.updated", node_row), ("runtime-request.updated", request), ("turn-item.updated", item)]);
        Ok((request_id, events))
    }
    /// A `request_user_input` question (or an async agent question answered by message).
    pub(crate) fn user_input_artifacts(&mut self, view: &Value, native_item: &str, native_request: &str, questions: &[Value], mode: Option<&str>, parent: Option<&str>) -> Result<(String, Events), StoreError> {
        let session_id = self.session_id.clone();
        let ctx = self.ctx()?.clone();
        let created = at(Utc::now());
        let request_id = uuid::Uuid::new_v4().to_string();
        let node = node_id(native_item);
        let item_id = item_id(native_item);
        let ordinal = self.ordinal(view, &item_id);
        let questions = normalize_questions(questions);
        let root = parent.map_or_else(|| self.root(), |parent| Ok(json!(parent)))?;
        let mut node_row = self.node(&node, "user_input_request", "waiting", Some(native_item), &root, &created, None)?;
        node_row["runtimeRequestId"] = json!(request_id);
        let capability = if mode == Some("message") { json!({"type":"message"}) } else { json!({"type":"live","providerSessionId":session_id}) };
        let request = json!({"id":request_id,"nodeId":node,"providerTurnId":ctx.provider_turn_id,"nativeRequestRef":native_ref(native_request),"kind":"user_input","status":"pending","responseCapability":capability,"createdAt":created,"resolvedAt":null});
        let mut item = self.item(&item_id, &node, ordinal, "waiting", Value::Null, Some(native_item), &created, None, &created, "user_input_request")?;
        item["requestId"] = json!(request_id);
        item["questions"] = json!(questions);
        if let Some(mode) = mode {
            item["responseMode"] = json!(mode);
        }
        let mut events = self.finish_retry(view, "completed")?;
        events.extend([("node.updated", node_row), ("runtime-request.updated", request), ("turn-item.updated", item)]);
        Ok((request_id, events))
    }
}

fn reasoning_key(turn: &str, item: &str, stream: &str, index: u64) -> String {
    json!([turn, item, stream, index]).to_string()
}
fn reasoning_part(key: &str) -> (String, usize) {
    let parts: Value = serde_json::from_str(key).unwrap_or(Value::Null);
    (parts[2].as_str().unwrap_or("summary").to_owned(), parts[3].as_u64().unwrap_or(0) as usize)
}
fn retry_progress(message: &str) -> Option<(u64, u64)> {
    let bytes = message.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_') {
            let first_end = (index..bytes.len()).find(|at| !bytes[*at].is_ascii_digit()).unwrap_or(bytes.len());
            let mut cursor = first_end;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'/' {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let second_end = (cursor..bytes.len()).find(|at| !bytes[*at].is_ascii_digit()).unwrap_or(bytes.len());
                let boundary = second_end == bytes.len() || !(bytes[second_end].is_ascii_alphanumeric() || bytes[second_end] == b'_');
                if second_end > cursor && boundary {
                    let (attempt, max) = (message[index..first_end].parse::<u64>().ok()?, message[cursor..second_end].parse::<u64>().ok()?);
                    return (attempt >= 1 && max >= 1).then_some((attempt, max));
                }
            }
            index = first_end;
        } else {
            index += 1;
        }
    }
    None
}
fn normalize_questions(questions: &[Value]) -> Vec<Value> {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let options: Vec<Value> = question["options"].as_array().into_iter().flatten().enumerate().map(|(position, option)| json!({"label":nonempty(&option["label"]).map_or_else(|| format!("Option {}", position + 1), str::to_owned),"description":nonempty(&option["description"]).or_else(|| option["label"].as_str()).unwrap_or("")})).collect();
            json!({"id":nonempty(&question["id"]).map_or_else(|| format!("question-{}", index + 1), str::to_owned),"header":nonempty(&question["header"]).unwrap_or("Question"),"question":nonempty(&question["question"]).unwrap_or("Choose an answer."),"options":options})
        })
        .collect()
}
fn web_search_patterns(item: &Value) -> Vec<String> {
    let compact = |values: Vec<Option<&str>>| -> Vec<String> { values.into_iter().flatten().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned).collect() };
    let action = &item["action"];
    let query = item["query"].as_str();
    match action["type"].as_str() {
        _ if action.is_null() => compact(vec![query]),
        Some("search") => {
            let mut values: Vec<Option<&str>> = action["queries"].as_array().into_iter().flatten().map(|query| query.as_str()).collect();
            values.extend([action["query"].as_str(), query]);
            compact(values)
        }
        Some("openPage") => compact(vec![action["url"].as_str(), query]),
        Some("findInPage") => compact(vec![action["pattern"].as_str(), action["url"].as_str(), query]),
        _ => compact(vec![query]),
    }
}
fn mcp_output(item: &Value) -> Option<Value> {
    let result = (!item["result"].is_null()).then(|| if !item["result"]["structuredContent"].is_null() { item["result"]["structuredContent"].clone() } else { item["result"]["content"].clone() });
    match (item["error"]["message"].as_str().filter(|_| !item["error"].is_null()), result) {
        (None, result) => result.filter(|value| !value.is_null()),
        (Some(message), None) => Some(json!({"error":message})),
        (Some(message), Some(result)) => Some(json!({"error":message,"result":result})),
    }
}
fn dynamic_output(item: &Value) -> Option<Value> {
    if !item["contentItems"].is_null() {
        return Some(item["contentItems"].clone());
    }
    (item["success"] == false).then(|| json!({"success":false}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_progress_reads_attempt_and_maximum_from_the_message() {
        assert_eq!(retry_progress("Reconnecting... 2/5"), Some((2, 5)));
        assert_eq!(retry_progress("retry 3 / 10 after error"), Some((3, 10)));
        assert_eq!(retry_progress("no counter"), None);
        assert_eq!(retry_progress("0/5"), None);
    }

    #[test]
    fn web_search_patterns_follow_the_action() {
        let search = json!({"query":"q","action":{"type":"search","queries":["a","b"],"query":"q"}});
        assert_eq!(web_search_patterns(&search), ["a", "b", "q", "q"]);
        assert_eq!(web_search_patterns(&json!({"query":"only"})), ["only"]);
        assert_eq!(web_search_patterns(&json!({"query":"x","action":{"type":"openPage","url":"https://e"}})), ["https://e", "x"]);
    }

    #[test]
    fn mcp_and_dynamic_tool_output_follow_the_original() {
        assert_eq!(mcp_output(&json!({"result":{"structuredContent":{"a":1},"content":[]}})), Some(json!({"a":1})));
        assert_eq!(mcp_output(&json!({"result":{"content":["x"]}})), Some(json!(["x"])));
        assert_eq!(mcp_output(&json!({"error":{"message":"boom"}})), Some(json!({"error":"boom"})));
        assert_eq!(mcp_output(&json!({"result":{"content":["x"]},"error":{"message":"boom"}})), Some(json!({"error":"boom","result":["x"]})));
        assert_eq!(dynamic_output(&json!({"contentItems":[{"type":"inputText","text":"t"}]})), Some(json!([{"type":"inputText","text":"t"}])));
        assert_eq!(dynamic_output(&json!({"success":false})), Some(json!({"success":false})));
        assert_eq!(dynamic_output(&json!({"success":true})), None);
    }
}
