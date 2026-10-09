//! App-owned delegated tasks: the spawn of a child thread, the completion cohort a parent run owes,
//! its delivery to the parent as a queued wake message, and acknowledging or disposing a delivery.
//! Ports `dispatchDelegatedTaskRequest`, `planDelegatedCompletionDelivery`,
//! `finalizeAppOwnedSubagent`, `finalizeDelegatedCompletionDelivery`, the wake-policy, acknowledge
//! and dispose commands, and the continuation that dispatches the wake (`ProviderContinuationService`).
//! Cohort disposal on Stop, cancel and delete lives in `delegated_tasks.rs`.
//!
//! Provider-agnostic: nothing here names a provider. A terminal run on a delegated child or on a wake
//! run reaches this module through [`start_listener`], which follows the store's committed events.
//! Not ported: the mailbox's native steering of a wake into an active turn (a wake always queues
//! behind the parent's active run) and the restart-continuation hold on a child's result.
use crate::{
    execution::{ExecutionService, at, checked, event},
    mcp_control::encode,
    persistence::{Decision, Event, Receipt, Store, StoreError, read_projection},
    subagents, thread,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}
fn blocking(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting"))
}
fn live(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("preparing" | "starting" | "running"))
}
/// The status a delegated task takes from its child run's, or none while the run is not over.
fn terminal_status(status: &str) -> Option<&'static str> {
    match status {
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "cancelled" | "rolled_back" => Some("cancelled"),
        "interrupted" => Some("interrupted"),
        _ => None,
    }
}
pub(crate) fn task_terminal(status: &Value) -> bool {
    matches!(status.as_str(), Some("completed" | "failed" | "cancelled" | "interrupted"))
}
fn delivered(task: &Value) -> bool {
    matches!(task["completionDelivery"]["state"].as_str(), Some("acknowledged" | "delivered" | "disposed"))
}

/// `delegatedCompletionWakeDetail`: what the parent is told when its tasks finish.
pub(crate) fn wake_text(task_ids: &[Value]) -> String {
    let list = task_ids.iter().map(|id| id.as_str().unwrap_or_default()).collect::<Vec<_>>().join(", ");
    if task_ids.len() == 1 {
        format!("Delegated task {list} reached a terminal state. Use task_status with taskId {list} to read the result.")
    } else {
        format!("Delegated tasks {list} reached terminal states. Use task_status with each taskId to read the results.")
    }
}

fn delivery_run<'a>(projection: &'a Value, delivery: &Value) -> Option<&'a Value> {
    if delivery.is_null() {
        return None;
    }
    let message = rows(projection, "messages").iter().find(|message| message["id"] == delivery["messageId"])?;
    rows(projection, "runs").iter().find(|run| run["id"] == message["runId"])
}
fn delivery_message<'a>(projection: &'a Value, delivery: &Value) -> Option<&'a Value> {
    rows(projection, "messages").iter().find(|message| message["id"] == delivery["messageId"])
}
fn with_ids(delivery: &Value, task_id: &Value) -> Vec<Value> {
    let mut ids: Vec<Value> = delivery["taskIds"].as_array().cloned().unwrap_or_default();
    if !ids.contains(task_id) {
        ids.push(task_id.clone());
    }
    ids
}
fn message_id(thread_id: &str, count: usize) -> String {
    format!("message:thread:{}:ordinal:{}:{}", encode(thread_id), count + 1, uuid::Uuid::new_v4())
}

/// The part of a message command that makes it a delegated-completion delivery: only the server's
/// own queued message for the cohort's current delivery is accepted, and its text is the wake text
/// of the tasks that delivery owes now.
pub(crate) fn admit_message(command: &mut Value, projection: &Value) -> Result<(), StoreError> {
    let Some(requested) = command.get("delegatedCompletion").filter(|requested| !requested.is_null()).cloned() else { return Ok(()) };
    if command["createdBy"] != "agent" || command["creationSource"] != "server" || command["dispatchMode"]["type"] != "queue_after_active" {
        return Err(StoreError::InvalidCommand("Delegated completion delivery must be a server-created queued message.".into()));
    }
    let parent = rows(projection, "runs").iter().find(|run| run["id"] == requested["parentRunId"]);
    let cohort = parent.map(|run| &run["delegatedCompletion"]);
    let delivery = cohort.map(|cohort| &cohort["delivery"]).filter(|delivery| delivery.is_object());
    let exists = rows(projection, "messages").iter().any(|message| message["id"] == command["messageId"]);
    let (Some(cohort), Some(delivery)) = (cohort, delivery) else {
        return Err(StoreError::InvalidCommand("Delegated completion delivery is no longer dispatchable.".into()));
    };
    if cohort["disposition"] != "open" || delivery["generation"] != requested["generation"] || delivery["messageId"] != command["messageId"] || exists {
        return Err(StoreError::InvalidCommand("Delegated completion delivery is no longer dispatchable.".into()));
    }
    let ids = delivery["taskIds"].as_array().cloned().unwrap_or_default();
    command["text"] = json!(wake_text(&ids));
    command["delegatedCompletion"] = json!({"parentRunId":requested["parentRunId"],"generation":delivery["generation"],"taskIds":ids});
    Ok(())
}

// ---- planning --------------------------------------------------------------------------------------

struct Plan {
    task: Value,
    parent_run: Option<Value>,
    message: Option<Value>,
    offer: bool,
}

