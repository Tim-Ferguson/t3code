//! The Codex adapter's half of subagents: the `collabAgentToolCall` items of the v1 collaboration
//! protocol, the `subAgentActivity` items of v2, and the native threads they name
//! (`registerSubagent*`, `emitSubagent*` of `CodexAdapterV2.ts`). Every native subagent thread is a
//! child app thread of the thread that spawned it, and nested subagents are children of children.
//!
//! The actor owns one [`Subagents`]. A child keeps its own [`Projector`], bound to the child thread,
//! so what the child does (messages, tools, plans, usage) is projected exactly as the top-level
//! turn's is, but into the child thread and without a run. What the parent shows of a child is a
//! `subagent` node, a `Subagent` row and a `subagent` timeline item, kept by [`Subagents::task_update`].
//! A child's approval and question are asked on the top-level thread, under the subagent that
//! asked, and answered there; the actor routes the answer back to the child's request.
use crate::{
    claude_projection::enc,
    codex_projection::{self as projection_ids, Projector},
    codex_runtime::{commit, find, projection},
    execution::at,
    persistence::{Store, StoreError},
    provider_process::ProviderProcess,
    provider_turn_context::TurnContext,
    subagents,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

const DRIVER: &str = "codex";

/// What the subagents need of the actor to write and to ask Codex.
pub(crate) struct Host<'a> {
    pub store: &'a Store,
    pub session_id: &'a str,
    /// The app-server; none in a test that has none to ask.
    pub process: Option<&'a ProviderProcess>,
}

/// A running native turn: the top-level turn or a subagent's. `ActiveCodexTurnContext` of the original,
/// without the projection state, which the turn's projector keeps.
#[derive(Clone)]
pub(crate) struct Scope {
    /// The app thread the turn's rows belong to, and the run it belongs to (none for a subagent).
    pub thread_id: String,
    pub run_id: Value,
    pub native_thread: String,
    pub native_turn: String,
    pub provider_thread: Value,
    pub provider_turn_id: Value,
    pub provider_turn_ordinal: u64,
    /// Where the turn's items hang, and the root of its node tree.
    pub item_parent_node_id: Value,
    pub root_node_id: Value,
    pub app_thread: Value,
    /// The model selection of the top-level run, which every subagent inherits.
    pub model_selection: Value,
    /// The native thread of the subagent this turn belongs to; none for the top-level turn.
    pub subagent: Option<String>,
}

struct Child {
    parent_native_thread: String,
    /// The turn of the parent that spawned the child, or the latest turn of the parent since.
    parent: Scope,
    provider_thread: Value,
    child_thread: Value,
    subagent_node_id: String,
    child_root_node_id: String,
    child_thread_id: String,
    native_turn_ids: HashSet<String>,
    started_at: String,
    turn_item_id: String,
    turn_item_ordinal: u64,
    task: Value,
    next_turn_ordinal: u64,
    projector: Projector,
    /// The turn the child is running, if any.
    active: Option<Scope>,
}

#[derive(Default)]
pub(crate) struct Subagents {
    children: HashMap<String, Child>,
    /// Turns of a thread that started before its spawn was reported, by native thread.
    pending_turns: HashMap<String, Vec<(String, String)>>,
    /// The model each native thread was last reported to run.
    models: HashMap<String, String>,
    /// Native turns that ended without completing; subagent turns that start under them are stopped.
    pub(crate) terminalized: HashSet<String>,
    /// Native turns Stop has been sent to: they may still report as completed.
    interrupting: HashSet<String>,
    /// Models read from Codex in the background, applied at the next notification.
    inbox: Arc<Mutex<Vec<(String, String)>>>,
}

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}
fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}
/// The next free item position of a thread, as the original's position store allocates them.
fn next_ordinal(view: &Value) -> u64 {
    rows(view, "turnItems").iter().filter_map(|item| item["ordinal"].as_u64()).max().map_or(0, |max| max + 1)
}
fn provider_turn_id(native_turn: &str) -> String {
    format!("provider-turn:provider:{DRIVER}:native-turn:{}", enc(native_turn))
}
fn provider_thread_id(native_thread: &str) -> String {
    format!("provider-thread:provider:{DRIVER}:native-thread:{}", enc(native_thread))
}
fn thread_id(native_thread: &str) -> String {
    format!("thread:provider:{DRIVER}:native-thread:{}", enc(native_thread))
}
fn timestamp(seconds: &Value) -> String {
    seconds.as_i64().and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0)).map_or_else(|| at(Utc::now()), at)
}
/// The state a subagent reports in `agentsStates`, as the `Subagent` status.
fn collab_status(state: &str) -> &'static str {
    match state {
        "pendingInit" => "pending",
        "running" => "running",
        "interrupted" => "interrupted",
        "completed" => "completed",
        "shutdown" => "cancelled",
        _ => "failed",
    }
}

