//! The Claude adapter's half of subagents (`updateClaudeSubagentNode` and the subagent routing of
//! `handleSdkMessageFrame` in `ClaudeAdapterV2.ts`). A subagent is an `Agent` call the CLI runs on its
//! own: its frames carry the launching call as `parent_tool_use_id`, its lifecycle arrives as
//! `task_*` system frames, and it runs in a child app thread of the thread that launched it. The
//! child has no provider thread and no run; what the parent shows of it is a `subagent` node, a
//! `Subagent` row and a `subagent` timeline item (see `subagents.rs`).
//!
//! The registry outlives the turn that launched a subagent: a background subagent reports after the
//! root turn ended, and a later turn may resume it. A child's open tool calls are kept by the child's
//! own projector, so a turn that ends does not fail them while the subagent runs.
use crate::{
    claude_projection::{self as projection, Projector, ToolOutput, enc, item_id, message_id, native_ref, node_id, result_is_error, subagent_result_text, tool_results},
    codex_runtime::{commit, projection as thread_view},
    execution::at,
    persistence::{Store, StoreError},
    provider_turn_context::TurnContext,
    subagents,
};
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const DRIVER: &str = "claudeAgent";
/// Unregistered launches and frames held for them are bounded, as the original bounds them.
const PENDING_CAP: usize = 64;
const FRAME_CAP: usize = 256;

/// The turn a subagent's parent rows belong to: the running one, or the one that settled last.
pub(crate) struct Parent<'a> {
    pub store: &'a Store,
    pub ctx: &'a TurnContext,
    pub selection: &'a Value,
    pub app_thread: &'a Value,
}

struct Subagent {
    task: Value,
    root_node_id: Value,
    child_thread_id: String,
    child_root_node_id: String,
    turn_item_id: String,
    turn_item_ordinal: u64,
    /// The call that started the current run: the `Agent` launch, then each `SendMessage` that resumes.
    run_tool_use_id: Option<String>,
    result_item_ordinal: Option<u64>,
    /// The latest assistant text routed into the child, accumulated across the per-block snapshots of
    /// one native message. A completion result equal to it is already on screen.
    last_assistant_text: Option<String>,
    last_assistant_message_id: Option<String>,
    /// Projects the child's tool calls; its positions are the child thread's.
    projector: Projector,
}

#[derive(Default, Clone)]
struct Launch {
    model: Option<String>,
    /// The subagent whose own `Agent` call starts this one.
    owner: Option<String>,
}

/// What a lifecycle frame or an `Agent` result says of a subagent.
#[derive(Default)]
pub(crate) struct Update<'a> {
    pub task_id: &'a str,
    pub tool_use_id: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub title: Option<&'a str>,
    pub model: Option<&'a str>,
    /// The tool use of the subagent that started this one, read only when this registers it.
    pub owner: Option<&'a str>,
    pub progress: Option<&'a str>,
    pub result: Option<&'a str>,
    pub status: &'a str,
    pub reopen: bool,
}

#[derive(Default)]
pub(crate) struct Subagents {
    by_task: IndexMap<String, Subagent>,
    task_by_tool_use: HashMap<String, String>,
    launches: IndexMap<String, Launch>,
    /// Frames of a subagent not registered yet, by the tool use that launches it.
    held: IndexMap<String, Vec<Value>>,
    /// Every `Agent` call seen: its result is a subagent's, never a tool's.
    agent_calls: HashSet<String>,
}

fn first_string<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().map(str::trim).filter(|text| !text.is_empty())
}
fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn native_task_ref(task_id: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":task_id,"strength":"strong"})
}