/// `planDelegatedCompletionDelivery`: what a task that just ended owes its parent's cohort. `task` is
/// the row before, `updated` the row with its terminal status.
fn plan_delivery(parent: &Value, parent_run: Option<&Value>, task: &Value, updated: &Value, now: &str, fresh_message_id: impl FnOnce() -> String) -> Plan {
    let plain = |task: Value| Plan { task, parent_run: None, message: None, offer: false };
    let with_state = |state: &str| {
        let mut task = updated.clone();
        task["completionDelivery"] = json!({"state":state,"observedByRunId":null});
        task
    };
    if delivered(updated) {
        return plain(updated.clone());
    }
    let cohort = parent_run.map(|run| &run["delegatedCompletion"]).filter(|cohort| cohort.is_object());
    if parent_run.is_none() || !parent["thread"]["archivedAt"].is_null() || !parent["thread"]["deletedAt"].is_null() || cohort.is_some_and(|cohort| cohort["disposition"] != "open") {
        return plain(with_state("disposed"));
    }
    let parent_run = parent_run.expect("checked above");
    // settled_only defers to a blocking delegate_task wait, which lives only as long as the turn
    // that spawned the task. Once that turn is over nothing else delivers the result.
    if task["completionWake"].as_str().unwrap_or("settled_only") == "settled_only" && live(parent_run) {
        return plain(updated.clone());
    }
    let with_cohort = |cohort: Value| {
        let mut run = parent_run.clone();
        run["delegatedCompletion"] = cohort;
        run
    };
    if let Some(cohort) = cohort {
        let delivery = &cohort["delivery"];
        if delivery.is_object() {
            let run = delivery_run(parent, delivery);
            let ids = with_ids(delivery, &task["id"]);
            let mut next = cohort.clone();
            next["delivery"] = json!({"generation":delivery["generation"],"messageId":delivery["messageId"],"taskIds":ids});
            if let Some(run) = run {
                if run["status"] == "queued" && run["userMessageId"] == delivery["messageId"] {
                    let message = delivery_message(parent, delivery).map(|message| {
                        let mut message = message.clone();
                        message["text"] = json!(wake_text(&ids));
                        message["delegatedCompletion"] = json!({"parentRunId":parent_run["id"],"generation":delivery["generation"],"taskIds":ids});
                        message["updatedAt"] = json!(now);
                        message
                    });
                    return Plan { task: with_state("claimed"), parent_run: Some(with_cohort(next)), message, offer: false };
                }
                // The terminal-run listener owns reconciliation of a delivery run that started: a
                // sibling stays pending for the successor it reserves.
                return plain(with_state("pending"));
            }
            return Plan { task: with_state("claimed"), parent_run: Some(with_cohort(next)), message: None, offer: true };
        }
    }
    let generation = cohort.map_or(json!(1), |cohort| cohort["nextGeneration"].clone());
    let next = json!({"disposition":"open","nextGeneration":generation.as_u64().unwrap_or(1) + 1,"delivery":{"generation":generation,"messageId":fresh_message_id(),"taskIds":[task["id"]]}});
    Plan { task: with_state("claimed"), parent_run: Some(with_cohort(next)), message: None, offer: true }
}

/// Raised inside a decision that has nothing to commit, so no receipt is recorded for it.
const NOTHING: &str = "delegated task: nothing to do";

fn quiet(result: Result<Receipt, StoreError>) -> Result<Option<Receipt>, StoreError> {
    match result {
        Err(StoreError::InvalidCommand(message)) if message == NOTHING => Ok(None),
        other => other.map(Some),
    }
}

fn provider_turn_for_run<'a>(projection: &'a Value, run: &Value) -> Option<&'a Value> {
    let attempt = run["activeAttemptId"].as_str()?;
    rows(projection, "providerTurns").iter().rev().find(|turn| turn["runAttemptId"] == attempt).or_else(|| {
        let attempt = rows(projection, "attempts").iter().find(|candidate| candidate["id"] == attempt)?;
        rows(projection, "providerTurns").iter().find(|turn| turn["id"] == attempt["providerTurnId"])
    })
}
/// `contextSourcePointForRun`.
fn source_point(projection: &Value, run: &Value) -> Value {
    let mut point = json!({"threadId":projection["thread"]["id"],"runId":run["id"]});
    if !run["checkpointId"].is_null() {
        point["checkpointId"] = run["checkpointId"].clone();
    }
    if let Some(reference) = rows(projection, "providerThreads").iter().find(|row| row["id"] == run["providerThreadId"]).map(|row| &row["nativeThreadRef"]).filter(|reference| !reference.is_null()) {
        point["providerThreadRef"] = reference.clone();
    }
    if let Some(reference) = provider_turn_for_run(projection, run).map(|turn| &turn["nativeTurnRef"]).filter(|reference| !reference.is_null()) {
        point["providerTurnRef"] = reference.clone();
    }
    point
}

// ---- the service -----------------------------------------------------------------------------------

/// The commands on delegated tasks, over the store and the execution service that runs their children.
#[derive(Clone)]
pub struct DelegatedTasks {
    execution: ExecutionService,
}

impl DelegatedTasks {
    pub fn new(execution: ExecutionService) -> Self {
        Self { execution }
    }
    fn store(&self) -> &Store {
        &self.execution.store
    }