impl Subagents {
    pub(crate) fn has_pending(&self) -> bool {
        self.children.values().any(|child| child.projector.has_pending())
    }
    /// The short flush timer: what the children streamed since the last write.
    pub(crate) fn flush(&mut self, store: &Store) -> Result<(), StoreError> {
        for child in self.children.values_mut() {
            if child.active.is_some() && child.projector.has_pending() {
                let projector = &mut child.projector;
                commit(store, &child.child_thread_id, |view| projector.flush(view))?;
            }
        }
        Ok(())
    }
    pub(crate) fn mark_interrupting(&mut self, native_turn: &str) {
        self.interrupting.insert(native_turn.to_owned());
    }
    /// The turns of subagents that are running, deepest last: what Stop on the top-level turn also stops.
    pub(crate) fn active_turns(&self) -> Vec<(String, String)> {
        self.children.values().filter_map(|child| child.active.as_ref().map(|scope| (scope.native_thread.clone(), scope.native_turn.clone()))).collect()
    }
    /// The subagent turn on this native thread that is running, if it is `turn`.
    pub(crate) fn scope_of(&self, native_thread: &str, native_turn: &str) -> Option<Scope> {
        self.children.get(native_thread)?.active.clone().filter(|scope| scope.native_turn == native_turn)
    }
    /// A running subagent turn of the thread, whichever it is.
    pub(crate) fn active_scope(&self, native_thread: &str) -> Option<Scope> {
        self.children.get(native_thread)?.active.clone()
    }
    /// The subagent node directly under the top-level turn that a request from this native thread
    /// belongs to: `Some(None)` for the top-level thread itself, `None` for a thread nobody spawned.
    pub(crate) fn request_owner(&self, native_thread: &str, root_native_thread: Option<&str>) -> Option<Option<String>> {
        let mut thread = native_thread;
        let mut top = None;
        for _ in 0..64 {
            if Some(thread) == root_native_thread {
                return Some(top);
            }
            let child = self.children.get(thread)?;
            top = Some(child.subagent_node_id.clone());
            thread = &child.parent_native_thread;
        }
        None
    }
    /// The turns above this scope, itself included, as native turn ids.
    fn lineage(&self, scope: &Scope) -> Vec<String> {
        let mut turns = vec![scope.native_turn.clone()];
        let mut current = scope.subagent.clone();
        while let Some(native) = current {
            let Some(child) = self.children.get(&native) else { break };
            turns.push(child.parent.native_turn.clone());
            current = child.parent.subagent.clone();
        }
        turns
    }
    /// `markSubagentUsage` on a child's turn; the top-level turn's is the actor's.
    pub(crate) fn mark_usage(&mut self, scope: &Scope) {
        if let Some(child) = scope.subagent.as_ref().and_then(|native| self.children.get_mut(native)) {
            child.projector.mark_subagents();
        }
    }

    // --- the parent's view of a child ---------------------------------------------------------
    /// `emitSubagentTaskUpdate`: the `Subagent` row and its timeline item. A settled subagent is
    /// only reopened by a new turn.
    fn task_update(&mut self, store: &Store, native: &str, status: &str, result: Option<Value>, completed_at: Option<&str>, started_at: Option<&str>, reopen: bool) -> Result<(), StoreError> {
        let Some(child) = self.children.get_mut(native) else { return Ok(()) };
        let now = at(Utc::now());
        let prior = child.task.clone();
        let settled = !crate::background_work::work_active(&prior["status"]);
        if settled && crate::background_work::work_active(&json!(status)) && !reopen {
            return Ok(());
        }
        let status = if settled && !reopen { prior["status"].as_str().unwrap_or(status).to_owned() } else { status.to_owned() };
        let completed = if crate::background_work::work_active(&json!(status)) {
            Value::Null
        } else if !prior["completedAt"].is_null() {
            prior["completedAt"].clone()
        } else {
            json!(completed_at.unwrap_or(&now))
        };
        let mut task = prior.clone();
        task["status"] = json!(status);
        if reopen {
            task["runId"] = child.parent.run_id.clone();
            task["parentNodeId"] = child.parent.item_parent_node_id.clone();
        }
        task["startedAt"] = json!(started_at.map_or_else(|| prior["startedAt"].as_str().unwrap_or(&now).to_owned(), str::to_owned));
        if let Some(result) = result {
            task["result"] = result;
        }
        task["completedAt"] = completed;
        task["updatedAt"] = json!(now);
        child.task = task.clone();
        if let Some(started_at) = started_at {
            child.started_at = started_at.to_owned();
        }
        let item = json!({"id":child.turn_item_id,"threadId":task["threadId"],"runId":task["runId"],"nodeId":task["id"],"providerThreadId":task["providerThreadId"],"providerTurnId":child.parent.provider_turn_id,"nativeItemRef":task["nativeTaskRef"],"parentItemId":null,"ordinal":child.turn_item_ordinal,"status":task["status"],"title":task["title"],"startedAt":task["startedAt"],"completedAt":task["completedAt"],"updatedAt":task["updatedAt"],"type":"subagent","subagentId":task["id"],"origin":task["origin"],"driver":task["driver"],"providerInstanceId":task["providerInstanceId"],"childThreadId":task["childThreadId"],"prompt":task["prompt"],"result":task["result"]});
        let parent_thread = task["threadId"].as_str().unwrap_or_default().to_owned();
        commit(store, &parent_thread, |_| Ok(vec![("subagent.updated", task.clone()), ("turn-item.updated", item.clone())]))?;
        self.sync_thread_model(store, native)
    }

