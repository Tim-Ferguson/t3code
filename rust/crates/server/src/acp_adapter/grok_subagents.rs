//! Subagents on the ACP adapter (Grok's, and Devin's through the same path): a `Task` or `spawn_subagent` call launches a subagent that runs in a
//! session of its own on the same connection. The parent shows a `subagent` node, row and timeline item
//! (see `subagents.rs`); the child's conversation lives in a child thread, with a provider thread once Grok
//! names the child's session. Updates of that session arrive interleaved with the parent's.
use super::*;
use crate::{subagents, xai_extension as xai};
use indexmap::IndexMap;

fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted" | "cancelled")
}
pub(super) struct Child {
    task: Value,
    child_thread_id: String,
    child_root_node_id: String,
    turn_item_id: String,
    turn_item_ordinal: u64,
    provider_turn_id: Value,
    parent_provider_thread_id: Value,
    child_session_id: Option<String>,
    assistant_text: String,
    assistant_messages: HashMap<String, String>,
    child_item_ordinals: HashMap<String, u64>,
    next_child_ordinal: u64,
    terminal_projected: bool,
    tools: HashMap<String, (crate::acp_tools::ToolState, String)>,
}
/// The subagents of the active turn, by the call that launched each, and the child sessions' updates that
/// arrived before the session was known.
#[derive(Default)]
pub(super) struct Children {
    by_task: IndexMap<String, Child>,
    by_session: HashMap<String, String>,
    pending: HashMap<String, Vec<Value>>,
}
impl Child {
    pub(super) fn task(&self) -> &Value {
        &self.task
    }
    pub(super) fn child_thread(&self) -> String {
        self.child_thread_id.clone()
    }
}
impl Children {
    /// Whether a subagent is still running: it holds the turn that launched it open.
    pub(super) fn blocks_settlement(&self) -> bool {
        self.by_task.values().any(|child| matches!(child.task["status"].as_str(), Some("running" | "pending")))
    }
    /// The subagent that runs in this child session.
    pub(super) fn by_child_session(&self, session: &str) -> Option<&Child> {
        self.by_session.get(session).and_then(|key| self.by_task.get(key))
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    fn find(&self, task: &str, session: Option<&str>) -> Option<String> {
        if self.by_task.contains_key(task) {
            return Some(task.to_owned());
        }
        session.and_then(|session| self.by_session.get(session)).cloned()
    }
}

impl Actor {
    fn grok_ids(&self, native: &str) -> (String, String, String) {
        let (item, node) = crate::acp_tools::provider_item_ids_for(self.flavor.driver(), &self.instance_id, native, self.item_identity_v2);
        let message = item.replacen("turn-item:", "message:", 1);
        (item, node, message)
    }
    /// `emitSubagent`: a launch, a poll of one, or the end of one.
    pub(super) fn emit_subagent(&mut self, update: &xai::SubagentUpdate) -> Result<(), StoreError> {
        let existing = self.children.find(&update.native_task_id, update.child_session_id.as_deref());
        let update_terminal = terminal(update.status);
        if let Some(key) = &existing {
            let child = &self.children.by_task[key];
            let status = child.task["status"].as_str().unwrap_or("running");
            if terminal(status) && !update_terminal {
                return Ok(());
            }
            if status == update.status && update_terminal && child.terminal_projected {
                return Ok(());
            }
        } else if (update.prompt.is_empty() && update.title.is_none()) || !update.suppress_normal_tool {
            // A poll of something that is not a known subagent (a monitor, a shell task) invents none.
            return Ok(());
        }
        let now = at(Utc::now());
        let view = projection(&self.store, &self.thread_id)?;
        let (run, attempt) = self.context(&view)?;
        let attempt = attempt.clone();
        let native_task = existing.as_ref().map(|key| self.children.by_task[key].task["nativeTaskRef"]["nativeId"].as_str().unwrap_or(&update.native_task_id).to_owned()).unwrap_or_else(|| update.native_task_id.clone());
        let native_ref = json!({"driver":self.flavor.driver(),"nativeId":native_task,"strength":"strong"});
        let (turn_item_id_new, node_id_new, _) = self.grok_ids(&native_task);
        let node_id = existing.as_ref().map(|key| self.children.by_task[key].task["id"].as_str().unwrap_or_default().to_owned()).unwrap_or(node_id_new);
        let native_session = self.session.setup.session_id.clone();
        let child_thread_id = existing.as_ref().map(|key| self.children.by_task[key].child_thread_id.clone()).unwrap_or_else(|| {
            let (_, id, _) = self.grok_ids(&format!("{native_session}:task:{native_task}"));
            id.replacen("node:", "thread:", 1)
        });
        let child_root_node_id = existing.as_ref().map(|key| self.children.by_task[key].child_root_node_id.clone()).unwrap_or_else(|| self.grok_ids(&format!("{native_task}:child-root")).1);
        let turn_item_id = existing.as_ref().map(|key| self.children.by_task[key].turn_item_id.clone()).unwrap_or(turn_item_id_new);
        let turn_item_ordinal = existing.as_ref().map(|key| self.children.by_task[key].turn_item_ordinal).unwrap_or_else(|| self.next_ordinal(&view, &run));
        let model = update.model.clone().filter(|model| !model.trim().is_empty()).or_else(|| existing.as_ref().and_then(|key| self.children.by_task[key].task["model"].as_str().map(str::to_owned)));
        let mut task = match &existing {
            Some(key) => self.children.by_task[key].task.clone(),
            None => json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"origin":"provider_native","createdBy":"agent","driver":self.flavor.driver(),"providerInstanceId":run["providerInstanceId"],"providerThreadId":null,"childThreadId":child_thread_id,"nativeTaskRef":native_ref,"prompt":update.prompt,"title":update.title,"result":null,"startedAt":now}),
        };
        task["model"] = json!(model);
        task["status"] = json!(update.status);
        task["result"] = update.result.clone().map(Value::from).unwrap_or_else(|| task.get("result").cloned().unwrap_or(Value::Null));
        task["completedAt"] = if terminal(update.status) { json!(now) } else { Value::Null };
        task["updatedAt"] = json!(now);
        let is_new = existing.is_none();
        let key = existing.clone().unwrap_or_else(|| native_task.clone());
        if is_new {
            self.children.by_task.insert(key.clone(), Child {
                task: task.clone(),
                child_thread_id: child_thread_id.clone(),
                child_root_node_id: child_root_node_id.clone(),
                turn_item_id: turn_item_id.clone(),
                turn_item_ordinal,
                provider_turn_id: attempt["providerTurnId"].clone(),
                parent_provider_thread_id: run["providerThreadId"].clone(),
                child_session_id: None,
                assistant_text: String::new(),
                assistant_messages: HashMap::new(),
                child_item_ordinals: HashMap::new(),
                next_child_ordinal: 101,
                terminal_projected: false,
                tools: HashMap::new(),
            });
            let mut selection = run["modelSelection"].clone();
            selection["model"] = json!(model.clone().unwrap_or_else(|| run["modelSelection"]["model"].as_str().unwrap_or_default().to_owned()));
            let title = subagents::thread_title(view["thread"]["title"].as_str().unwrap_or_default(), &json!(update.title), &update.prompt, self.children.by_task.len() as u64);
            let thread = subagents::child_thread(&view["thread"], &json!({"childThreadId":child_thread_id,"parentNodeId":node_id,"activeProviderThreadId":null,"providerInstanceId":run["providerInstanceId"],"modelSelection":selection,"title":title,"createdBy":"agent","creationSource":"provider"}), &now);
            subagents::ensure_child_thread(&self.store, &thread)?;
            let prompt_native = format!("{native_task}:prompt");
            let (prompt_item, _, prompt_message) = self.grok_ids(&prompt_native);
            let (message, item) = subagents::conversation_artifacts(&json!({"senderThreadId":self.thread_id,"messageId":prompt_message,"turnItemId":prompt_item,"threadId":child_thread_id,"rootNodeId":child_root_node_id,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":json!({"driver":self.flavor.driver(),"nativeId":prompt_native,"strength":"weak"}),"role":"user","text":update.prompt,"ordinal":100}), &now);
            commit(&self.store, &child_thread_id, |_| Ok(vec![("message.updated", message), ("turn-item.updated", item)]))?;
        }
        self.children.by_task[&key].task.clone_from(&task);
        // The child's session becomes known: its provider thread forks from the parent's turn, and what the
        // child said meanwhile is projected.
        if let Some(session) = update.child_session_id.as_deref() {
            if self.children.by_task[&key].child_session_id.is_none() {
                self.register_child_session(&key, session, &run, &attempt, &now)?;
            }
        }
        let (assistant_empty, shown) = { let child = &self.children.by_task[&key]; (child.assistant_text.is_empty(), child.assistant_text.clone()) };
        if update.status != "running" && assistant_empty {
            if let Some(result) = &update.result {
                self.emit_subagent_assistant(&key, result, false, None)?;
            }
        }
        let child = &self.children.by_task[&key];
        let result = update.result.clone().map(Value::from).or_else(|| Some(child.task["result"].clone()).filter(|result| !result.is_null())).unwrap_or_else(|| if shown.is_empty() { Value::Null } else { json!(shown) });
        let mut task = child.task.clone();
        task["status"] = json!(update.status);
        task["result"] = result.clone();
        task["completedAt"] = if terminal(update.status) { json!(now) } else { Value::Null };
        task["updatedAt"] = json!(now);
        let provider_thread_id = task["providerThreadId"].clone();
        let node = json!({"id":node_id,"threadId":self.thread_id,"runId":task["runId"],"parentNodeId":task["parentNodeId"],"rootNodeId":task["parentNodeId"],"kind":"subagent","status":update.status,"countsForRun":false,"providerThreadId":child.parent_provider_thread_id,"providerTurnId":child.provider_turn_id,"nativeItemRef":native_ref,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":task["startedAt"],"completedAt":task["completedAt"]});
        let root = json!({"id":child_root_node_id,"threadId":child_thread_id,"runId":null,"parentNodeId":null,"rootNodeId":child_root_node_id,"kind":"root_turn","status":update.status,"countsForRun":false,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":native_ref,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":task["startedAt"],"completedAt":task["completedAt"]});
        let item = json!({"id":turn_item_id,"threadId":self.thread_id,"runId":task["runId"],"nodeId":node_id,"providerThreadId":child.parent_provider_thread_id,"providerTurnId":child.provider_turn_id,"nativeItemRef":native_ref,"parentItemId":null,"ordinal":turn_item_ordinal,"status":update.status,"title":task["title"],"startedAt":task["startedAt"],"completedAt":task["completedAt"],"updatedAt":now,"type":"subagent","subagentId":task["id"],"origin":"provider_native","driver":self.flavor.driver(),"providerInstanceId":run["providerInstanceId"],"childThreadId":child_thread_id,"prompt":task["prompt"],"result":result});
        commit(&self.store, &child_thread_id, |_| Ok(vec![("node.updated", root)]))?;
        commit(&self.store, &self.thread_id, |_| Ok(vec![("node.updated", node), ("subagent.updated", task.clone()), ("turn-item.updated", item)]))?;
        let child = self.children.by_task.get_mut(&key).unwrap();
        child.task = task;
        child.terminal_projected |= terminal(update.status);
        Ok(())
    }
    fn register_child_session(&mut self, key: &str, session: &str, run: &Value, attempt: &Value, now: &str) -> Result<(), StoreError> {
        let (child_thread_id, instance) = (self.children.by_task[key].child_thread_id.clone(), run["providerInstanceId"].clone());
        let (_, provider_thread_id, _) = self.grok_ids(&format!("{child_thread_id}:provider-thread"));
        let provider_thread_id = provider_thread_id.replacen("node:", "provider-thread:", 1);
        let row = json!({"id":provider_thread_id,"driver":self.flavor.driver(),"providerInstanceId":instance,"providerSessionId":self.session_id,"appThreadId":child_thread_id,"ownerNodeId":null,"nativeThreadRef":native_ref(self.flavor.driver(), session, true),"nativeConversationHeadRef":null,"status":"idle","firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":{"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"]},"pendingBackgroundTasks":[],"contextUsage":null,"nativeMetadata":if self.item_identity_v2{json!({"itemIdentityVersion":2})}else{Value::Null},"goal":null,"createdAt":now,"updatedAt":now});
        {
            let child = self.children.by_task.get_mut(key).unwrap();
            child.child_session_id = Some(session.to_owned());
            child.task["providerThreadId"] = json!(provider_thread_id);
        }
        self.children.by_session.insert(session.to_owned(), key.to_owned());
        commit(&self.store, &child_thread_id, |_| Ok(vec![("provider-thread.updated", row.clone())]))?;
        commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-thread.updated", row.clone())]))?;
        for notification in self.children.pending.remove(session).unwrap_or_default() {
            self.project_child_message(key, &notification)?;
        }
        Ok(())
    }
    /// `emitSubagentAssistant`: what the child says, in the child thread, one message per Grok message id.
    fn emit_subagent_assistant(&mut self, key: &str, text: &str, replace: bool, message_id: Option<&str>) -> Result<(), StoreError> {
        if text.is_empty() && !replace {
            return Ok(());
        }
        let native_item = {
            let child = &self.children.by_task[key];
            format!("{}:message:{}", child.task["nativeTaskRef"]["nativeId"].as_str().unwrap_or_default(), message_id.unwrap_or("result"))
        };
        let (item_id, _, message_id) = self.grok_ids(&native_item);
        let now = at(Utc::now());
        let driver = self.flavor.driver();
        let (artifacts, thread) = {
            let child = self.children.by_task.get_mut(key).unwrap();
            let previous = child.assistant_messages.get(&native_item).cloned().unwrap_or_default();
            let message_text = if replace { text.to_owned() } else { format!("{previous}{text}") };
            child.assistant_messages.insert(native_item.clone(), message_text.clone());
            child.assistant_text = message_text.clone();
            let ordinal = match child.child_item_ordinals.get(&native_item) {
                Some(ordinal) => *ordinal,
                None => {
                    let ordinal = child.next_child_ordinal;
                    child.next_child_ordinal += 1;
                    child.child_item_ordinals.insert(native_item.clone(), ordinal);
                    ordinal
                }
            };
            let artifacts = subagents::conversation_artifacts(&json!({"messageId":message_id,"turnItemId":item_id,"threadId":child.child_thread_id,"rootNodeId":child.child_root_node_id,"providerThreadId":child.task["providerThreadId"],"providerTurnId":null,"nativeItemRef":{"driver":driver,"nativeId":native_item,"strength":"weak"},"role":"assistant","text":message_text,"ordinal":ordinal}), &now);
            (artifacts, child.child_thread_id.clone())
        };
        let (message, item) = artifacts;
        commit(&self.store, &thread, |_| Ok(vec![("message.updated", message), ("turn-item.updated", item)]))
    }
    fn project_child_message(&mut self, key: &str, notification: &Value) -> Result<(), StoreError> {
        let update = &notification["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(text) = crate::acp_model::content_display_text(&update["content"]) {
                    self.emit_subagent_assistant(key, &text, false, update["messageId"].as_str())?;
                }
            }
            Some("agent_message") => {
                if let Some(contents) = update["content"].as_array() {
                    let text = contents.iter().filter_map(crate::acp_model::content_display_text).collect::<Vec<_>>().join("\n");
                    self.emit_subagent_assistant(key, &text, true, update["messageId"].as_str())?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// An update of a session other than the root's: a subagent's tool call or assistant text.
    pub(super) fn child_update(&mut self, notification: &Value) -> Result<(), StoreError> {
        if self.active_run.is_none() {
            return Ok(());
        }
        let session = notification["sessionId"].as_str().unwrap_or_default().to_owned();
        let update = &notification["update"];
        let key = self.children.by_session.get(&session).cloned();
        match update["sessionUpdate"].as_str() {
            Some("tool_call" | "tool_call_update") => {
                let Some(key) = key else { return Ok(()) };
                let Some(parsed) = crate::acp_tools::ToolState::parse(update, update["sessionUpdate"] == "tool_call") else { return Ok(()) };
                let parsed = self.flavor.normalize_tool(parsed);
                if let Some(subagent) = self.subagent_update(&parsed.wire) {
                    let native_task = self.children.by_task[&key].task["nativeTaskRef"]["nativeId"].as_str().unwrap_or_default().to_owned();
                    if subagent.native_task_id == native_task || subagent.child_session_id.as_deref() == Some(session.as_str()) {
                        self.emit_subagent(&subagent)?;
                    }
                    return Ok(());
                }
                self.project_child_tool(&key, parsed)
            }
            Some("agent_message_chunk") if crate::acp_model::content_display_text(&update["content"]).is_some() => self.route_child_message(key, notification),
            Some("agent_message") if update["content"].is_array() => self.route_child_message(key, notification),
            _ => Ok(()),
        }
    }
    fn route_child_message(&mut self, key: Option<String>, notification: &Value) -> Result<(), StoreError> {
        match key {
            Some(key) => self.project_child_message(&key, notification),
            None => {
                let session = notification["sessionId"].as_str().unwrap_or_default().to_owned();
                self.children.pending.entry(session).or_default().push(notification.clone());
                Ok(())
            }
        }
    }
    fn project_child_tool(&mut self, key: &str, incoming: crate::acp_tools::ToolState) -> Result<(), StoreError> {
        let tool_id = incoming.wire["toolCallId"].as_str().unwrap_or_default().to_owned();
        let tool_key = format!("{}:tool:{tool_id}", self.children.by_task[key].task["nativeTaskRef"]["nativeId"].as_str().unwrap_or_default());
        let now = at(Utc::now());
        let (item_id, _, _) = self.grok_ids(&tool_key);
        let driver = self.flavor.driver();
        let child = self.children.by_task.get_mut(key).unwrap();
        let previous = child.tools.get(&tool_key).map(|(tool, _)| tool);
        let merged = crate::acp_tools::ToolState::merge(previous, incoming);
        let started = child.tools.get(&tool_key).map(|(_, started)| started.clone()).unwrap_or_else(|| now.clone());
        child.tools.insert(tool_key.clone(), (merged.clone(), started.clone()));
        let ordinal = match child.child_item_ordinals.get(&tool_key) {
            Some(ordinal) => *ordinal,
            None => {
                let ordinal = child.next_child_ordinal;
                child.next_child_ordinal += 1;
                child.child_item_ordinals.insert(tool_key.clone(), ordinal);
                ordinal
            }
        };
        let status = match merged.wire["status"].as_str() {
            Some("pending") => "pending",
            Some("completed") => "completed",
            Some("failed") => "failed",
            Some("requiresAction") => "waiting",
            _ => "running",
        };
        let data = &merged.wire["data"];
        let name = merged.wire["title"].as_str().or_else(|| merged.wire["kind"].as_str()).unwrap_or("Tool").to_owned();
        let item = json!({"id":item_id,"threadId":child.child_thread_id,"runId":null,"nodeId":child.child_root_node_id,"providerThreadId":child.task["providerThreadId"],"providerTurnId":null,"nativeItemRef":{"driver":driver,"nativeId":tool_key,"strength":"strong"},"parentItemId":null,"ordinal":ordinal,"status":status,"title":name,"startedAt":started,"completedAt":if matches!(status,"completed"|"failed"){json!(now)}else{Value::Null},"updatedAt":now,"type":"dynamic_tool","toolName":name,"input":data.get("rawInput").cloned().unwrap_or(Value::Null),"output":data.get("rawOutput").or_else(|| data.get("content")).cloned().unwrap_or(Value::Null)});
        let thread = child.child_thread_id.clone();
        commit(&self.store, &thread, |_| Ok(vec![("turn-item.updated", item)]))
    }
}