    /// `delegated_task.request`: composes the child thread, its first message, the parent's task row,
    /// node and timeline item, and the `subagent_spawn` context transfer, in one commit.
    pub fn request(&self, command: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        let parent_id = command["parentThreadId"].as_str().unwrap_or_default().to_owned();
        let command_id = command["commandId"].as_str().unwrap_or_default().to_owned();
        let (receipt, replayed) = self.store().dispatch_resolved(
            &command_id,
            "thread",
            "delegated_task.request",
            now,
            |_, _| Ok(parent_id.clone()),
            |transaction, _| {
                let parent = read_projection(transaction, "thread", &parent_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                self.plan_request(command, &parent, now)
            },
            thread::reduce,
        )?;
        if !replayed {
            self.execution.wake();
        }
        Ok(receipt)
    }

    fn plan_request(&self, command: &Value, parent: &Value, now: DateTime<Utc>) -> Result<Decision, StoreError> {
        let invalid = |cause: String| StoreError::InvalidCommand(cause);
        let (parent_id, command_id) = (command["parentThreadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default());
        let parent_run = rows(parent, "runs").iter().find(|run| run["id"] == command["parentRunId"]).filter(|run| blocking(run)).ok_or_else(|| invalid(format!("Parent run {} is not active.", command["parentRunId"].as_str().unwrap_or_default())))?;
        // Stop reached the run: its agent can still call tools until the provider stops it, but nothing such a call starts may outlive the Stop.
        if crate::thread_pull_requests::stop_reached_run(parent, parent_run["id"].as_str().unwrap_or_default()) {
            return Err(invalid(format!("Parent run {} is stopping.", command["parentRunId"].as_str().unwrap_or_default())));
        }
        let parent_node = rows(parent, "nodes").iter().find(|node| node["id"] == command["parentNodeId"]).filter(|node| node["runId"] == parent_run["id"] && !parent_run["rootNodeId"].is_null());
        if parent_node.is_none() {
            return Err(invalid(format!("Parent node {} is not part of active run {}.", command["parentNodeId"].as_str().unwrap_or_default(), parent_run["id"].as_str().unwrap_or_default())));
        }
        let model = &command["modelSelection"];
        let driver = self.execution.providers.driver(model["instanceId"].as_str().unwrap_or_default()).map_err(|error| invalid(error.to_string()))?;
        let stamp = at(now);
        let node_id = format!("node:delegated-task:{}", encode(command_id));
        let child_id = format!("thread:delegated-task:{}", encode(command_id));
        let child_message_id = format!("message:delegated-task:{}", encode(command_id));
        let item_id = format!("turn-item:delegated-task:{}", encode(command_id));
        let prompt = command["task"].as_str().unwrap_or_default();
        let title = subagents::thread_title(parent["thread"]["title"].as_str().unwrap_or_default(), &command["title"], prompt, rows(parent, "subagents").len() as u64 + 1);
        let mut child = subagents::child_thread(
            &parent["thread"],
            &json!({"createdBy":command["createdBy"],"creationSource":command["creationSource"],"childThreadId":child_id,"title":title,"providerInstanceId":model["instanceId"],"modelSelection":model,"activeProviderThreadId":null,"parentNodeId":node_id}),
            &stamp,
        );
        child["runtimeMode"] = command["runtimeMode"].clone();
        child["interactionMode"] = command["interactionMode"].clone();
        let mut task = json!({"id":node_id,"threadId":parent_id,"runId":parent_run["id"],"parentNodeId":command["parentNodeId"],"origin":"app_owned","createdBy":command["createdBy"],"driver":driver,"providerInstanceId":model["instanceId"],"providerThreadId":null,"childThreadId":child_id,"nativeTaskRef":null,"prompt":prompt,"title":command["title"].as_str().map_or(Value::Null,|title|json!(title)),"model":model["model"],"status":"running","result":null,"startedAt":stamp,"completedAt":null,"updatedAt":stamp});
        if let Some(wake) = command.get("completionWake").filter(|wake| !wake.is_null()) {
            task["completionWake"] = wake.clone();
        }
        let node = json!({"id":node_id,"threadId":parent_id,"runId":parent_run["id"],"parentNodeId":command["parentNodeId"],"rootNodeId":parent_run["rootNodeId"],"kind":"subagent","status":"running","countsForRun":false,"providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":stamp,"completedAt":null});
        let item = json!({"id":item_id,"threadId":parent_id,"runId":parent_run["id"],"nodeId":node_id,"providerThreadId":parent_run["providerThreadId"],"providerTurnId":provider_turn_for_run(parent, parent_run).map_or(Value::Null,|turn|turn["id"].clone()),"nativeItemRef":null,"parentItemId":null,"ordinal":rows(parent,"turnItems").iter().filter_map(|item|item["ordinal"].as_u64()).max().map_or(0,|ordinal|ordinal+1),"status":"running","title":command["title"].as_str().map_or(json!(title),|title|json!(title)),"startedAt":stamp,"completedAt":null,"updatedAt":stamp,"type":"subagent","subagentId":node_id,"origin":"app_owned","driver":driver,"providerInstanceId":model["instanceId"],"childThreadId":child_id,"prompt":prompt,"result":null});
        let created = event(&child_id, command_id, "thread.created", t3_contracts::normalize_event_payload("thread.created", child.clone()).map_err(|error| invalid(error.to_string()))?, now);
        let child_projection = thread::projection_after(None, &created)?;
        let mut events: Vec<Event> = vec![created];
        events.push(event(parent_id, command_id, "node.updated", checked::<t3_contracts::ExecutionNode>(node)?, now));
        events.push(event(parent_id, command_id, "subagent.updated", checked::<t3_contracts::Subagent>(task)?, now));
        events.push(event(parent_id, command_id, "turn-item.updated", checked::<t3_contracts::TurnItem>(item)?, now));
        let message = json!({"type":"message.dispatch","createdBy":command["createdBy"],"creationSource":command["creationSource"],"commandId":command_id,"threadId":child_id,"senderThreadId":parent_id,"messageId":child_message_id,"text":prompt,"attachments":[],"modelSelection":model,"dispatchMode":{"type":"start_immediately"}});
        let (child_events, effects) = match crate::execution::plan_message_for_driver(&message, &child_projection, now, driver)? {
            Decision::Accepted { events, effects } => (events, effects),
            _ => return Err(invalid(format!("Delegated child thread {child_id} did not create a run."))),
        };
        let child_run = child_events.iter().find(|event| event.event_type == "run.created").map(|event| event.payload.clone()).ok_or_else(|| invalid(format!("Delegated child thread {child_id} did not create a run.")))?;
        events.extend(child_events);
        let mut spawn_point = source_point(parent, parent_run);
        spawn_point["turnItemId"] = json!(item_id);
        let transfer = json!({"id":format!("context-transfer:type:subagent_spawn:source-thread:{}:target-thread:{}:{}",encode(parent_id),encode(&child_id),uuid::Uuid::new_v4()),"type":"subagent_spawn","sourceThreadId":parent_id,"targetThreadId":child_id,"sourcePoint":spawn_point,"basePoint":null,"sourceProviderInstanceId":parent_run["providerInstanceId"],"targetProviderInstanceId":model["instanceId"],"targetRunId":child_run["id"],"status":"consumed","resolution":null,"createdBy":command["createdBy"],"error":null,"createdAt":stamp,"updatedAt":stamp,"consumedAt":stamp});
        events.push(event(&child_id, command_id, "context-transfer.created", checked::<t3_contracts::ContextTransfer>(transfer)?, now));
        Ok(Decision::Accepted { events, effects })
    }

    /// `delegated_task.wake-policy`: rewrites a task's `completionWake` after it was created, which a
    /// blocking wait does when its window ends. A task that already ended gets the delivery it
    /// skipped when its spawning run is live.
    pub fn wake_policy(&self, command: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        let parent_id = command["parentThreadId"].as_str().unwrap_or_default().to_owned();
        let command_id = command["commandId"].as_str().unwrap_or_default().to_owned();
        let offer = std::cell::RefCell::new(None);
        let (receipt, replayed) = self.store().dispatch_resolved(
            &command_id,
            "thread",
            "delegated_task.wake-policy",
            now,
            |_, _| Ok(parent_id.clone()),
            |transaction, _| {
                let parent = read_projection(transaction, "thread", &parent_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                let task = rows(&parent, "subagents").iter().find(|task| task["id"] == command["taskId"] && task["origin"] == "app_owned").ok_or_else(|| StoreError::InvalidCommand(format!("Delegated task {} is not an app-owned task of thread {parent_id}.", command["taskId"].as_str().unwrap_or_default())))?;
                if task["completionWake"] == command["completionWake"] {
                    return Err(StoreError::InvalidCommand(format!("Delegated task {} already wakes the parent with completionWake {}.", command["taskId"].as_str().unwrap_or_default(), command["completionWake"].as_str().unwrap_or_default())));
                }
                let stamp = at(now);
                let mut updated = task.clone();
                updated["completionWake"] = command["completionWake"].clone();
                updated["updatedAt"] = json!(stamp);
                let parent_run = task["runId"].as_str().and_then(|id| rows(&parent, "runs").iter().find(|run| run["id"] == id));
                // A terminal task needs a delivery only when its spawning run is live now: that is
                // exactly when finalize skipped it. Otherwise finalize already offered.
                let plan = (command["completionWake"] == "always" && task_terminal(&task["status"]) && parent_run.is_some_and(live))
                    .then(|| plan_delivery(&parent, parent_run, &updated, &updated, &stamp, || message_id(&parent_id, rows(&parent, "messages").len())));
                let mut events = vec![event(&parent_id, &command_id, "subagent.updated", checked::<t3_contracts::Subagent>(plan.as_ref().map_or(updated.clone(), |plan| plan.task.clone()))?, now)];
                if let Some(plan) = &plan {
                    if let Some(run) = &plan.parent_run {
                        events.push(event(&parent_id, &command_id, "run.updated", checked::<t3_contracts::Run>(run.clone())?, now));
                    }
                    if let Some(message) = &plan.message {
                        events.push(event(&parent_id, &command_id, "message.updated", checked::<t3_contracts::ConversationMessage>(message.clone())?, now));
                    }
                    if plan.offer {
                        *offer.borrow_mut() = plan.parent_run.as_ref().map(|run| run["id"].as_str().unwrap_or_default().to_owned());
                    }
                }
                Ok(Decision::Accepted { events, effects: vec![] })
            },
            thread::reduce,
        )?;
        if !replayed {
            if let Some(run_id) = offer.into_inner() {
                self.offer(&parent_id, &run_id);
            }
        }
        Ok(receipt)
    }

    /// `delegated_task.completion-delivery.acknowledge` and `.dispose`: the parent has read a task's
    /// result, or will never be woken for it. The wake still queued for it shrinks or is cancelled.
    pub fn resolve_delivery(&self, command: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        let parent_id = command["parentThreadId"].as_str().unwrap_or_default().to_owned();
        let command_id = command["commandId"].as_str().unwrap_or_default().to_owned();
        let acknowledge = command["type"] == "delegated_task.completion-delivery.acknowledge";
        let (receipt, _) = self.store().dispatch_resolved(
            &command_id,
            "thread",
            if acknowledge { "delegated_task.completion-delivery.acknowledge" } else { "delegated_task.completion-delivery.dispose" },
            now,
            |_, _| Ok(parent_id.clone()),
            |transaction, _| {
                let parent = read_projection(transaction, "thread", &parent_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                self.plan_resolution(command, &parent, acknowledge, now)
            },
            thread::reduce,
        )?;
        Ok(receipt)
    }

    fn plan_resolution(&self, command: &Value, parent: &Value, acknowledge: bool, now: DateTime<Utc>) -> Result<Decision, StoreError> {
        let (parent_id, command_id) = (command["parentThreadId"].as_str().unwrap_or_default(), command["commandId"].as_str().unwrap_or_default());
        let task = rows(parent, "subagents").iter().find(|task| task["id"] == command["taskId"] && task["origin"] == "app_owned").ok_or_else(|| StoreError::InvalidCommand(format!("Delegated task {} is not an app-owned task of thread {parent_id}.", command["taskId"].as_str().unwrap_or_default())))?;
        let state = if acknowledge { "acknowledged" } else { "disposed" };
        let stamp = at(now);
        let current = task["completionDelivery"]["state"].as_str();
        let subagent = |task: &Value| -> Result<Event, StoreError> { Ok(event(parent_id, command_id, "subagent.updated", checked::<t3_contracts::Subagent>(task.clone())?, now)) };
        // task_status and t3_thread_read use distinct command ids, so two valid observations can race:
        // re-emitting the row makes the second a successful no-op.
        if current == Some(state) || (acknowledge && current == Some("disposed")) {
            return Ok(Decision::Accepted { events: vec![subagent(task)?], effects: vec![] });
        }
        let mut updated = task.clone();
        updated["completionDelivery"] = json!({"state":state,"observedByRunId":if acknowledge {command["observedByRunId"].clone()} else {Value::Null}});
        updated["updatedAt"] = json!(stamp);
        let mut events = vec![subagent(&updated)?];
        let parent_run = task["runId"].as_str().and_then(|id| rows(parent, "runs").iter().find(|run| run["id"] == id));
        let cohort = parent_run.map(|run| &run["delegatedCompletion"]).filter(|cohort| cohort.is_object());
        let (Some(run), Some(cohort)) = (parent_run, cohort) else { return Ok(Decision::Accepted { events, effects: vec![] }) };
        let delivery = &cohort["delivery"];
        if !delivery.is_object() || !delivery["taskIds"].as_array().is_some_and(|ids| ids.contains(&task["id"])) {
            return Ok(Decision::Accepted { events, effects: vec![] });
        }
        let remaining: Vec<Value> = delivery["taskIds"].as_array().unwrap().iter().filter(|id| **id != task["id"]).cloned().collect();
        let delivering = delivery_run(parent, delivery);
        let clear = remaining.is_empty() && delivering.is_none_or(|run| run["status"] == "queued");
        let mut next = run.clone();
        next["delegatedCompletion"] = json!({"disposition":cohort["disposition"],"nextGeneration":cohort["nextGeneration"],"delivery":if clear {Value::Null} else {json!({"generation":delivery["generation"],"messageId":delivery["messageId"],"taskIds":remaining})}});
        events.push(event(parent_id, command_id, "run.updated", checked::<t3_contracts::Run>(next)?, now));
        if let Some(delivering) = delivering.filter(|run| run["status"] == "queued") {
            if remaining.is_empty() {
                for (kind, payload) in crate::delegated_tasks::queued_run_cancellation(parent, delivering, &stamp) {
                    events.extend(crate::delegated_tasks::to_events(parent_id, command_id, vec![(kind, payload)], now)?);
                }
            } else if let Some(message) = delivery_message(parent, delivery) {
                let mut message = message.clone();
                message["text"] = json!(wake_text(&remaining));
                message["delegatedCompletion"] = json!({"parentRunId":run["id"],"generation":delivery["generation"],"taskIds":remaining});
                message["updatedAt"] = json!(stamp);
                events.push(event(parent_id, command_id, "message.updated", checked::<t3_contracts::ConversationMessage>(message)?, now));
            }
        }
        Ok(Decision::Accepted { events, effects: vec![] })
    }

    /// `thread.created.record`: the card in the parent's timeline for a thread one of its runs created.
    pub fn record_created_thread(&self, command: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        let parent_id = command["parentThreadId"].as_str().unwrap_or_default().to_owned();
        let command_id = command["commandId"].as_str().unwrap_or_default().to_owned();
        let (receipt, _) = self.store().dispatch_resolved(
            &command_id,
            "thread",
            "thread.created.record",
            now,
            |_, _| Ok(parent_id.clone()),
            |transaction, _| {
                let invalid = |cause: String| StoreError::InvalidCommand(cause);
                let parent = read_projection(transaction, "thread", &parent_id)?.ok_or_else(|| invalid("Thread not found.".into()))?;
                let target_id = command["targetThreadId"].as_str().unwrap_or_default();
                let target = read_projection(transaction, "thread", target_id)?.ok_or_else(|| invalid("Thread not found.".into()))?;
                let (run_id, node_id) = (command["parentRunId"].as_str().unwrap_or_default(), command["parentNodeId"].as_str().unwrap_or_default());
                let run = rows(&parent, "runs").iter().find(|run| run["id"] == run_id);
                let node = rows(&parent, "nodes").iter().find(|node| node["id"] == node_id);
                let (Some(run), Some(_)) = (run, node.filter(|node| node["runId"] == run_id)) else { return Err(invalid(format!("Parent node {node_id} is not the root of run {run_id}."))) };
                if run["rootNodeId"] != node_id {
                    return Err(invalid(format!("Parent node {node_id} is not the root of run {run_id}.")));
                }
                if parent["thread"]["projectId"] != target["thread"]["projectId"] {
                    return Err(invalid(format!("Target thread {target_id} belongs to another project.")));
                }
                if !command["targetRunId"].is_null() && !rows(&target, "runs").iter().any(|candidate| candidate["id"] == command["targetRunId"]) {
                    return Err(invalid(format!("Target run {} does not belong to thread {target_id}.", command["targetRunId"].as_str().unwrap_or_default())));
                }
                let stamp = at(now);
                let item = json!({"id":format!("turn-item:created-thread:{}",encode(&command_id)),"threadId":parent_id,"runId":run_id,"nodeId":node_id,"providerThreadId":run["providerThreadId"],"providerTurnId":provider_turn_for_run(&parent, run).map_or(Value::Null,|turn|turn["id"].clone()),"nativeItemRef":null,"parentItemId":null,"ordinal":rows(&parent,"turnItems").iter().filter_map(|item|item["ordinal"].as_u64()).max().map_or(0,|ordinal|ordinal+1),"status":"completed","title":target["thread"]["title"],"startedAt":stamp,"completedAt":stamp,"updatedAt":stamp,"type":"thread_created","targetThreadId":target_id,"targetRunId":command["targetRunId"],"targetProviderInstanceId":target["thread"]["modelSelection"]["instanceId"],"targetModel":target["thread"]["modelSelection"]["model"]});
                Ok(Decision::Accepted { events: vec![event(&parent_id, &command_id, "turn-item.updated", checked::<t3_contracts::TurnItem>(item)?, now)], effects: vec![] })
            },
            thread::reduce,
        )?;
        Ok(receipt)
    }

    // ---- completion ------------------------------------------------------------------------------

    /// `finalizeAppOwnedSubagent`: when a delegated child's work has ended, its result becomes the
    /// task's result in the parent, with a `subagent_result` transfer, and the parent is offered a
    /// wake. A child with a result transfer already is finalized; one still working is left alone.
    pub fn finalize_child(&self, child_id: &str) -> Result<(), StoreError> {
        let now = Utc::now();
        let command_id = format!("delegated-task-finalize:{child_id}:{}", uuid::Uuid::new_v4());
        let offer = std::cell::RefCell::new(None::<(String, String)>);
        let parent_id = std::cell::RefCell::new(String::new());
        let result = self.store().dispatch_resolved(
            &command_id,
            "thread",
            "delegated_task.finalize",
            now,
            |transaction, _| {
                let child = read_projection(transaction, "thread", child_id)?.ok_or_else(|| StoreError::InvalidCommand(NOTHING.into()))?;
                let parent = child["thread"]["lineage"]["parentThreadId"].as_str().ok_or_else(|| StoreError::InvalidCommand(NOTHING.into()))?.to_owned();
                *parent_id.borrow_mut() = parent.clone();
                Ok(parent)
            },
            |transaction, parent_thread| {
                let nothing = || StoreError::InvalidCommand(NOTHING.into());
                let child = read_projection(transaction, "thread", child_id)?.ok_or_else(nothing)?;
                let forked = &child["thread"]["forkedFrom"];
                if child["thread"]["lineage"]["relationshipToParent"] != "subagent" || forked["type"] != "node" {
                    return Err(nothing());
                }
                let (state, result_run) = subagents::delegated_task_progress(&child);
                let result_run = result_run.filter(|_| state == "result_available").and_then(|id| rows(&child, "runs").iter().find(|run| run["id"] == id)).ok_or_else(nothing)?;
                let status = terminal_status(result_run["status"].as_str().unwrap_or_default()).ok_or_else(nothing)?;
                let parent = read_projection(transaction, "thread", parent_thread)?.ok_or_else(nothing)?;
                let task = rows(&parent, "subagents").iter().find(|task| task["id"] == forked["nodeId"] && task["origin"] == "app_owned" && task["childThreadId"] == child_id).ok_or_else(nothing)?;
                if rows(&parent, "contextTransfers").iter().any(|transfer| transfer["type"] == "subagent_result" && transfer["sourceThreadId"] == child_id && transfer["targetThreadId"] == parent_thread) {
                    return Err(nothing());
                }
                let stamp = at(now);
                let result = subagents::result_for_run(&child, result_run);
                let parent_run = task["runId"].as_str().and_then(|id| rows(&parent, "runs").iter().find(|run| run["id"] == id));
                let parent_node = rows(&parent, "nodes").iter().find(|node| node["id"] == task["id"]);
                let parent_item = rows(&parent, "turnItems").iter().find(|item| item["type"] == "subagent" && item["subagentId"] == task["id"]);
                let mut updated = task.clone();
                updated["providerThreadId"] = result_run["providerThreadId"].clone();
                updated["status"] = json!(status);
                updated["result"] = result["text"].clone();
                updated["completedAt"] = json!(stamp);
                updated["updatedAt"] = json!(stamp);
                let plan = plan_delivery(&parent, parent_run, task, &updated, &stamp, || message_id(parent_thread, rows(&parent, "messages").len()));
                let transfer_id = format!("context-transfer:type:subagent_result:source-thread:{}:target-thread:{}:{}", encode(child_id), encode(parent_thread), uuid::Uuid::new_v4());
                let child_provider = rows(&child, "providerThreads").iter().find(|row| row["id"] == result_run["providerThreadId"]);
                let parent_provider = parent_run.and_then(|run| rows(&parent, "providerThreads").iter().find(|row| row["id"] == run["providerThreadId"]));
                let handoff = match (parent_run, child_provider, parent_provider) {
                    (Some(parent_run), Some(child_provider), Some(parent_provider)) => Some(json!({"id":format!("context-handoff:thread:{}:from-provider-instance:{}:to-provider-instance:{}:{}",encode(parent_thread),encode(result_run["providerInstanceId"].as_str().unwrap_or_default()),encode(parent_run["providerInstanceId"].as_str().unwrap_or_default()),uuid::Uuid::new_v4()),"transferId":transfer_id,"threadId":parent_thread,"targetRunId":parent_run["id"],"fromProviderThreadIds":[child_provider["id"]],"toProviderThreadId":parent_provider["id"],"coveredRunOrdinals":{"from":result_run["ordinal"],"to":result_run["ordinal"]},"strategy":"manual_context","status":"ready","summaryMessageId":result["messageId"],"summaryText":result["text"],"createdByProviderInstanceId":result_run["providerInstanceId"],"createdAt":stamp,"updatedAt":stamp})),
                    _ => None,
                };
                let mut point = source_point(&child, result_run);
                if !result["turnItemId"].is_null() {
                    point["turnItemId"] = result["turnItemId"].clone();
                }
                let transfer = json!({"id":transfer_id,"type":"subagent_result","sourceThreadId":child_id,"targetThreadId":parent_thread,"sourcePoint":point,"basePoint":null,"sourceProviderInstanceId":result_run["providerInstanceId"],"targetProviderInstanceId":parent_run.map_or(parent["thread"]["providerInstanceId"].clone(),|run|run["providerInstanceId"].clone()),"targetRunId":parent_run.map_or(Value::Null,|run|run["id"].clone()),"status":"consumed","resolution":handoff.as_ref().map_or(Value::Null,|handoff|json!({"strategy":"portable_context","contextHandoffId":handoff["id"]})),"createdBy":"system","error":null,"createdAt":stamp,"updatedAt":stamp,"consumedAt":stamp});
                let mut events = vec![event(parent_thread, &command_id, "subagent.updated", checked::<t3_contracts::Subagent>(plan.task.clone())?, now)];
                if let Some(run) = &plan.parent_run {
                    events.push(event(parent_thread, &command_id, "run.updated", checked::<t3_contracts::Run>(run.clone())?, now));
                }
                if let Some(message) = &plan.message {
                    events.push(event(parent_thread, &command_id, "message.updated", checked::<t3_contracts::ConversationMessage>(message.clone())?, now));
                }
                if let Some(node) = parent_node {
                    let mut node = node.clone();
                    node["status"] = json!(status);
                    node["providerThreadId"] = result_run["providerThreadId"].clone();
                    node["completedAt"] = json!(stamp);
                    events.push(event(parent_thread, &command_id, "node.updated", checked::<t3_contracts::ExecutionNode>(node)?, now));
                }
                if let Some(item) = parent_item {
                    let mut item = item.clone();
                    item["status"] = json!(status);
                    item["result"] = result["text"].clone();
                    item["completedAt"] = json!(stamp);
                    item["updatedAt"] = json!(stamp);
                    events.push(event(parent_thread, &command_id, "turn-item.updated", checked::<t3_contracts::TurnItem>(item)?, now));
                }
                if let Some(handoff) = handoff {
                    events.push(event(parent_thread, &command_id, "context-handoff.updated", checked::<t3_contracts::ContextHandoff>(handoff)?, now));
                }
                events.push(event(parent_thread, &command_id, "context-transfer.created", checked::<t3_contracts::ContextTransfer>(transfer)?, now));
                if plan.offer {
                    *offer.borrow_mut() = plan.parent_run.as_ref().map(|run| (parent_thread.to_owned(), run["id"].as_str().unwrap_or_default().to_owned()));
                }
                Ok(Decision::Accepted { events, effects: vec![] })
            },
            thread::reduce,
        );
        quiet(result.map(|(receipt, _)| receipt))?;
        if let Some((parent, run)) = offer.into_inner() {
            self.offer(&parent, &run);
        }
        Ok(())
    }

    /// `finalizeDelegatedCompletionDelivery`: a wake run ended. The tasks it carried are delivered (or
    /// pending again when it was cancelled), and results that arrived meanwhile go out together in
    /// one successor.
    pub fn finalize_delivery(&self, thread_id: &str, run_id: &str) -> Result<(), StoreError> {
        let now = Utc::now();
        let command_id = format!("delegated-delivery-finalize:{run_id}:{}", uuid::Uuid::new_v4());
        let offer = std::cell::RefCell::new(None::<String>);
        let result = self.store().dispatch_resolved(
            &command_id,
            "thread",
            "delegated_task.delivery-finalize",
            now,
            |_, _| Ok(thread_id.to_owned()),
            |transaction, _| {
                let nothing = || StoreError::InvalidCommand(NOTHING.into());
                let projection = read_projection(transaction, "thread", thread_id)?.ok_or_else(nothing)?;
                let run = rows(&projection, "runs").iter().find(|run| run["id"] == run_id).ok_or_else(nothing)?;
                let message = rows(&projection, "messages").iter().find(|message| message["id"] == run["userMessageId"]).ok_or_else(nothing)?;
                let ownership = &message["delegatedCompletion"];
                if !ownership.is_object() {
                    return Err(nothing());
                }
                let parent_run = rows(&projection, "runs").iter().find(|candidate| candidate["id"] == ownership["parentRunId"]).ok_or_else(nothing)?;
                let cohort = &parent_run["delegatedCompletion"];
                let delivery = &cohort["delivery"];
                if !cohort.is_object() || !delivery.is_object() || delivery["messageId"] != run["userMessageId"] || delivery["generation"] != ownership["generation"] {
                    return Err(nothing());
                }
                let stamp = at(now);
                let mut next_states: Vec<(Value, Value)> = vec![];
                for task in rows(&projection, "subagents") {
                    if task["origin"] == "app_owned" && task["runId"] == parent_run["id"] && delivery["taskIds"].as_array().is_some_and(|ids| ids.contains(&task["id"])) && task["completionDelivery"]["state"] == "claimed" {
                        next_states.push((task["id"].clone(), json!({"state":if run["status"] == "cancelled" {"pending"} else {"delivered"},"observedByRunId":null})));
                    }
                }
                let state_of = |id: &Value, next: &[(Value, Value)]| next.iter().find(|(known, _)| known == id).map(|(_, state)| state["state"].clone());
                let pending: Vec<Value> = rows(&projection, "subagents")
                    .iter()
                    .filter(|task| task["origin"] == "app_owned" && task["runId"] == parent_run["id"] && task_terminal(&task["status"]) && (task["completionDelivery"]["state"] == "pending" || state_of(&task["id"], &next_states) == Some(json!("pending"))))
                    .map(|task| task["id"].clone())
                    .collect();
                // Results that arrived while this delivery was outstanding go out together in one
                // successor. Each child becomes pending once, so a cohort's successors are bounded by its children.
                let reserve = cohort["disposition"] == "open" && projection["thread"]["archivedAt"].is_null() && projection["thread"]["deletedAt"].is_null() && !pending.is_empty();
                let next_delivery = reserve.then(|| json!({"generation":cohort["nextGeneration"],"messageId":message_id(thread_id, rows(&projection, "messages").len()),"taskIds":pending}));
                if next_delivery.is_some() {
                    for id in &pending {
                        next_states.retain(|(known, _)| known != id);
                        next_states.push((id.clone(), json!({"state":"claimed","observedByRunId":null})));
                    }
                }
                let mut updated_parent = parent_run.clone();
                updated_parent["delegatedCompletion"] = json!({"disposition":cohort["disposition"],"nextGeneration":cohort["nextGeneration"].as_u64().unwrap_or(1) + u64::from(next_delivery.is_some()),"delivery":next_delivery.clone().unwrap_or(Value::Null)});
                let mut events = vec![];
                for task in rows(&projection, "subagents") {
                    if let Some((_, state)) = next_states.iter().find(|(id, _)| *id == task["id"]) {
                        let mut task = task.clone();
                        task["completionDelivery"] = state.clone();
                        task["updatedAt"] = json!(stamp);
                        events.push(event(thread_id, &command_id, "subagent.updated", checked::<t3_contracts::Subagent>(task)?, now));
                    }
                }
                events.push(event(thread_id, &command_id, "run.updated", checked::<t3_contracts::Run>(updated_parent)?, now));
                if next_delivery.is_some() {
                    *offer.borrow_mut() = parent_run["id"].as_str().map(str::to_owned);
                }
                Ok(Decision::Accepted { events, effects: vec![] })
            },
            thread::reduce,
        );
        quiet(result.map(|(receipt, _)| receipt))?;
        if let Some(run) = offer.into_inner() {
            self.offer(thread_id, &run);
        }
        Ok(())
    }

    /// `offerDelegatedCompletionDelivery` and the continuation that acts on it: dispatches the wake
    /// message the cohort's delivery names, queued behind the parent's active run. Called again for
    /// a delivery whose dispatch failed; a delivery already dispatched is left alone.
    pub fn offer(&self, thread_id: &str, parent_run_id: &str) {
        let Ok(Some(projection)) = self.store().projection("thread", thread_id) else { return };
        let Some(parent_run) = rows(&projection, "runs").iter().find(|run| run["id"] == parent_run_id) else { return };
        let cohort = &parent_run["delegatedCompletion"];
        let delivery = &cohort["delivery"];
        if cohort["disposition"] != "open" || !delivery.is_object() || delivery["taskIds"].as_array().is_none_or(Vec::is_empty) || !projection["thread"]["archivedAt"].is_null() || !projection["thread"]["deletedAt"].is_null() {
            return;
        }
        if delivery_message(&projection, delivery).is_some() || !rows(&projection, "providerThreads").iter().any(|row| row["id"] == parent_run["providerThreadId"]) {
            return;
        }
        let command = json!({"type":"message.dispatch","commandId":format!("delegated-completion:{thread_id}:{}",delivery["messageId"].as_str().unwrap_or_default()),"threadId":thread_id,"messageId":delivery["messageId"],"text":wake_text(delivery["taskIds"].as_array().unwrap()),"attachments":[],"dispatchMode":{"type":"queue_after_active"},"createdBy":"agent","creationSource":"server","delegatedCompletion":{"parentRunId":parent_run_id,"generation":delivery["generation"],"taskIds":delivery["taskIds"]}});
        if let Err(error) = self.execution.dispatch(&command, Utc::now()) {
            tracing::warn!(thread_id, %error, "Could not dispatch a delegated completion delivery");
        }
    }

    /// Acts on one committed `run.updated` that ended a run: the run of a delegated child, or the
    /// wake run that delivered a cohort.
    fn run_ended(&self, thread_id: &str, run_id: &str) {
        let Ok(Some(view)) = self.store().projection("thread", thread_id) else { return };
        let Some(run) = rows(&view, "runs").iter().find(|run| run["id"] == run_id) else { return };
        if terminal_status(run["status"].as_str().unwrap_or_default()).is_none() {
            return;
        }
        if rows(&view, "messages").iter().any(|message| message["id"] == run["userMessageId"] && message["delegatedCompletion"].is_object()) {
            if let Err(error) = self.finalize_delivery(thread_id, run_id) {
                tracing::warn!(thread_id, run_id, %error, "Could not settle a delegated completion delivery");
            }
        }
        if view["thread"]["lineage"]["relationshipToParent"] == "subagent" && view["thread"]["forkedFrom"]["type"] == "node" {
            if let Err(error) = self.finalize_child(thread_id) {
                tracing::warn!(thread_id, %error, "Could not finalize a delegated task");
            }
        }
    }

    /// The startup pass: finalizes children whose runs ended while no listener ran, and offers the
    /// deliveries a restart cut short.
    pub fn recover(&self) {
        let Ok(views) = self.store().projections("thread") else { return };
        for view in &views {
            let id = view["thread"]["id"].as_str().unwrap_or_default();
            if view["thread"]["lineage"]["relationshipToParent"] == "subagent" && view["thread"]["forkedFrom"]["type"] == "node" {
                if let Err(error) = self.finalize_child(id) {
                    tracing::warn!(thread_id = id, %error, "Could not recover a delegated task");
                }
            }
        }
        for view in &views {
            let id = view["thread"]["id"].as_str().unwrap_or_default();
            for run in rows(view, "runs") {
                if run["delegatedCompletion"]["delivery"].is_object() {
                    self.offer(id, run["id"].as_str().unwrap_or_default());
                }
            }
        }
    }
}

/// Follows the store's committed events for ended runs that owe a delegated task something. It ends
/// with the store (when no sender is left).
pub fn start_listener(execution: &ExecutionService) {
    let service = DelegatedTasks::new(execution.clone());
    let mut events = execution.store.subscribe();
    let recovering = service.clone();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || recovering.recover()).await;
        loop {
            let batch = match events.recv().await {
                Ok(batch) => batch,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let again = service.clone();
                    let _ = tokio::task::spawn_blocking(move || again.recover()).await;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let ended: Vec<(String, String)> = batch
                .iter()
                .filter(|stored| stored.event.event_type == "run.updated" && terminal_status(stored.event.payload["status"].as_str().unwrap_or_default()).is_some())
                .map(|stored| (stored.event.aggregate_id.clone(), stored.event.payload["id"].as_str().unwrap_or_default().to_owned()))
                .collect();
            if ended.is_empty() {
                continue;
            }
            let handler = service.clone();
            let _ = tokio::task::spawn_blocking(move || {
                for (thread_id, run_id) in ended {
                    handler.run_ended(&thread_id, &run_id);
                }
            })
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_text_and_terminal_statuses_match_the_original() {
        let mut compared = 0;
        for line in include_str!("../tests/fixtures/delegated-completion.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["kind"].as_str().unwrap() {
                "wake" => assert_eq!(wake_text(row["taskIds"].as_array().unwrap()), row["text"].as_str().unwrap(), "{row}"),
                _ => {
                    let status = row["status"].as_str().unwrap();
                    assert_eq!(terminal_status(status).map_or(Value::Null, |status| json!(status)), row["terminal"], "{status}");
                    assert_eq!(task_terminal(&json!(status)), row["taskTerminal"], "{status}");
                }
            }
            compared += 1;
        }
        assert_eq!(compared, 15);
    }

    fn parent(run_status: &str, cohort: Value, extra: Value) -> Value {
        let mut run = json!({"id":"run","status":run_status,"userMessageId":"user"});
        if !cohort.is_null() {
            run["delegatedCompletion"] = cohort;
        }
        let mut projection = json!({"thread":{"id":"parent","archivedAt":null,"deletedAt":null},"runs":[run],"messages":[],"subagents":[]});
        for (key, value) in extra.as_object().into_iter().flatten() {
            projection[key] = value.clone();
        }
        projection
    }
    fn task(id: &str, wake: &str) -> Value {
        json!({"id":id,"completionWake":wake,"status":"completed"})
    }
    fn plan(projection: &Value, task: &Value) -> Plan {
        plan_delivery(projection, projection["runs"].get(0), task, task, "now", || "message:new".into())
    }

    #[test]
    fn a_finished_task_claims_a_new_delivery_unless_something_already_settled_its_wake() {
        // A parent that is idle owes a new delivery, with a fresh message and the next generation.
        let idle = parent("completed", Value::Null, json!({}));
        let first = plan(&idle, &task("t1", "always"));
        assert_eq!((first.task["completionDelivery"]["state"].as_str(), first.offer), (Some("claimed"), true));
        assert_eq!(first.parent_run.unwrap()["delegatedCompletion"], json!({"disposition":"open","nextGeneration":2,"delivery":{"generation":1,"messageId":"message:new","taskIds":["t1"]}}));
        // settled_only defers to a blocking wait while the spawning run is live; once it is over it delivers.
        let live = parent("running", Value::Null, json!({}));
        let deferred = plan(&live, &task("t1", "settled_only"));
        assert_eq!((deferred.task.get("completionDelivery").is_none(), deferred.offer, deferred.parent_run.is_none()), (true, false, true));
        assert!(plan(&idle, &task("t1", "settled_only")).offer);
        // An always-wake task is delivered even while the parent runs.
        assert!(plan(&live, &task("t1", "always")).offer);
        // A stopped cohort, an archived thread and a task already read all end the task's delivery.
        let stopped = parent("running", json!({"disposition":"stopped","nextGeneration":2,"delivery":null}), json!({}));
        assert_eq!(plan(&stopped, &task("t1", "always")).task["completionDelivery"]["state"], "disposed");
        let mut archived = parent("completed", Value::Null, json!({}));
        archived["thread"]["archivedAt"] = json!("then");
        assert_eq!(plan(&archived, &task("t1", "always")).task["completionDelivery"]["state"], "disposed");
        let mut read = task("t1", "always");
        read["completionDelivery"] = json!({"state":"acknowledged","observedByRunId":"run"});
        assert!(!plan(&idle, &read).offer);
    }

    #[test]
    fn a_task_joins_a_delivery_still_queued_and_waits_behind_one_that_started() {
        let cohort = json!({"disposition":"open","nextGeneration":2,"delivery":{"generation":1,"messageId":"wake","taskIds":["t1"]}});
        let queued = parent("running", cohort.clone(), json!({"messages":[{"id":"wake","runId":"wake-run","text":"old"}]}));
        let mut queued = queued;
        queued["runs"].as_array_mut().unwrap().push(json!({"id":"wake-run","status":"queued","userMessageId":"wake"}));
        let joined = plan(&queued, &task("t2", "always"));
        assert_eq!((joined.task["completionDelivery"]["state"].as_str(), joined.offer), (Some("claimed"), false));
        assert_eq!(joined.parent_run.unwrap()["delegatedCompletion"]["delivery"]["taskIds"], json!(["t1", "t2"]));
        let message = joined.message.unwrap();
        assert_eq!(message["delegatedCompletion"], json!({"parentRunId":"run","generation":1,"taskIds":["t1","t2"]}));
        assert!(message["text"].as_str().unwrap().starts_with("Delegated tasks t1, t2 reached terminal states."));
        // Once the wake run started, a sibling stays pending for the successor the listener reserves.
        queued["runs"][1]["status"] = json!("running");
        let waiting = plan(&queued, &task("t2", "always"));
        assert_eq!((waiting.task["completionDelivery"]["state"].as_str(), waiting.offer, waiting.message.is_none()), (Some("pending"), false, true));
        // A delivery with no run yet (its dispatch failed) is claimed again and offered again.
        let undispatched = parent("running", cohort, json!({}));
        let again = plan(&undispatched, &task("t2", "always"));
        assert_eq!((again.task["completionDelivery"]["state"].as_str(), again.offer), (Some("claimed"), true));
    }

    #[test]
    fn only_the_servers_own_queued_wake_for_the_current_delivery_is_admitted() {
        let cohort = json!({"disposition":"open","nextGeneration":2,"delivery":{"generation":1,"messageId":"wake","taskIds":["t1","t2"]}});
        let projection = parent("running", cohort, json!({}));
        let command = |overrides: Value| {
            let mut command = json!({"createdBy":"agent","creationSource":"server","messageId":"wake","text":"ignored","dispatchMode":{"type":"queue_after_active"},"delegatedCompletion":{"parentRunId":"run","generation":1,"taskIds":["x"]}});
            for (key, value) in overrides.as_object().unwrap() {
                command[key] = value.clone();
            }
            command
        };
        let mut admitted = command(json!({}));
        admit_message(&mut admitted, &projection).unwrap();
        assert_eq!(admitted["delegatedCompletion"]["taskIds"], json!(["t1", "t2"]), "the cohort names the tasks, not the caller");
        assert_eq!(admitted["text"], wake_text(&[json!("t1"), json!("t2")]));
        for refused in [command(json!({"createdBy":"user"})), command(json!({"creationSource":"mcp"})), command(json!({"dispatchMode":{"type":"start_immediately"}})), command(json!({"messageId":"other"})), command(json!({"delegatedCompletion":{"parentRunId":"run","generation":2,"taskIds":[]}})), command(json!({"delegatedCompletion":{"parentRunId":"nowhere","generation":1,"taskIds":[]}}))] {
            let mut refused = refused;
            assert!(admit_message(&mut refused, &projection).is_err(), "{refused}");
        }
        let mut ordinary = json!({"text":"hello"});
        admit_message(&mut ordinary, &projection).unwrap();
        assert_eq!(ordinary["text"], "hello");
    }
}