impl Subagents {
    pub(crate) fn agent_calls(&self) -> &HashSet<String> {
        &self.agent_calls
    }
    /// A `task_started` that raced past settle reopens a settled subagent before its frame is handled.
    pub(crate) fn preopen(&mut self, task_id: &str) {
        let Some(subagent) = self.by_task.get_mut(task_id) else { return };
        if subagent.task["status"] == "running" {
            return;
        }
        let now = at(Utc::now());
        if let Some(object) = subagent.task.as_object_mut() {
            object.remove("progress");
        }
        subagent.task["status"] = json!("running");
        subagent.task["result"] = Value::Null;
        subagent.task["startedAt"] = json!(now);
        subagent.task["completedAt"] = Value::Null;
        subagent.task["updatedAt"] = json!(now);
    }
    pub(crate) fn is_registered(&self, task_id: &str) -> bool {
        self.by_task.contains_key(task_id)
    }
    pub(crate) fn is_running(&self, task_id: &str) -> bool {
        self.by_task.get(task_id).is_some_and(|subagent| subagent.task["status"] == "running")
    }
    pub(crate) fn task_of_tool_use(&self, tool_use_id: &str) -> Option<&str> {
        self.task_by_tool_use.get(tool_use_id).map(String::as_str)
    }
    pub(crate) fn task(&self, task_id: &str) -> Option<&Value> {
        self.by_task.get(task_id).map(|subagent| &subagent.task)
    }
    pub(crate) fn child_thread_of(&self, task_id: &str) -> Option<&str> {
        self.by_task.get(task_id).map(|subagent| subagent.child_thread_id.as_str())
    }
    /// Whether this tool use is a call a subagent has open (its approval is asked on the parent).
    pub(crate) fn child_has_tool(&self, tool_use_id: &str) -> bool {
        self.by_task.values().any(|subagent| subagent.projector.has_tool(tool_use_id))
    }

    /// Whether a subagent's child thread still takes events. The run that owns a subagent routes
    /// what it does to its child thread while that run is live, or while any subagent the run owns is
    /// running; once the run has settled and its subagents have all ended, a late frame of a child
    /// (its call's last result, its final answer) has no run to be written under and is dropped.
    fn child_open(&self, p: &Parent, task_id: &str) -> Result<bool, StoreError> {
        let Some(subagent) = self.by_task.get(task_id) else { return Ok(false) };
        let run = &subagent.task["runId"];
        let view = thread_view(p.store, p.app_thread["id"].as_str().unwrap_or_default())?;
        let live = view["runs"].as_array().into_iter().flatten().any(|candidate| candidate["id"] == *run && matches!(candidate["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting")));
        Ok(live || self.by_task.values().any(|other| other.task["runId"] == *run && other.task["status"] == "running"))
    }
    fn resolve(&self, task_id: &str, tool_use_id: Option<&str>) -> Option<String> {
        if self.by_task.contains_key(task_id) {
            return Some(task_id.to_owned());
        }
        tool_use_id.and_then(|id| self.task_by_tool_use.get(id)).filter(|task| self.by_task.contains_key(*task)).cloned()
    }
    pub(crate) fn resolve_tool_use(&self, tool_use_id: &str) -> Option<&str> {
        self.task_by_tool_use.get(tool_use_id).map(String::as_str).filter(|task| self.by_task.contains_key(*task))
    }

    // --- launches -----------------------------------------------------------------------------
    fn remember_launch_fields(&mut self, tool_use_id: &str, launch: Launch) {
        let merged = {
            let mut current = self.launches.get(tool_use_id).cloned().unwrap_or_default();
            if launch.model.is_some() {
                current.model = launch.model;
            }
            if launch.owner.is_some() {
                current.owner = launch.owner;
            }
            current
        };
        self.launches.insert(tool_use_id.to_owned(), merged);
        if self.launches.len() > PENDING_CAP {
            self.launches.shift_remove_index(0);
        }
    }
    /// `rememberClaudeSubagentLaunch`: an `Agent` call carries the model override even when the SDK
    /// omits the child's snapshots, and a subagent's own `Agent` call names its owner.
    pub(crate) fn remember_launch(&mut self, tool_use_id: &str, input: &Value, owner_tool_use: Option<&str>, parent_model: &str) {
        self.agent_calls.insert(tool_use_id.to_owned());
        if self.task_by_tool_use.contains_key(tool_use_id) {
            return;
        }
        let requested = first_string(input, "model");
        // "inherit" is the caller's model: the session's here, an owner's once the task starts.
        let model = match requested {
            Some("inherit") => owner_tool_use.is_none().then(|| parent_model.to_owned()),
            other => other.map(str::to_owned),
        };
        let known = self.launches.get(tool_use_id).and_then(|launch| launch.model.as_ref()).is_some();
        let launch = Launch { model: model.filter(|_| !known), owner: owner_tool_use.map(str::to_owned) };
        if launch.model.is_some() || launch.owner.is_some() {
            self.remember_launch_fields(tool_use_id, launch);
        }
    }
    pub(crate) fn take_launch(&mut self, tool_use_id: Option<&str>) -> Option<(Option<String>, Option<String>)> {
        let launch = self.launches.shift_remove(tool_use_id?)?;
        Some((launch.model, launch.owner))
    }