    /// A native subagent's thread starts on the parent's model; when the provider names the real
    /// one, the thread moves to it (`syncSubagentThreadModel`).
    fn sync_thread_model(&self, store: &Store, native: &str) -> Result<(), StoreError> {
        let Some(child) = self.children.get(native) else { return Ok(()) };
        let Some(model) = text(&child.task["model"]) else { return Ok(()) };
        let Some(view) = store.projection("thread", &child.child_thread_id)? else { return Ok(()) };
        if view["thread"]["modelSelection"]["model"] == model {
            return Ok(());
        }
        let mut thread = view["thread"].clone();
        thread["modelSelection"] = json!({"instanceId":thread["modelSelection"]["instanceId"],"model":model});
        thread["updatedAt"] = json!(at(Utc::now()));
        commit(store, &child.child_thread_id, |_| Ok(vec![("thread.model-selection-updated", thread.clone())]))
    }

    /// `updateSubagentModel`: Codex named the model a thread runs.
    pub(crate) fn update_model(&mut self, store: &Store, native: &str, model: &Value) -> Result<(), StoreError> {
        let Some(model) = model.as_str().map(t3_contracts::trim_wire_string).filter(|model| !model.is_empty()) else { return Ok(()) };
        self.models.insert(native.to_owned(), model.to_owned());
        let Some(child) = self.children.get_mut(native) else { return Ok(()) };
        if child.task["model"] == model {
            return Ok(());
        }
        child.task["model"] = json!(model);
        child.task["updatedAt"] = json!(at(Utc::now()));
        let task = child.task.clone();
        let parent_thread = task["threadId"].as_str().unwrap_or_default().to_owned();
        commit(store, &parent_thread, |_| Ok(vec![("subagent.updated", task.clone())]))?;
        self.sync_thread_model(store, native)
    }
    /// Models the background reads found, applied now.
    pub(crate) fn apply_models(&mut self, store: &Store) -> Result<(), StoreError> {
        let found: Vec<(String, String)> = std::mem::take(&mut *self.inbox.lock().unwrap());
        for (native, model) in found {
            if !self.models.contains_key(&native) {
                self.update_model(store, &native, &json!(model))?;
            }
        }
        Ok(())
    }

    // --- spawning -----------------------------------------------------------------------------
    /// `registerSubagentThread`: the child thread, its nodes, provider thread and prompt.
    #[allow(clippy::too_many_arguments)]
    fn register_thread(&mut self, host: &Host, parent: &Scope, native_thread: &str, native_item: &str, native_tool_call: &str, prompt: &str, title: Option<&str>, model: Option<&str>, ordinal: u64, emit_initial_prompt: bool) -> Result<(), StoreError> {
        if self.children.contains_key(native_thread) {
            return Ok(());
        }
        let now = at(Utc::now());
        let subagent_node = projection_ids::node_id(native_item);
        let child_root_node = projection_ids::node_id(&format!("{native_item}:thread-root"));
        let child_thread_id = thread_id(native_thread);
        let parent_view = projection(host.store, &parent.thread_id)?;
        let turn_item_ordinal = next_ordinal(&parent_view);
        let instance = parent.model_selection["instanceId"].clone();
        let provider_thread = json!({
            "id": provider_thread_id(native_thread), "driver": DRIVER, "providerInstanceId": instance, "providerSessionId": host.session_id,
            "appThreadId": child_thread_id, "ownerNodeId": null, "nativeThreadRef": projection_ids::native_ref(native_thread), "nativeConversationHeadRef": null,
            "status": "idle", "firstRunOrdinal": null, "lastRunOrdinal": null, "handoffIds": [],
            "forkedFrom": {"providerThreadId": parent.provider_thread["id"], "providerTurnId": parent.provider_turn_id},
            "pendingBackgroundTasks": [], "contextUsage": null, "nativeMetadata": null, "goal": null,
            "createdAt": now, "updatedAt": now,
        });
        let model = self.models.get(native_thread).cloned().or_else(|| model.map(str::to_owned));
        let task = json!({
            "id": subagent_node, "threadId": parent.thread_id, "runId": parent.run_id, "parentNodeId": parent.item_parent_node_id,
            "origin": "provider_native", "createdBy": "agent", "driver": DRIVER, "providerInstanceId": instance,
            "providerThreadId": provider_thread["id"], "childThreadId": child_thread_id, "nativeTaskRef": projection_ids::native_ref(native_item),
            "prompt": prompt, "title": title, "model": model, "status": "running", "result": null,
            "startedAt": now, "completedAt": null, "updatedAt": now,
        });
        let mut selection = parent.model_selection.clone();
        selection["model"] = model.as_deref().map_or_else(|| parent.model_selection["model"].clone(), |model| json!(model));
        let thread_title = subagents::thread_title(parent.app_thread["title"].as_str().unwrap_or_default(), &json!(title), prompt, ordinal);
        let child_thread = subagents::child_thread(
            &parent.app_thread,
            &json!({"childThreadId":child_thread_id,"parentNodeId":subagent_node,"activeProviderThreadId":provider_thread["id"],"providerInstanceId":instance,"modelSelection":selection,"title":thread_title,"createdBy":"agent","creationSource":"provider"}),
            &now,
        );
        let child = Child {
            parent_native_thread: parent.native_thread.clone(),
            parent: parent.clone(),
            provider_thread: provider_thread.clone(),
            child_thread: child_thread.clone(),
            subagent_node_id: subagent_node.clone(),
            child_root_node_id: child_root_node.clone(),
            child_thread_id: child_thread_id.clone(),
            native_turn_ids: HashSet::new(),
            started_at: now.clone(),
            turn_item_id: projection_ids::item_id(native_item),
            turn_item_ordinal,
            task: task.clone(),
            next_turn_ordinal: 0,
            projector: Projector::for_child(&child_thread_id, host.session_id),
            active: None,
        };
        self.children.insert(native_thread.to_owned(), child);
        subagents::ensure_child_thread(host.store, &child_thread)?;
        let subagent_node_row = json!({"id":subagent_node,"threadId":parent.thread_id,"runId":parent.run_id,"parentNodeId":parent.item_parent_node_id,"rootNodeId":parent.root_node_id,"kind":"subagent","status":"running","countsForRun":false,"providerThreadId":provider_thread["id"],"providerTurnId":parent.provider_turn_id,"nativeItemRef":projection_ids::native_ref(native_tool_call),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        commit(host.store, &parent.thread_id, |_| Ok(vec![("node.updated", subagent_node_row.clone())]))?;
        let root_row = json!({"id":child_root_node,"threadId":child_thread_id,"runId":null,"parentNodeId":null,"rootNodeId":child_root_node,"kind":"root_turn","status":"running","countsForRun":false,"providerThreadId":provider_thread["id"],"providerTurnId":null,"nativeItemRef":projection_ids::native_ref(native_item),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
        commit(host.store, &child_thread_id, |_| Ok(vec![("node.updated", root_row.clone())]))?;
        self.write_provider_thread(host.store, native_thread, provider_thread.clone())?;
        if emit_initial_prompt && !prompt.is_empty() {
            let prompt_item = format!("{native_item}:prompt");
            let (message, item) = subagents::conversation_artifacts(
                &json!({"senderThreadId":parent.thread_id,"messageId":projection_ids::message_id(&prompt_item),"turnItemId":projection_ids::item_id(&prompt_item),"threadId":child_thread_id,"rootNodeId":child_root_node,"providerThreadId":provider_thread["id"],"providerTurnId":null,"nativeItemRef":projection_ids::native_ref(&prompt_item),"role":"user","text":prompt,"ordinal":0}),
                &now,
            );
            commit(host.store, &child_thread_id, |_| Ok(vec![("message.updated", message.clone()), ("turn-item.updated", item.clone())]))?;
        }
        self.task_update(host.store, native_thread, "running", None, None, None, false)?;
        for (turn, started) in self.pending_turns.remove(native_thread).unwrap_or_default() {
            self.turn_started_in(host, native_thread, &turn, &started, None)?;
        }
        if task["model"].is_null() {
            self.read_model(host, native_thread);
        }
        Ok(())
    }

    /// A child's provider thread lives in the child thread, and in the thread that holds the
    /// child's `Subagent` row, which lists the provider threads its subagents run.
    fn write_provider_thread(&mut self, store: &Store, native: &str, row: Value) -> Result<(), StoreError> {
        let Some(child) = self.children.get_mut(native) else { return Ok(()) };
        child.provider_thread = row.clone();
        commit(store, &child.child_thread_id, |_| Ok(vec![("provider-thread.updated", row.clone())]))?;
        commit(store, &child.parent.thread_id, |_| Ok(vec![("provider-thread.updated", row.clone())]))
    }

    /// The model of a subagent the spawn did not name, read from Codex in the background.
    fn read_model(&self, host: &Host, native: &str) {
        let Some(process) = host.process.cloned() else { return };
        let (inbox, native) = (self.inbox.clone(), native.to_owned());
        tokio::spawn(async move {
            let found = tokio::time::timeout(Duration::from_secs(5), async {
                let read = process.request("thread/read", json!({"threadId":native,"includeTurns":false}), Duration::from_secs(5)).await.ok().filter(|response| response["thread"]["id"] == native.as_str());
                if let Some(model) = read.as_ref().and_then(|response| response["thread"]["model"].as_str()).map(str::trim).filter(|model| !model.is_empty()) {
                    return Some(model.to_owned());
                }
                let resume = process.request("thread/resume", json!({"threadId":native,"excludeTurns":true}), Duration::from_secs(5)).await.ok().filter(|response| response["thread"]["id"] == native.as_str())?;
                resume["model"].as_str().map(str::trim).filter(|model| !model.is_empty()).map(str::to_owned)
            })
            .await;
            if let Ok(Some(model)) = found {
                inbox.lock().unwrap().push((native, model));
            }
        });
    }

    /// `registerSubagentThreads`: the threads a v1 `spawnAgent` call started.
    pub(crate) fn register_spawned(&mut self, host: &Host, parent: &Scope, item: &Value) -> Result<(), StoreError> {
        if item["tool"] != "spawnAgent" {
            return Ok(());
        }
        let model = text(&item["model"]).map(str::to_owned);
        for (index, receiver) in item["receiverThreadIds"].as_array().into_iter().flatten().filter_map(Value::as_str).enumerate() {
            let call = item["id"].as_str().unwrap_or_default();
            self.register_thread(host, parent, receiver, &format!("{call}:{receiver}"), call, item["prompt"].as_str().unwrap_or(""), None, model.as_deref(), index as u64 + 1, true)?;
        }
        Ok(())
    }

    /// `registerSubagentActivity`: a v2 subagent started, or finished or was interrupted.
    pub(crate) fn activity(&mut self, host: &Host, context: Option<&Scope>, item: &Value) -> Result<(), StoreError> {
        let Some(agent) = item["agentThreadId"].as_str() else { return Ok(()) };
        let kind = item["kind"].as_str().unwrap_or_default();
        if kind == "started" {
            let Some(parent) = context else { return Ok(()) };
            let ordinal = self.children.values().filter(|child| child.parent.root_node_id == parent.root_node_id).count() as u64 + 1;
            let call = item["id"].as_str().unwrap_or_default();
            return self.register_thread(host, parent, agent, &format!("{call}:{agent}"), call, "", item["agentPath"].as_str(), None, ordinal, false);
        }
        if matches!(kind, "interrupted" | "completed") && self.children.contains_key(agent) {
            self.task_update(host.store, agent, kind, None, None, None, false)?;
        }
        Ok(())
    }

    /// `updateSubagentStates`: the states a v1 call reports its subagents in.
    pub(crate) fn update_states(&mut self, host: &Host, item: &Value) -> Result<(), StoreError> {
        for (native, state) in item["agentsStates"].as_object().into_iter().flatten() {
            if !self.children.contains_key(native) {
                continue;
            }
            let result = (!state["message"].is_null()).then(|| state["message"].clone());
            self.task_update(host.store, native, collab_status(state["status"].as_str().unwrap_or_default()), result, None, None, false)?;
        }
        Ok(())
    }

    // --- a child's turns ----------------------------------------------------------------------
    /// `rememberSubagentTurnStarted`: a turn started on a thread; it belongs to a subagent once that
    /// is known.
    pub(crate) fn turn_started(&mut self, host: &Host, root: Option<&Scope>, native_thread: &str, native_turn: &str, started_at: &str) -> Result<(), StoreError> {
        if self.children.contains_key(native_thread) {
            return self.turn_started_in(host, native_thread, native_turn, started_at, root);
        }
        self.pending_turns.entry(native_thread.to_owned()).or_default().push((native_turn.to_owned(), started_at.to_owned()));
        Ok(())
    }

    /// `emitSubagentProviderTurnStarted`
    fn turn_started_in(&mut self, host: &Host, native_thread: &str, native_turn: &str, started_at: &str, root: Option<&Scope>) -> Result<(), StoreError> {
        let Some(child) = self.children.get_mut(native_thread) else { return Ok(()) };
        if !child.native_turn_ids.insert(native_turn.to_owned()) {
            return Ok(());
        }
        // The parent may have moved on to a newer turn since the spawn.
        let parent_thread = child.parent_native_thread.clone();
        let current_parent = match root {
            Some(root) if root.native_thread == parent_thread => Some(root.clone()),
            _ => self.children.get(&parent_thread).and_then(|parent| parent.active.clone()),
        };
        if let Some(parent) = current_parent {
            self.children.get_mut(native_thread).unwrap().parent = parent;
        }
        let parent = self.children[native_thread].parent.clone();
        // A subagent that starts a turn under a stopped turn is stopped at once.
        if self.lineage(&parent).iter().any(|turn| self.terminalized.contains(turn)) {
            if let Some(process) = host.process.cloned() {
                let (thread, turn) = (native_thread.to_owned(), native_turn.to_owned());
                tokio::spawn(async move {
                    if let Err(error) = process.request("turn/interrupt", json!({"threadId":thread,"turnId":turn}), Duration::from_secs(10)).await {
                        tracing::warn!(%error, "Could not stop a Codex subagent that started under a stopped turn");
                    }
                });
            }
            return Ok(());
        }
        let child = self.children.get_mut(native_thread).unwrap();
        let ordinal = child.next_turn_ordinal.max(1);
        child.next_turn_ordinal = ordinal + 1;
        let provider_turn = provider_turn_id(native_turn);
        let node_id = if ordinal == 1 { child.child_root_node_id.clone() } else { projection_ids::node_id(&format!("{native_turn}:thread-root")) };
        let scope = Scope {
            thread_id: child.child_thread_id.clone(),
            run_id: Value::Null,
            native_thread: native_thread.to_owned(),
            native_turn: native_turn.to_owned(),
            provider_thread: child.provider_thread.clone(),
            provider_turn_id: json!(provider_turn),
            provider_turn_ordinal: ordinal,
            item_parent_node_id: json!(node_id),
            root_node_id: json!(node_id),
            app_thread: child.child_thread.clone(),
            model_selection: parent.model_selection.clone(),
            subagent: Some(native_thread.to_owned()),
        };
        child.projector.begin(TurnContext { run_id: String::new(), attempt_id: Value::Null, root_node_id: json!(node_id), provider_thread_id: child.provider_thread["id"].clone(), provider_turn_id: json!(provider_turn), provider_instance_id: scope.model_selection["instanceId"].clone() }, native_turn);
        child.active = Some(scope);
        let reopen = ordinal > 1;
        self.task_update(host.store, native_thread, "running", reopen.then_some(Value::Null), None, Some(started_at), reopen)?;
        let child = self.children.get(native_thread).unwrap();
        let mut thread_row = child.provider_thread.clone();
        thread_row["status"] = json!("active");
        thread_row["updatedAt"] = json!(at(Utc::now()));
        let (child_thread, child_node, native_ref, provider_thread_row) = (child.child_thread_id.clone(), node_id.clone(), child.task["nativeTaskRef"].clone(), child.provider_thread["id"].clone());
        self.write_provider_thread(host.store, native_thread, thread_row)?;
        let turn_row = json!({"id":provider_turn,"providerThreadId":provider_thread_row,"nodeId":child_node,"runAttemptId":null,"nativeTurnRef":projection_ids::native_ref(native_turn),"ordinal":ordinal,"status":"running","startedAt":started_at,"completedAt":null});
        let node_row = json!({"id":child_node,"threadId":child_thread,"runId":null,"parentNodeId":null,"rootNodeId":child_node,"kind":"root_turn","status":"running","countsForRun":false,"providerThreadId":provider_thread_row,"providerTurnId":provider_turn,"nativeItemRef":native_ref,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started_at,"completedAt":null});
        commit(host.store, &child_thread, |_| Ok(vec![("provider-turn.updated", turn_row.clone())]))?;
        commit(host.store, &child_thread, |_| Ok(vec![("node.updated", node_row.clone())]))
    }

    /// A notification about a subagent's thread. `root` is the top-level turn, when one runs.
    pub(crate) fn notification(&mut self, host: &Host, root: Option<&Scope>, method: &str, params: &Value) -> Result<(), StoreError> {
        let Some(thread) = params["threadId"].as_str() else { return Ok(()) };
        if method == "turn/started" {
            let turn = params["turn"]["id"].as_str().unwrap_or_default();
            return self.turn_started(host, root, thread, turn, &timestamp(&params["turn"]["startedAt"]));
        }
        let Some(child) = self.children.get_mut(thread) else { return Ok(()) };
        if method == "thread/tokenUsage/updated" && child.active.as_ref().map(|scope| scope.native_turn.as_str()) != params["turnId"].as_str() {
            // Usage is thread-wide: a late update still moves the baseline.
            child.projector.accumulate_usage(params["turnId"].as_str().unwrap_or_default(), &params["tokenUsage"]);
            return Ok(());
        }
        if method == "turn/completed" {
            return self.turn_completed(host, thread, params);
        }
        let Some(active) = child.active.clone().filter(|scope| params["turnId"].as_str() == Some(scope.native_turn.as_str())) else { return Ok(()) };
        let sender = child.parent.thread_id.clone();
        if matches!(method, "item/started" | "item/completed") && params["item"]["type"] == "userMessage" {
            // The first turn's message is the spawn prompt; a later turn's continues the subagent.
            if active.provider_turn_ordinal != 1 {
                let item = &params["item"];
                let started = if method == "item/started" { params["startedAtMs"].clone() } else { Value::Null };
                let projector = &mut child.projector;
                let events = commit_events(host.store, &active.thread_id, |view| projector.subagent_user_message(view, item, &sender, &started))?;
                if events {
                    return Ok(());
                }
            }
            return Ok(());
        }
        if child.projector.buffer_delta(method, params) {
            return Ok(());
        }
        if child.projector.is_bookkeeping(method, params) {
            child.projector.notification(&Value::Null, method, params)?;
        } else {
            let projector = &mut child.projector;
            commit(host.store, &active.thread_id, |view| projector.notification(view, method, params))?;
        }
        if let Some(result) = child.projector.take_final_result() {
            self.task_update(host.store, thread, &self.children[thread].task["status"].as_str().unwrap_or("running").to_owned(), Some(json!(result)), None, None, false)?;
        }
        Ok(())
    }

    /// `finalizeCodexTurn` for a subagent's turn: what it wrote ends, and the parent hears of it.
    fn turn_completed(&mut self, host: &Host, native_thread: &str, params: &Value) -> Result<(), StoreError> {
        let turn = params["turn"]["id"].as_str().unwrap_or_default().to_owned();
        let Some(child) = self.children.get_mut(native_thread) else { return Ok(()) };
        let Some(scope) = child.active.clone().filter(|scope| scope.native_turn == turn) else { return Ok(()) };
        let native = match params["turn"]["status"].as_str() {
            Some("completed") => "completed",
            Some("interrupted") => "interrupted",
            _ => "failed",
        };
        let status = if native == "completed" && self.interrupting.remove(&turn) { "interrupted" } else { native };
        let completed_at = timestamp(&params["turn"]["completedAt"]);
        if status != "completed" {
            self.terminalized.insert(turn.clone());
        }
        let child = self.children.get_mut(native_thread).unwrap();
        let projector = &mut child.projector;
        commit(host.store, &scope.thread_id, |view| {
            let mut events = projector.finish(view, status)?;
            for (kind, row) in &mut events {
                if *kind == "provider-turn.updated" && row["id"] == scope.provider_turn_id {
                    row["status"] = json!(status);
                    row["completedAt"] = json!(completed_at);
                }
            }
            Ok(events)
        })?;
        // The turn's root node and whatever it left running end with it.
        let ended = if status == "completed" { "cancelled" } else { status };
        let node_id = scope.root_node_id.clone();
        let started = child.started_at.clone();
        commit(host.store, &scope.thread_id, |view| {
            let mut events: Vec<(&'static str, Value)> = vec![];
            for node in rows(view, "nodes").iter().filter(|node| node["providerTurnId"] == scope.provider_turn_id && node["id"] != node_id && crate::background_work::work_active(&node["status"])) {
                let mut node = node.clone();
                node["status"] = json!(ended);
                node["completedAt"] = json!(completed_at);
                events.push(("node.updated", node));
            }
            for item in rows(view, "turnItems").iter().filter(|item| item["providerTurnId"] == scope.provider_turn_id && crate::background_work::work_active(&item["status"])) {
                let mut item = item.clone();
                item["status"] = json!(ended);
                item["completedAt"] = json!(completed_at);
                item["updatedAt"] = json!(completed_at);
                if item.get("streaming").is_some() {
                    item["streaming"] = json!(false);
                }
                events.push(("turn-item.updated", item));
            }
            let root = find(view, "nodes", &node_id)?.clone();
            let mut root = root;
            root["status"] = json!(status);
            root["completedAt"] = json!(completed_at);
            events.push(("node.updated", root));
            Ok(events)
        })?;
        let child = self.children.get_mut(native_thread).unwrap();
        child.active = None;
        let mut thread_row = child.provider_thread.clone();
        thread_row["status"] = json!("idle");
        thread_row["updatedAt"] = json!(completed_at);
        let subagent_node = json!({"id":child.subagent_node_id,"threadId":child.parent.thread_id,"runId":child.parent.run_id,"parentNodeId":child.parent.item_parent_node_id,"rootNodeId":child.parent.root_node_id,"kind":"subagent","status":status,"countsForRun":false,"providerThreadId":thread_row["id"],"providerTurnId":child.parent.provider_turn_id,"nativeItemRef":child.task["nativeTaskRef"],"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":completed_at});
        let parent_thread = child.parent.thread_id.clone();
        self.write_provider_thread(host.store, native_thread, thread_row)?;
        commit(host.store, &parent_thread, |_| Ok(vec![("node.updated", subagent_node.clone())]))?;
        self.task_update(host.store, native_thread, status, None, Some(completed_at.as_str()), None, false)
    }

    /// A stopped subagent turn that Codex never reported ending is over with the actor.
    pub(crate) fn clear_active(&mut self) {
        for child in self.children.values_mut() {
            child.active = None;
        }
    }
}

/// Commit what a projector built; whether anything was written.
fn commit_events(store: &Store, thread: &str, plan: impl FnOnce(&Value) -> Result<Vec<(&'static str, Value)>, StoreError>) -> Result<bool, StoreError> {
    let mut wrote = false;
    commit(store, thread, |view| {
        let events = plan(view)?;
        wrote = !events.is_empty();
        Ok(events)
    })?;
    Ok(wrote)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent thread in a store, and the turn of it that spawns subagents.
    fn parent(store: &Store) -> Scope {
        let now = at(Utc::now());
        let thread = json!({"createdBy":"user","creationSource":"web","id":"thread:parent","projectId":"project","title":"Parent","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"parent-model","options":[{"id":"reasoningEffort","value":"high"}]},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"linkedPullRequest":null,"pullRequests":[],"activeProviderThreadId":null,"lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread:parent"},"forkedFrom":null,"createdAt":now,"updatedAt":now,"archivedAt":null,"settledOverride":null,"settledAt":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null});
        subagents::ensure_child_thread(store, &thread).unwrap();
        Scope {
            thread_id: "thread:parent".into(),
            run_id: json!("run:1"),
            native_thread: "native-parent".into(),
            native_turn: "turn-parent".into(),
            provider_thread: json!({"id":"provider-thread:parent"}),
            provider_turn_id: json!("provider-turn:parent"),
            provider_turn_ordinal: 1,
            item_parent_node_id: json!("node:root"),
            root_node_id: json!("node:root"),
            app_thread: thread.clone(),
            model_selection: thread["modelSelection"].clone(),
            subagent: None,
        }
    }
    fn view(store: &Store, thread: &str) -> Value {
        store.projection("thread", thread).unwrap().unwrap()
    }

    /// A v1 spawn registers one child thread per receiver, with the prompt as the child's first
    /// message; the call's states and a close move the subagents, and a settled one is not reopened
    /// by a state snapshot that arrives late.
    #[test]
    fn a_spawn_registers_children_and_the_states_it_reports_settle_them() {
        let store = Store::memory().unwrap();
        let parent = parent(&store);
        let host = Host { store: &store, session_id: "session", process: None };
        let mut subagents = Subagents::default();
        let spawn = json!({"type":"collabAgentToolCall","id":"call","tool":"spawnAgent","status":"completed","senderThreadId":"native-parent","receiverThreadIds":["native-a","native-b"],"prompt":"Read package.json","model":"child-model","agentsStates":{"native-a":{"status":"pendingInit","message":null},"native-b":{"status":"running","message":null}}});
        subagents.register_spawned(&host, &parent, &spawn).unwrap();
        subagents.update_states(&host, &spawn).unwrap();
        let root = view(&store, "thread:parent");
        let tasks = root["subagents"].as_array().unwrap();
        assert_eq!(tasks.iter().map(|task| task["status"].as_str().unwrap()).collect::<Vec<_>>(), ["pending", "running"]);
        assert_eq!(tasks[0]["model"], "child-model");
        assert_eq!(root["nodes"].as_array().unwrap().iter().filter(|node| node["kind"] == "subagent").count(), 2);
        // The parent lists the provider threads its subagents run, and the timeline names each one.
        assert_eq!(root["providerThreads"].as_array().unwrap().len(), 2);
        assert_eq!(root["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "subagent").count(), 2);
        let child = view(&store, "thread:provider:codex:native-thread:native-a");
        assert_eq!((child["thread"]["lineage"]["relationshipToParent"].as_str(), child["thread"]["modelSelection"]["model"].as_str(), child["thread"]["title"].as_str()), (Some("subagent"), Some("child-model"), Some("Read package.json")));
        assert_eq!(child["messages"][0]["text"], "Read package.json");
        assert_eq!(child["messages"][0]["senderThreadId"], "thread:parent");
        // The v1 `close` and `wait` calls carry the states: shutdown ends a subagent as cancelled, an
        // error as failed, and a completed one keeps the message it reported.
        let close = json!({"type":"collabAgentToolCall","id":"close","tool":"closeAgent","status":"completed","receiverThreadIds":["native-a"],"agentsStates":{"native-a":{"status":"shutdown","message":null},"native-b":{"status":"completed","message":"done"}}});
        subagents.update_states(&host, &close).unwrap();
        let root = view(&store, "thread:parent");
        let tasks = root["subagents"].as_array().unwrap();
        assert_eq!((tasks[0]["status"].as_str(), tasks[1]["status"].as_str(), tasks[1]["result"].as_str()), (Some("cancelled"), Some("completed"), Some("done")));
        assert!(tasks[0]["completedAt"].is_string());
        let late = json!({"type":"collabAgentToolCall","id":"wait","tool":"wait","agentsStates":{"native-a":{"status":"running","message":null},"native-b":{"status":"errored","message":null}}});
        subagents.update_states(&host, &late).unwrap();
        let root = view(&store, "thread:parent");
        let tasks = root["subagents"].as_array().unwrap();
        assert_eq!((tasks[0]["status"].as_str(), tasks[1]["status"].as_str()), (Some("cancelled"), Some("completed")), "a settled subagent is only reopened by a new turn");
        for status in ["pendingInit", "running", "interrupted", "completed", "errored", "shutdown", "notFound"] {
            assert!(["pending", "running", "interrupted", "completed", "failed", "cancelled"].contains(&collab_status(status)));
        }
    }

    /// A v2 subagent nests: the child of a child is a thread of the child, its approvals belong to
    /// the top-level subagent, and a turn that starts under a stopped turn is not adopted.
    #[test]
    fn nested_subagents_belong_to_the_thread_that_spawned_them() {
        let store = Store::memory().unwrap();
        let parent = parent(&store);
        let host = Host { store: &store, session_id: "session", process: None };
        let mut subagents = Subagents::default();
        let started = |id: &str, agent: &str, path: &str| json!({"type":"subAgentActivity","id":id,"kind":"started","agentThreadId":agent,"agentPath":path});
        subagents.activity(&host, Some(&parent), &started("one", "native-one", "/root/one")).unwrap();
        subagents.turn_started(&host, Some(&parent), "native-one", "turn-one", "2026-01-01T00:00:00.000Z").unwrap();
        let one = subagents.active_scope("native-one").expect("the first subagent's turn runs");
        assert_eq!((one.provider_turn_ordinal, one.subagent.as_deref()), (1, Some("native-one")));
        subagents.activity(&host, Some(&one), &started("two", "native-two", "/root/one/two")).unwrap();
        assert_eq!(subagents.request_owner("native-two", Some("native-parent")), Some(Some(subagents.children["native-one"].subagent_node_id.clone())));
        assert_eq!(subagents.request_owner("native-parent", Some("native-parent")), Some(None));
        assert_eq!(subagents.request_owner("native-stranger", Some("native-parent")), None);
        let middle = view(&store, "thread:provider:codex:native-thread:native-one");
        assert_eq!(middle["subagents"][0]["childThreadId"], "thread:provider:codex:native-thread:native-two");
        let grandchild = view(&store, "thread:provider:codex:native-thread:native-two");
        assert_eq!(grandchild["thread"]["lineage"]["parentThreadId"], "thread:provider:codex:native-thread:native-one");
        assert_eq!(grandchild["thread"]["lineage"]["rootThreadId"], "thread:parent");
        // The parent turn was stopped; a subagent turn that starts under it is left unadopted.
        subagents.terminalized.insert("turn-one".into());
        subagents.turn_started(&host, Some(&parent), "native-two", "turn-two", "2026-01-01T00:00:01.000Z").unwrap();
        assert!(subagents.active_scope("native-two").is_none());
        // A turn that starts before the spawn is reported waits for it.
        subagents.turn_started(&host, Some(&parent), "native-later", "turn-later", "2026-01-01T00:00:02.000Z").unwrap();
        assert!(subagents.active_scope("native-later").is_none());
        subagents.activity(&host, Some(&parent), &started("later", "native-later", "/root/later")).unwrap();
        assert!(subagents.active_scope("native-later").is_some());
    }
}