    // --- the parent's view of a subagent ------------------------------------------------------
    /// `updateClaudeSubagentNode`: the registry entry, the child thread, and the `subagent` node, row
    /// and timeline item of the parent. A settled subagent is reopened only by an authoritative
    /// `task_started`; a late `task_progress` never does.
    pub(crate) fn update(&mut self, p: &Parent, root: &mut Projector, u: Update) -> Result<(), StoreError> {
        let key = self.resolve(u.task_id, u.tool_use_id);
        if key.is_none() && u.status != "running" {
            return Ok(());
        }
        root.mark_subagents();
        let existing_status = key.as_ref().map(|key| self.by_task[key].task["status"].as_str().unwrap_or("running").to_owned());
        let existing_open = existing_status.as_deref() == Some("running");
        let is_reopen = u.reopen && key.is_some() && !existing_open && u.status == "running";
        if key.is_some() && !existing_open && u.status == "running" && !is_reopen {
            return Ok(());
        }
        // A call the subagent left open never returns once it ends: it ends first.
        if let (Some(key), true) = (&key, u.status != "running") {
            let subagent = self.by_task.get_mut(key).unwrap();
            let view = thread_view(p.store, &subagent.child_thread_id)?;
            let events = subagent.projector.end_tools(&view, if u.status == "cancelled" { "interrupted" } else { "failed" })?;
            if !events.is_empty() {
                commit(p.store, &subagent.child_thread_id, |_| Ok(events))?;
            }
        }
        let resume_tool_use = match (&key, u.tool_use_id) {
            (Some(key), Some(tool_use)) if u.reopen && u.prompt.is_some() && self.by_task[key].run_tool_use_id.as_deref() != Some(tool_use) => Some(tool_use.to_owned()),
            _ => None,
        };
        let lifecycle_changed = key.is_none() || existing_status.as_deref() != Some(u.status) || (u.reopen && u.status == "running");
        let now = at(Utc::now());
        let native_item = format!("task:{}", u.task_id);
        let parent_view = thread_view(p.store, p.app_thread["id"].as_str().unwrap_or_default())?;
        let provider_thread = p.ctx.provider_thread_id.as_str().unwrap_or_default();
        let (node, child_root_node, child_thread_id, turn_item_id) = match &key {
            Some(key) => {
                let existing = &self.by_task[key];
                (existing.task["id"].as_str().unwrap_or_default().to_owned(), existing.child_root_node_id.clone(), existing.child_thread_id.clone(), existing.turn_item_id.clone())
            }
            None => (
                node_id(&native_item),
                node_id(&format!("{native_item}:thread-root")),
                format!("thread:provider:{DRIVER}:native-thread:{}", enc(&format!("{provider_thread}:{}", u.task_id))),
                item_id(&format!("{native_item}:subagent")),
            ),
        };
        let turn_item_ordinal = match &key {
            Some(key) => self.by_task[key].turn_item_ordinal,
            None => root.position(&parent_view, &format!("{native_item}:subagent")),
        };
        let owner_node = u.owner.and_then(|owner| self.resolve_tool_use(owner)).and_then(|task| self.by_task.get(task)).map(|owner| owner.task["id"].clone());
        let mut task = match &key {
            Some(key) => {
                let mut prior = self.by_task[key].task.clone();
                if is_reopen {
                    prior.as_object_mut().map(|object| object.remove("progress"));
                    prior["result"] = Value::Null;
                }
                prior
            }
            None => json!({
                "id": node, "threadId": p.app_thread["id"], "runId": p.ctx.run_id, "parentNodeId": owner_node.clone().unwrap_or_else(|| p.ctx.root_node_id.clone()),
                "origin": "provider_native", "createdBy": "agent", "driver": DRIVER, "providerInstanceId": p.ctx.provider_instance_id,
                "providerThreadId": null, "childThreadId": child_thread_id, "nativeTaskRef": native_task_ref(u.task_id),
                "prompt": u.prompt.unwrap_or(""), "title": u.title, "model": u.model, "result": null, "startedAt": now,
            }),
        };
        let prior_completed = task.get("completedAt").filter(|value| !value.is_null()).cloned();
        task["status"] = json!(u.status);
        if u.reopen && u.status == "running" && key.is_some() {
            task["runId"] = json!(p.ctx.run_id);
        }
        for (field, value) in [("prompt", u.prompt), ("title", u.title), ("model", u.model), ("progress", u.progress), ("result", u.result)] {
            if let Some(value) = value {
                task[field] = json!(value);
            }
        }
        if is_reopen {
            task["startedAt"] = json!(now);
        }
        task["completedAt"] = if u.status == "running" { Value::Null } else { prior_completed.unwrap_or_else(|| json!(now)) };
        task["updatedAt"] = json!(now);

        let model_selection = match u.model.or(task["model"].as_str()).filter(|model| !model.is_empty()) {
            Some(model) if model != p.selection["model"].as_str().unwrap_or_default() => json!({"instanceId":p.selection["instanceId"],"model":model}),
            _ => p.selection.clone(),
        };
        let is_new = key.is_none();
        let task_key = u.task_id.to_owned();
        if is_new {
            let ordinal = self.by_task.len() as u64 + 1;
            let title = subagents::thread_title(p.app_thread["title"].as_str().unwrap_or_default(), &task["title"], task["prompt"].as_str().unwrap_or_default(), ordinal);
            let thread = subagents::child_thread(p.app_thread, &json!({"childThreadId":child_thread_id,"parentNodeId":node,"activeProviderThreadId":null,"providerInstanceId":p.ctx.provider_instance_id,"modelSelection":model_selection,"title":title,"createdBy":"agent","creationSource":"provider"}), &now);
            subagents::ensure_child_thread(p.store, &thread)?;
            self.by_task.insert(
                task_key.clone(),
                Subagent {
                    task: task.clone(),
                    root_node_id: p.ctx.root_node_id.clone(),
                    child_thread_id: child_thread_id.clone(),
                    child_root_node_id: child_root_node.clone(),
                    turn_item_id: turn_item_id.clone(),
                    turn_item_ordinal,
                    run_tool_use_id: u.tool_use_id.map(str::to_owned),
                    result_item_ordinal: None,
                    last_assistant_text: None,
                    last_assistant_message_id: None,
                    projector: Projector::for_child(&child_thread_id, &child_root_node, &p.ctx.provider_instance_id, p.selection.clone()),
                },
            );
        }
        let subagent = self.by_task.get_mut(key.as_deref().unwrap_or(&task_key)).unwrap();
        subagent.task = task.clone();
        if !is_new {
            subagent.run_tool_use_id = resume_tool_use.clone().or_else(|| subagent.run_tool_use_id.clone());
        }
        if u.reopen {
            subagent.last_assistant_text = None;
            subagent.last_assistant_message_id = None;
        }
        if let Some(tool_use) = u.tool_use_id {
            self.task_by_tool_use.insert(tool_use.to_owned(), u.task_id.to_owned());
        }
        let subagent = self.by_task.get_mut(key.as_deref().unwrap_or(&task_key)).unwrap();

        let mut parent_events: Vec<(&'static str, Value)> = vec![];
        if lifecycle_changed {
            parent_events.push(("node.updated", json!({"id":node,"threadId":task["threadId"],"runId":task["runId"],"parentNodeId":task["parentNodeId"],"rootNodeId":subagent.root_node_id,"kind":"subagent","status":u.status,"countsForRun":false,"providerThreadId":p.ctx.provider_thread_id,"providerTurnId":p.ctx.provider_turn_id,"nativeItemRef":native_task_ref(u.task_id),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":task["startedAt"],"completedAt":task["completedAt"]})));
            let root_row = json!({"id":child_root_node,"threadId":child_thread_id,"runId":null,"parentNodeId":null,"rootNodeId":child_root_node,"kind":"root_turn","status":u.status,"countsForRun":false,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":task["nativeTaskRef"],"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":task["startedAt"],"completedAt":task["completedAt"]});
            commit(p.store, &child_thread_id, |_| Ok(vec![("node.updated", root_row)]))?;
        }
        // Each run opens with its own prompt in the child thread: the launch task, then every message
        // that resumes the subagent.
        let prompt_native = if is_new { Some(format!("{native_item}:prompt")) } else { resume_tool_use.as_ref().map(|tool_use| format!("{native_item}:prompt:{tool_use}")) };
        if let Some(prompt_native) = prompt_native {
            let ordinal = Projector::arrival(&thread_view(p.store, &child_thread_id)?, &item_id(&prompt_native));
            let (message, item) = subagents::conversation_artifacts(
                &json!({"senderThreadId":p.app_thread["id"],"messageId":message_id(&prompt_native),"turnItemId":item_id(&prompt_native),"threadId":child_thread_id,"rootNodeId":child_root_node,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":native_ref(&prompt_native),"role":"user","text":task["prompt"],"ordinal":ordinal}),
                &now,
            );
            commit(p.store, &child_thread_id, |_| Ok(vec![("message.updated", message), ("turn-item.updated", item)]))?;
        }
        parent_events.push(("subagent.updated", task.clone()));
        let mut item = json!({"id":subagent.turn_item_id,"threadId":task["threadId"],"runId":task["runId"],"nodeId":task["id"],"providerThreadId":p.ctx.provider_thread_id,"providerTurnId":p.ctx.provider_turn_id,"nativeItemRef":task["nativeTaskRef"],"parentItemId":null,"ordinal":subagent.turn_item_ordinal,"status":task["status"],"title":task["title"],"startedAt":task["startedAt"],"completedAt":task["completedAt"],"updatedAt":task["updatedAt"],"type":"subagent","subagentId":task["id"],"origin":task["origin"],"driver":task["driver"],"providerInstanceId":task["providerInstanceId"],"childThreadId":task["childThreadId"],"prompt":task["prompt"],"result":task["result"]});
        if let Some(progress) = task.get("progress").filter(|progress| !progress.is_null()) {
            item["progress"] = progress.clone();
        }
        parent_events.push(("turn-item.updated", item));
        let parent_thread = p.app_thread["id"].as_str().unwrap_or_default().to_owned();
        commit(p.store, &parent_thread, |_| Ok(parent_events))?;

        // A completed subagent's result is normally its final assistant message, already in the child
        // thread when its text was routed there. Failures and cancellations always get the message.
        if let (Some(result), true) = (u.result, u.status != "running" && self.child_open(p, u.task_id)?) {
            let subagent = self.by_task.get_mut(key.as_deref().unwrap_or(&task_key)).unwrap();
            let already_shown = u.status == "completed" && normalize_text(subagent.last_assistant_text.as_deref().unwrap_or_default()) == normalize_text(result);
            if !result.trim().is_empty() && !already_shown {
                let result_native = format!("{native_item}:result");
                let ordinal = Projector::arrival(&thread_view(p.store, &child_thread_id)?, &item_id(&result_native));
                subagent.result_item_ordinal = Some(ordinal);
                let (message, item) = subagents::conversation_artifacts(
                    &json!({"messageId":message_id(&result_native),"turnItemId":item_id(&result_native),"threadId":child_thread_id,"rootNodeId":child_root_node,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":native_ref(&result_native),"role":"assistant","text":result,"ordinal":ordinal}),
                    &now,
                );
                commit(p.store, &child_thread_id, |_| Ok(vec![("message.updated", message), ("turn-item.updated", item)]))?;
            }
        }
        Ok(())
    }

    /// A subagent launched before a server restart is missing from the registry when `SendMessage`
    /// resumes it. The resume's `task_started` names only the `SendMessage` call, while the resumed
    /// run's frames carry the launch call as their parent, which only the CLI's session storage still
    /// records. Registering the subagent as a finished run lets the resume reopen it like an
    /// in-session one: the same child thread, the launch prompt kept, the new prompt appended.
    pub(crate) fn recover_resumed(&mut self, p: &Parent, root: &mut Projector, task_id: &str, title: &str, launch_tool_use: &str) -> Result<(), StoreError> {
        if self.by_task.contains_key(task_id) {
            return Ok(());
        }
        let now = at(Utc::now());
        let native_item = format!("task:{task_id}");
        let provider_thread = p.ctx.provider_thread_id.as_str().unwrap_or_default();
        let child_thread_id = format!("thread:provider:{DRIVER}:native-thread:{}", enc(&format!("{provider_thread}:{task_id}")));
        let child_root = node_id(&format!("{native_item}:thread-root"));
        let parent_view = thread_view(p.store, p.app_thread["id"].as_str().unwrap_or_default())?;
        let task = json!({
            "id": node_id(&native_item), "threadId": p.app_thread["id"], "runId": p.ctx.run_id, "parentNodeId": p.ctx.root_node_id,
            "origin": "provider_native", "createdBy": "agent", "driver": DRIVER, "providerInstanceId": p.ctx.provider_instance_id,
            "providerThreadId": null, "childThreadId": child_thread_id, "nativeTaskRef": native_task_ref(task_id),
            "prompt": "", "title": title, "model": null, "result": null, "status": "completed", "startedAt": now, "completedAt": now, "updatedAt": now,
        });
        let subagent = Subagent {
            task,
            root_node_id: p.ctx.root_node_id.clone(),
            child_thread_id: child_thread_id.clone(),
            child_root_node_id: child_root.clone(),
            turn_item_id: item_id(&format!("{native_item}:subagent")),
            turn_item_ordinal: root.position(&parent_view, &format!("{native_item}:subagent")),
            run_tool_use_id: Some(launch_tool_use.to_owned()),
            result_item_ordinal: None,
            last_assistant_text: None,
            last_assistant_message_id: None,
            projector: Projector::for_child(&child_thread_id, &child_root, &p.ctx.provider_instance_id, p.selection.clone()),
        };
        self.by_task.insert(task_id.to_owned(), subagent);
        self.task_by_tool_use.insert(launch_tool_use.to_owned(), task_id.to_owned());
        Ok(())
    }

    // --- lifecycle frames ---------------------------------------------------------------------
    /// `task_started` of a subagent: registers it, or reopens one a message resumed.
    pub(crate) fn task_started(&mut self, p: &Parent, root: &mut Projector, message: &Value) -> Result<(), StoreError> {
        let task_id = message["task_id"].as_str().unwrap_or_default();
        let tool_use = message["tool_use_id"].as_str();
        let (model, owner) = self.take_launch(tool_use).unwrap_or((None, None));
        let owner_model = owner.as_deref().and_then(|owner| self.resolve_tool_use(owner)).and_then(|task| self.by_task.get(task)).and_then(|owner| owner.task["model"].as_str().map(str::to_owned));
        // With no model of its own, a nested subagent runs on its owner's.
        let model = model.or(owner_model);
        self.update(
            p,
            root,
            Update { task_id, tool_use_id: tool_use, prompt: message["prompt"].as_str(), title: message["description"].as_str(), model: model.as_deref(), owner: owner.as_deref(), status: "running", reopen: true, ..Default::default() },
        )?;
        self.release(p, root, message)
    }
    pub(crate) fn progress(&mut self, p: &Parent, root: &mut Projector, message: &Value) -> Result<(), StoreError> {
        let progress = message["description"].as_str().unwrap_or_default().trim();
        if progress.is_empty() {
            return Ok(());
        }
        self.update(p, root, Update { task_id: message["task_id"].as_str().unwrap_or_default(), tool_use_id: message["tool_use_id"].as_str(), progress: Some(progress), status: "running", ..Default::default() })
    }
    pub(crate) fn notification(&mut self, p: &Parent, root: &mut Projector, message: &Value) -> Result<(), StoreError> {
        let status = match message["status"].as_str() {
            Some("completed") => "completed",
            Some("stopped") => "cancelled",
            _ => "failed",
        };
        self.update(p, root, Update { task_id: message["task_id"].as_str().unwrap_or_default(), tool_use_id: message["tool_use_id"].as_str(), result: message["summary"].as_str(), status, ..Default::default() })
    }
    /// The `Agent` call's own result. A background launch resolves at once with an acknowledgement
    /// while the task keeps running; only its `task_notification` ends the subagent.
    pub(crate) fn agent_result(&mut self, p: &Parent, root: &mut Projector, result: &Value, structured: Option<&Value>) -> Result<(), StoreError> {
        let Some(tool_use) = result["tool_use_id"].as_str() else { return Ok(()) };
        let Some(task_id) = self.resolve_tool_use(tool_use).map(str::to_owned) else { return Ok(()) };
        let output = ToolOutput::from_result(result, structured);
        let value = output.value();
        let text = match &value {
            Value::Array(_) => subagent_result_text(&value),
            Value::Object(object) if object.contains_key("content") => subagent_result_text(&value),
            Value::Null => String::new(),
            other => projection::js_stringify(other),
        };
        let launched = value["isAsync"] == true || value["status"] == "async_launched" || text.starts_with("Async agent launched successfully.");
        if launched {
            return Ok(());
        }
        self.update(p, root, Update { task_id: &task_id, tool_use_id: Some(tool_use), result: (!text.is_empty()).then_some(text.as_str()), status: if result_is_error(result) { "failed" } else { "completed" }, ..Default::default() })
    }

    // --- a subagent's own frames --------------------------------------------------------------
    /// Frames held for a subagent that has now registered, handled in the order they arrived.
    fn release(&mut self, p: &Parent, root: &mut Projector, message: &Value) -> Result<(), StoreError> {
        if message["type"] != "system" || !matches!(message["subtype"].as_str(), Some("task_started" | "task_progress" | "task_notification")) {
            return Ok(());
        }
        let (Some(tool_use), Some(task_id)) = (message["tool_use_id"].as_str(), message["task_id"].as_str()) else { return Ok(()) };
        if !self.by_task.contains_key(task_id) {
            return Ok(());
        }
        // The pair is authoritative, so it also fills the tool-use index.
        self.task_by_tool_use.insert(tool_use.to_owned(), task_id.to_owned());
        for held in self.held.shift_remove(tool_use).unwrap_or_default() {
            self.child_frame(p, root, &held, true)?;
        }
        Ok(())
    }
    /// Held frames that this lifecycle frame releases ahead of itself (all but `task_started`).
    pub(crate) fn release_before(&mut self, p: &Parent, root: &mut Projector, message: &Value) -> Result<(), StoreError> {
        if message["subtype"] == "task_started" {
            return Ok(());
        }
        self.release(p, root, message)
    }
    /// An assistant or user frame of a subagent. Its text, thinking and tools belong to the child
    /// thread, never the parent's transcript. An owner not registered yet holds the frame.
    pub(crate) fn child_frame(&mut self, p: &Parent, root: &mut Projector, message: &Value, replayed: bool) -> Result<(), StoreError> {
        let Some(parent_tool) = message["parent_tool_use_id"].as_str() else { return Ok(()) };
        let assistant = message["type"] == "assistant";
        if assistant && !replayed {
            if let Some(model) = message["message"]["model"].as_str().map(str::trim).filter(|model| !model.is_empty()) {
                match self.resolve_tool_use(parent_tool).map(str::to_owned) {
                    None => self.remember_launch_fields(parent_tool, Launch { model: Some(model.to_owned()), owner: None }),
                    Some(task_id) => {
                        let current = self.by_task[&task_id].task.clone();
                        if current["status"] == "running" && current["model"] != model {
                            self.update(p, root, Update { task_id: &task_id, tool_use_id: Some(parent_tool), model: Some(model), status: "running", ..Default::default() })?;
                        }
                    }
                }
            }
        }
        let Some(task_id) = self.resolve_tool_use(parent_tool).map(str::to_owned) else {
            let frames = self.held.entry(parent_tool.to_owned()).or_default();
            if frames.len() < FRAME_CAP {
                frames.push(message.clone());
            }
            if self.held.len() > PENDING_CAP {
                self.held.shift_remove_index(0);
            }
            return Ok(());
        };
        if !self.child_open(p, &task_id)? {
            return Ok(());
        }
        let blocks = message["message"]["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if assistant {
            for block in blocks.iter().filter(|block| block["id"].is_string() && block["name"].is_string() && block.get("input").is_some()) {
                let (id, name, input) = (block["id"].as_str().unwrap(), block["name"].as_str().unwrap(), &block["input"]);
                if name == "Agent" {
                    let parent_model = self.by_task[&task_id].task["model"].as_str().unwrap_or_default().to_owned();
                    self.remember_launch(id, input, Some(parent_tool), &parent_model);
                    continue;
                }
                if self.child_has_tool(id) || root.has_tool(id) {
                    continue;
                }
                let subagent = self.by_task.get_mut(&task_id).unwrap();
                let view = thread_view(p.store, &subagent.child_thread_id)?;
                let events = subagent.projector.tool_started(&view, id, name, input.clone(), message.get("tool_use_meta"))?;
                commit(p.store, &subagent.child_thread_id, |_| Ok(events))?;
            }
        }
        let results = tool_results(message);
        let structured = (!assistant && results.len() == 1).then(|| message.get("tool_use_result")).flatten().filter(|value| !value.is_null()).cloned();
        for result in &results {
            let Some(id) = result["tool_use_id"].as_str() else { continue };
            // A nested subagent's own `Agent` call returns here.
            if self.resolve_tool_use(id).is_some() && !self.child_has_tool(id) && !root.has_tool(id) {
                self.agent_result(p, root, result, structured.as_ref())?;
                continue;
            }
            if root.has_tool(id) {
                let view = thread_view(p.store, p.app_thread["id"].as_str().unwrap_or_default())?;
                let events = root.tool_result(&view, result, structured.as_ref(), message)?;
                commit(p.store, p.app_thread["id"].as_str().unwrap_or_default(), |_| Ok(events))?;
                continue;
            }
            let owner = self.child_owning_tool(id).unwrap_or_else(|| task_id.clone());
            let subagent = self.by_task.get_mut(&owner).unwrap();
            let view = thread_view(p.store, &subagent.child_thread_id)?;
            let events = subagent.projector.tool_result(&view, result, structured.as_ref(), message)?;
            commit(p.store, &subagent.child_thread_id, |_| Ok(events))?;
        }
        if assistant {
            let thinking: Vec<&str> = blocks.iter().filter(|block| block["type"] == "thinking").filter_map(|block| block["thinking"].as_str()).filter(|text| !text.trim().is_empty()).collect();
            let now = at(Utc::now());
            let uuid = message["uuid"].as_str().unwrap_or_default();
            for (index, text) in thinking.into_iter().enumerate() {
                let native = format!("{uuid}:thinking:{index}");
                let subagent = self.by_task.get_mut(&task_id).unwrap();
                let ordinal = Projector::arrival(&thread_view(p.store, &subagent.child_thread_id)?, &item_id(&native));
                let item = json!({"id":item_id(&native),"threadId":subagent.child_thread_id,"runId":null,"nodeId":subagent.child_root_node_id,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":native_ref(&native),"parentItemId":null,"ordinal":ordinal,"type":"reasoning","title":"Thinking","text":text,"streaming":false,"status":"completed","startedAt":now,"completedAt":now,"updatedAt":now});
                commit(p.store, &subagent.child_thread_id, |_| Ok(vec![("turn-item.updated", item)]))?;
            }
            let text: String = blocks.iter().filter(|block| block["type"] == "text").filter_map(|block| block["text"].as_str()).collect();
            if !text.is_empty() {
                let subagent = self.by_task.get_mut(&task_id).unwrap();
                let native_message = message["message"]["id"].as_str().map(str::to_owned);
                subagent.last_assistant_text = Some(match (&native_message, &subagent.last_assistant_message_id) {
                    (Some(current), Some(last)) if current == last => format!("{}\n{text}", subagent.last_assistant_text.as_deref().unwrap_or_default()),
                    _ => text.clone(),
                });
                subagent.last_assistant_message_id = native_message;
                let ordinal = Projector::arrival(&thread_view(p.store, &subagent.child_thread_id)?, &item_id(uuid));
                let (row, item) = subagents::conversation_artifacts(
                    &json!({"messageId":message_id(uuid),"turnItemId":item_id(uuid),"threadId":subagent.child_thread_id,"rootNodeId":subagent.child_root_node_id,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":native_ref(uuid),"role":"assistant","text":text,"ordinal":ordinal}),
                    &now,
                );
                commit(p.store, &subagent.child_thread_id, |_| Ok(vec![("message.updated", row), ("turn-item.updated", item)]))?;
            }
        }
        Ok(())
    }
    fn child_owning_tool(&self, tool_use_id: &str) -> Option<String> {
        self.by_task.iter().find(|(_, subagent)| subagent.projector.has_tool(tool_use_id)).map(|(id, _)| id.clone())
    }

    /// Ends the calls subagents left open once the process that ran them is gone.
    pub(crate) fn end_open_calls(&mut self, store: &Store, status: &str) -> Result<(), StoreError> {
        for subagent in self.by_task.values_mut() {
            if !subagent.projector.has_open_tools() {
                continue;
            }
            let view = thread_view(store, &subagent.child_thread_id)?;
            let events = subagent.projector.end_tools(&view, status)?;
            commit(store, &subagent.child_thread_id, |_| Ok(events))?;
        }
        Ok(())
    }
    /// A process that ends takes its running subagents with it: the CLI never reports them again, and
    /// a later `task_started` reopens them.
    pub(crate) fn end_running(&mut self) {
        for subagent in self.by_task.values_mut() {
            if subagent.task["status"] == "running" {
                subagent.task["status"] = json!("interrupted");
            }
        }
    }
}
