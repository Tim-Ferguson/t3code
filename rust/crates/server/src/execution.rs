//! Persisted message commands and owned provider execution.
use crate::{
    persistence::{Decision, Event, NewEffect, Receipt, Store, StoreError, read_projection},
    provider_registry::ProviderRegistry,
    thread,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub fn checked<T: DeserializeOwned + Serialize>(value: Value) -> Result<Value, StoreError> {
    Ok(serde_json::to_value(serde_json::from_value::<T>(value)?)?)
}
pub fn at(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}
pub fn event(
    thread_id: &str,
    command_id: &str,
    kind: &str,
    payload: Value,
    now: DateTime<Utc>,
) -> Event {
    Event {
        event_id: uuid::Uuid::new_v4().to_string(),
        aggregate_kind: "thread".into(),
        aggregate_id: thread_id.into(),
        occurred_at: at(now),
        command_id: Some(command_id.into()),
        causation_event_id: None,
        correlation_id: Some(command_id.into()),
        event_type: kind.into(),
        payload,
        metadata: json!({}),
    }
}

#[derive(Clone)]
pub struct ExecutionService {
    pub(crate) store: Store,
    pub(crate) providers: ProviderRegistry,
    runtime: std::sync::Arc<crate::codex_runtime::RuntimeOwner>,
}
impl ExecutionService {
    pub fn start(store: Store, providers: ProviderRegistry) -> Self {
        Self::try_start(store, providers).expect("Native provider recovery failed")
    }
    pub fn try_start(store: Store, providers: ProviderRegistry) -> Result<Self, StoreError> {
        let lease = store.acquire_runtime_lease()?;
        Self::try_start_with_lease(store, providers, lease)
    }
    pub fn try_start_with_lease(
        store: Store,
        providers: ProviderRegistry,
        lease: crate::persistence::RuntimeLease,
    ) -> Result<Self, StoreError> {
        store.validate_runtime_lease(&lease)?;
        crate::codex_runtime::recover(&store, &lease)?;
        let runtime = crate::codex_runtime::start(store.clone(), providers.clone(), lease);
        Ok(Self {
            store,
            providers,
            runtime,
        })
    }
    #[cfg(test)]
    pub(crate) fn pause_next_actor(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(
        crate::codex_runtime::ActorLifetime,
        tokio::sync::oneshot::Sender<()>,
    )> {
        self.runtime.pause_next_actor()
    }
    pub fn authentication_stop(&self) -> crate::provider_auth_service::StopInstances {
        let runtime = std::sync::Arc::downgrade(&self.runtime);
        std::sync::Arc::new(move |instances| {
            let runtime = runtime.clone();
            Box::pin(async move {
                if let Some(runtime) = runtime.upgrade() {
                    runtime.stop_instances(&instances).await;
                }
                Ok(())
            })
        })
    }
    pub async fn shutdown(&self) {
        self.runtime.shutdown().await;
    }
    pub fn dispatch(&self, input: &Value, now: DateTime<Utc>) -> Result<Receipt, StoreError> {
        let mut input = input.clone();
        if input["type"] == "message.dispatch" {
            if input.get("createdBy").is_none() {
                input["createdBy"] = json!("user");
            }
            if input.get("creationSource").is_none() {
                input["creationSource"] = json!("web");
            }
        }
        let decoded: t3_contracts::ProviderCommand = serde_json::from_value(input)?;
        let command = decoded.service_payload()?;
        let id = command["commandId"].as_str().unwrap();
        let thread_id = command["threadId"].as_str().unwrap();
        let kind = command["type"].as_str().unwrap();
        let (receipt, replayed) = self.store.dispatch_resolved(
            id,
            "thread",
            kind,
            now,
            |_, _| Ok(thread_id.into()),
            |transaction, _| {
                let projection = read_projection(transaction, "thread", thread_id)?
                    .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                if !projection["thread"]["deletedAt"].is_null()
                    || !projection["thread"]["archivedAt"].is_null()
                {
                    return Err(StoreError::InvalidCommand(
                        "Thread is deleted or archived.".into(),
                    ));
                }
                match kind {
                    "message.dispatch" => {
                        let model = command
                            .get("modelSelection")
                            .unwrap_or(&projection["thread"]["modelSelection"]);
                        let driver = self
                            .providers
                            .driver(model["instanceId"].as_str().unwrap())
                            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                        plan_message_for_driver(&command, &projection, now, driver)
                    }
                    "run.interrupt" => plan_interrupt(&command, &projection, now),
                    "runtime-request.respond" => plan_response(&command, &projection, now),
                    "prepared-run.release" => plan_release(&command, &projection, now),
                    _ => Err(StoreError::InvalidCommand(format!(
                        "Native provider command {kind} is not yet available."
                    ))),
                }
            },
            thread::reduce,
        )?;
        if kind == "run.interrupt" && !replayed {
            self.runtime
                .cancel_start(thread_id, command["runId"].as_str().unwrap());
        }
        self.runtime.wake();
        Ok(receipt)
    }
}

fn effect(command: &Value, request: Value, now: DateTime<Utc>) -> NewEffect {
    NewEffect {
        id: format!(
            "effect:{}:{}:{}",
            command["commandId"].as_str().unwrap(),
            request["type"].as_str().unwrap(),
            request
                .get("runId")
                .or_else(|| request.get("requestId"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
        command_id: command["commandId"].as_str().unwrap().into(),
        thread_id: command["threadId"].as_str().unwrap().into(),
        request,
        available_at: at(now),
    }
}
fn plan_interrupt(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    let run = projection["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["id"] == command["runId"])
        .ok_or_else(|| StoreError::InvalidCommand("Run not found.".into()))?;
    if !matches!(
        run["status"].as_str(),
        Some("preparing" | "starting" | "running" | "waiting")
    ) {
        return Err(StoreError::InvalidCommand(
            "Run is no longer active.".into(),
        ));
    }
    let thread_id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    let timestamp = at(now);
    let attempt = projection["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attempt| attempt["id"] == run["activeAttemptId"])
        .unwrap();
    let mut item = json!({"id":format!("{}:interrupt-request",run["id"].as_str().unwrap()),"threadId":thread_id,"runId":run["id"],"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len(),"status":"completed","title":"Interrupt requested","startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"run_interrupt_request","message":command.get("reason").cloned().unwrap_or(json!("Interrupt requested"))});
    let mut events = vec![event(
        thread_id,
        command_id,
        "turn-item.updated",
        checked::<t3_contracts::TurnItem>(item.clone())?,
        now,
    )];
    if command["holdQueue"] == true {
        for queued in projection["runs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|run| run["status"] == "queued")
        {
            let mut queued = queued.clone();
            queued["queueHeld"] = json!(true);
            events.push(event(
                thread_id,
                command_id,
                "run.updated",
                checked::<t3_contracts::Run>(queued)?,
                now,
            ));
        }
    }
    if attempt["providerTurnId"].is_null() {
        for preparation in projection["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                item["runId"] == run["id"]
                    && item["type"] == "command_execution"
                    && item["input"] == "Preparing workspace"
                    && matches!(
                        item["status"].as_str(),
                        Some("running" | "pending" | "waiting")
                    )
            })
        {
            let mut preparation = preparation.clone();
            preparation["status"] = json!("interrupted");
            preparation["completedAt"] = json!(timestamp);
            preparation["updatedAt"] = json!(timestamp);
            events.push(event(
                thread_id,
                command_id,
                "turn-item.updated",
                checked::<t3_contracts::TurnItem>(preparation)?,
                now,
            ));
        }
        let mut run = run.clone();
        run["status"] = json!("interrupted");
        run["completedAt"] = json!(timestamp);
        let mut attempt = attempt.clone();
        attempt["status"] = json!("interrupted");
        attempt["completedAt"] = json!(timestamp);
        let mut node = projection["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == run["rootNodeId"])
            .unwrap()
            .clone();
        node["status"] = json!("interrupted");
        node["completedAt"] = json!(timestamp);
        item["id"] = json!(format!("{}:interrupt-result", run["id"].as_str().unwrap()));
        item["parentItemId"] = json!(format!("{}:interrupt-request", run["id"].as_str().unwrap()));
        item["ordinal"] = json!(run["ordinal"].as_u64().unwrap() * 100 + 98);
        item["type"] = json!("run_interrupt_result");
        item["status"] = json!("interrupted");
        item["title"] = json!("Interrupted");
        item["message"] = json!("Run interrupted by user");
        for (kind, payload) in [
            ("run.updated", checked::<t3_contracts::Run>(run)?),
            (
                "run-attempt.updated",
                checked::<t3_contracts::RunAttempt>(attempt)?,
            ),
            (
                "node.updated",
                checked::<t3_contracts::ExecutionNode>(node)?,
            ),
            (
                "turn-item.updated",
                checked::<t3_contracts::TurnItem>(item)?,
            ),
        ] {
            events.push(event(thread_id, command_id, kind, payload, now));
        }
        return Ok(Decision::Accepted {
            events,
            effects: vec![],
        });
    }
    Ok(Decision::Accepted {
        events,
        effects: vec![effect(
            command,
            json!({"type":"provider-turn.interrupt","runId":run["id"]}),
            now,
        )],
    })
}
fn plan_response(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    let request = projection["runtimeRequests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["id"] == command["requestId"])
        .ok_or_else(|| StoreError::InvalidCommand("Runtime request not found.".into()))?;
    if request["status"] != "pending" || request["responseCapability"]["type"] != "live" {
        return Err(StoreError::InvalidCommand(
            "Runtime request is not pending in a live provider session.".into(),
        ));
    }
    if !projection["providerSessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| {
            session["id"] == request["responseCapability"]["providerSessionId"]
                && session["status"] == "ready"
        })
    {
        return Err(StoreError::InvalidCommand(
            "Runtime request provider session is not ready.".into(),
        ));
    }
    if command.get("attachmentsByQuestionId").is_some() {
        return Err(StoreError::InvalidCommand(
            "Native answer attachment materialization is not yet available.".into(),
        ));
    }
    if request["kind"] == "user_input" {
        if command.get("decision").is_some()
            || !command["answers"]
                .as_object()
                .is_some_and(|answers| !answers.is_empty())
        {
            return Err(StoreError::InvalidCommand(
                "User input requires answers.".into(),
            ));
        }
    } else if command.get("decision").is_none() || command.get("answers").is_some() {
        return Err(StoreError::InvalidCommand(
            "Approval requires a decision.".into(),
        ));
    }
    let id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    let mut resolved = request.clone();
    resolved["status"] = json!("resolved");
    resolved["resolvedAt"] = json!(at(now));
    for field in ["decision", "answers"] {
        if let Some(value) = command.get(field) {
            resolved[field] = value.clone();
        }
    }
    let mut events = vec![event(
        id,
        command_id,
        "runtime-request.updated",
        checked::<t3_contracts::RuntimeRequest>(resolved)?,
        now,
    )];
    let status = if matches!(command["decision"].as_str(), Some("decline" | "cancel")) {
        "cancelled"
    } else {
        "completed"
    };
    if let Some(node) = projection["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == request["nodeId"])
    {
        let mut node = node.clone();
        node["status"] = json!(status);
        node["completedAt"] = json!(at(now));
        events.push(event(
            id,
            command_id,
            "node.updated",
            checked::<t3_contracts::ExecutionNode>(node)?,
            now,
        ));
    }
    if let Some(item) = projection["turnItems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["requestId"] == request["id"])
    {
        let mut item = item.clone();
        item["status"] = json!(status);
        item["completedAt"] = json!(at(now));
        item["updatedAt"] = json!(at(now));
        events.push(event(
            id,
            command_id,
            "turn-item.updated",
            checked::<t3_contracts::TurnItem>(item)?,
            now,
        ));
    }
    let mut response = json!({"type":"runtime-request.respond","requestId":request["id"],"nativeRequestId":request["nativeRequestRef"]["nativeId"],"providerSessionId":request["responseCapability"]["providerSessionId"]});
    for field in ["decision", "answers"] {
        if let Some(value) = command.get(field) {
            response[field] = value.clone();
        }
    }
    Ok(Decision::Accepted {
        events,
        effects: vec![effect(command, response, now)],
    })
}

/// Allocate the complete run graph and the external effect in the same transaction.
pub fn plan_message(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    plan_message_for_driver(command, projection, now, "codex")
}
pub fn plan_message_for_driver(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
    driver: &str,
) -> Result<Decision, StoreError> {
    for field in [
        "notification",
        "scheduledTaskId",
        "senderThreadId",
        "titleSeed",
        "sourcePlanRef",
        "restartContinuationOfRunId",
        "usageLimitContinuationOfRunId",
        "manualContinuationOfRunId",
        "usageLimitRecoveryRequestId",
        "delegatedCompletion",
    ] {
        if command.get(field).is_some() {
            return Err(StoreError::InvalidCommand(format!(
                "Native message {field} orchestration is not yet available."
            )));
        }
    }
    if command
        .get("deliveryIntent")
        .is_some_and(|intent| intent != "auto")
    {
        return Err(StoreError::InvalidCommand(
            "Native deferred delivery is not yet available.".into(),
        ));
    }
    let deferred = command["dispatchMode"]["type"] == "defer_start";
    if !deferred && command["dispatchMode"]["type"] != "start_immediately" {
        return Err(StoreError::InvalidCommand(
            "Native queue, steering and prepared-message orchestration is not yet available."
                .into(),
        ));
    }
    if deferred && command["dispatchMode"]["workspaceStrategy"]["type"] == "worktree" {
        return Err(StoreError::InvalidCommand(
            "Native Git worktree preparation is not yet available.".into(),
        ));
    }
    if !command["attachments"].as_array().is_some_and(Vec::is_empty)
        || command.get("context").is_some()
    {
        return Err(StoreError::InvalidCommand(
            "Native attachment/context materialization is not yet available.".into(),
        ));
    }
    let runs = projection["runs"].as_array().unwrap();
    if runs.iter().any(|run| {
        matches!(
            run["status"].as_str(),
            Some("preparing" | "starting" | "running" | "waiting")
        )
    }) {
        return Err(StoreError::InvalidCommand(
            "Thread already has an active run.".into(),
        ));
    }
    if projection["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["id"] == command["messageId"])
    {
        return Err(StoreError::InvalidCommand(
            "Message id already exists.".into(),
        ));
    }
    let thread_id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    let timestamp = at(now);
    let model = command
        .get("modelSelection")
        .unwrap_or(&projection["thread"]["modelSelection"]);
    if model["instanceId"] != projection["thread"]["modelSelection"]["instanceId"] {
        return Err(StoreError::InvalidCommand(
            "Native provider switching via handoff is not yet available.".into(),
        ));
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let node_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let ordinal = runs
        .iter()
        .filter_map(|run| run["ordinal"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let existing_provider_thread = projection["providerThreads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["id"] == projection["thread"]["activeProviderThreadId"]
                && row["providerInstanceId"] == model["instanceId"]
                && row["status"] != "closed"
        });
    let provider_thread_id = existing_provider_thread
        .and_then(|row| row["id"].as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut events = vec![];
    if model != &projection["thread"]["modelSelection"] {
        let mut thread = projection["thread"].clone();
        thread["modelSelection"] = model.clone();
        thread["updatedAt"] = json!(timestamp);
        events.push(event(
            thread_id,
            command_id,
            "thread.model-selection-updated",
            thread,
            now,
        ));
    }
    if existing_provider_thread.is_none() {
        let provider_thread = checked::<t3_contracts::ProviderThread>(
            json!({"id":provider_thread_id,"driver":driver,"providerInstanceId":model["instanceId"],"providerSessionId":null,"appThreadId":thread_id,"ownerNodeId":null,"nativeThreadRef":null,"nativeConversationHeadRef":null,"status":"not_loaded","firstRunOrdinal":ordinal,"lastRunOrdinal":ordinal,"handoffIds":[],"forkedFrom":null,"pendingBackgroundTasks":[],"createdAt":timestamp,"updatedAt":timestamp}),
        )?;
        events.push(event(
            thread_id,
            command_id,
            "provider-thread.updated",
            provider_thread,
            now,
        ));
    }
    let mut run = json!({"id":run_id,"threadId":thread_id,"ordinal":ordinal,"providerInstanceId":model["instanceId"],"modelSelection":model,"providerThreadId":provider_thread_id,"userMessageId":command["messageId"],"rootNodeId":node_id,"activeAttemptId":attempt_id,"status":if deferred{"preparing"}else{"starting"},"requestedAt":timestamp,"startedAt":null,"completedAt":null,"checkpointId":null,"contextHandoffId":null});
    if deferred {
        if let Some(strategy) = command["dispatchMode"].get("workspaceStrategy") {
            run["workspacePreparation"] = strategy.clone();
        }
    }
    let run = checked::<t3_contracts::Run>(run)?;
    let attempt = checked::<t3_contracts::RunAttempt>(
        json!({"id":attempt_id,"runId":run_id,"attemptOrdinal":1,"rootNodeId":node_id,"providerInstanceId":model["instanceId"],"providerThreadId":provider_thread_id,"providerTurnId":null,"reason":"initial","status":"pending","startedAt":null,"completedAt":null}),
    )?;
    let node = checked::<t3_contracts::ExecutionNode>(
        json!({"id":node_id,"threadId":thread_id,"runId":run_id,"parentNodeId":null,"rootNodeId":node_id,"kind":"root_turn","status":"pending","countsForRun":true,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":null,"completedAt":null}),
    )?;
    let message = checked::<t3_contracts::ConversationMessage>(
        json!({"createdBy":command["createdBy"],"creationSource":command["creationSource"],"id":command["messageId"],"threadId":thread_id,"runId":run_id,"nodeId":node_id,"role":"user","text":command["text"],"attachments":[],"streaming":false,"createdAt":timestamp,"updatedAt":timestamp}),
    )?;
    let item = checked::<t3_contracts::TurnItem>(
        json!({"id":uuid::Uuid::new_v4().to_string(),"threadId":thread_id,"runId":run_id,"nodeId":node_id,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len(),"status":"completed","title":null,"startedAt":timestamp,"completedAt":timestamp,"updatedAt":timestamp,"type":"user_message","createdBy":command["createdBy"],"creationSource":command["creationSource"],"messageId":command["messageId"],"inputIntent":"turn_start","text":command["text"],"attachments":[]}),
    )?;
    for (kind, payload) in [
        ("run.created", run),
        ("run-attempt.created", attempt),
        ("node.updated", node),
        ("message.updated", message),
        ("turn-item.updated", item),
    ] {
        events.push(event(thread_id, command_id, kind, payload, now));
    }
    if deferred {
        let item = checked::<t3_contracts::TurnItem>(
            json!({"id":format!("workspace-preparation:{run_id}"),"threadId":thread_id,"runId":run_id,"nodeId":node_id,"providerThreadId":provider_thread_id,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len()+1,"status":"running","title":"Preparing workspace","startedAt":timestamp,"completedAt":null,"updatedAt":timestamp,"type":"command_execution","input":"Preparing workspace"}),
        )?;
        events.push(event(thread_id, command_id, "turn-item.updated", item, now));
    }
    Ok(Decision::Accepted {
        events,
        effects: if deferred {
            vec![]
        } else {
            vec![NewEffect {
                id: format!("effect:{command_id}:provider-turn.start:{run_id}"),
                command_id: command_id.into(),
                thread_id: thread_id.into(),
                request: json!({"type":"provider-turn.start","runId":run_id}),
                available_at: timestamp,
            }]
        },
    })
}

pub(crate) fn plan_release(
    command: &Value,
    projection: &Value,
    now: DateTime<Utc>,
) -> Result<Decision, StoreError> {
    let run = projection["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["id"] == command["runId"] && run["status"] == "preparing")
        .ok_or_else(|| {
            StoreError::InvalidCommand("Run is not awaiting workspace preparation.".into())
        })?;
    if run["workspacePreparation"]["type"] == "worktree" {
        return Err(StoreError::InvalidCommand(
            "Native Git worktree preparation is not yet available.".into(),
        ));
    }
    let mut item = projection["turnItems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["runId"] == run["id"]
                && item["type"] == "command_execution"
                && item["input"] == "Preparing workspace"
        })
        .cloned()
        .ok_or_else(|| {
            StoreError::InvalidCommand("Workspace preparation activity is missing.".into())
        })?;
    let timestamp = at(now);
    item["status"] = json!("completed");
    item["title"] = json!("Workspace ready");
    item["output"] = json!("Workspace preparation completed.");
    item["exitCode"] = json!(0);
    item["completedAt"] = json!(timestamp);
    item["updatedAt"] = json!(timestamp);
    let mut run = run.clone();
    run["status"] = json!("starting");
    let thread_id = command["threadId"].as_str().unwrap();
    let command_id = command["commandId"].as_str().unwrap();
    Ok(Decision::Accepted {
        events: vec![
            event(
                thread_id,
                command_id,
                "turn-item.updated",
                checked::<t3_contracts::TurnItem>(item)?,
                now,
            ),
            event(
                thread_id,
                command_id,
                "run.updated",
                checked::<t3_contracts::Run>(run)?,
                now,
            ),
        ],
        effects: vec![effect(
            command,
            json!({"type":"provider-turn.start","runId":command["runId"]}),
            now,
        )],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{launch::ThreadLaunchService, project::ProjectService};
    fn project(store: &Store, path: &std::path::Path) {
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":path}),Utc::now()).unwrap();
    }
    fn launch(store: &Store, id: &str) -> String {
        ThreadLaunchService::new(store.clone()).launch(json!({"commandId":id,"projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap()["threadId"].as_str().unwrap().into()
    }
    fn message(thread: &str, id: &str, text: &str) -> Value {
        json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":format!("message:{id}"),"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})
    }
    #[test]
    fn prepared_message_has_no_provider_effect_until_release_and_stop_blocks_later_release() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        project(&store, directory.path());
        let id = launch(&store, "launch");
        let mut command = message(&id, "prepared", "Prepare first");
        command["dispatchMode"] = json!({"type":"defer_start","workspaceStrategy":{"type":"root"}});
        store
            .dispatch(
                "prepared",
                "thread",
                &id,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    plan_message(
                        &command,
                        &read_projection(transaction, "thread", &id)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        let projection = store.projection("thread", &id).unwrap().unwrap();
        assert_eq!(projection["runs"][0]["status"], "preparing");
        assert_eq!(projection["turnItems"][1]["status"], "running");
        assert!(
            store
                .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
        let release = json!({"type":"prepared-run.release","commandId":"release","threadId":id,"runId":projection["runs"][0]["id"]});
        let receipt = store
            .dispatch(
                "release",
                "thread",
                &id,
                "prepared-run.release",
                Utc::now(),
                |transaction| {
                    plan_release(
                        &release,
                        &read_projection(transaction, "thread", &id)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        assert_eq!(receipt.status, "accepted");
        let claimed = store
            .claim_effect("worker", Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.command_id, "release");
        assert_eq!(claimed.request["runId"], projection["runs"][0]["id"]);
        let second = launch(&store, "second");
        let mut command = message(&second, "stopped-prepared", "Do not start");
        command["dispatchMode"] = json!({"type":"defer_start","workspaceStrategy":{"type":"root"}});
        store
            .dispatch(
                "stopped-prepared",
                "thread",
                &second,
                "message.dispatch",
                Utc::now(),
                |transaction| {
                    plan_message(
                        &command,
                        &read_projection(transaction, "thread", &second)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        let projection = store.projection("thread", &second).unwrap().unwrap();
        let stop = json!({"type":"run.interrupt","commandId":"stop-prepared","threadId":second,"runId":projection["runs"][0]["id"]});
        store
            .dispatch(
                "stop-prepared",
                "thread",
                &second,
                "run.interrupt",
                Utc::now(),
                |transaction| {
                    plan_interrupt(
                        &stop,
                        &read_projection(transaction, "thread", &second)?.unwrap(),
                        Utc::now(),
                    )
                },
                thread::reduce,
            )
            .unwrap();
        let stopped = store.projection("thread", &second).unwrap().unwrap();
        assert_eq!(stopped["runs"][0]["status"], "interrupted");
        assert_eq!(stopped["turnItems"][1]["status"], "interrupted");
        assert!(plan_release(&json!({"type":"prepared-run.release","commandId":"late-release","threadId":second,"runId":projection["runs"][0]["id"]}),&stopped,Utc::now()).is_err());
    }
    #[test]
    fn startup_cancels_pending_and_claimed_process_work_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        project(&store, directory.path());
        let a = launch(&store, "a");
        let b = launch(&store, "b");
        for (thread, id) in [(&a, "a-message"), (&b, "b-message")] {
            let command = message(thread, id, "unsent");
            store
                .dispatch(
                    id,
                    "thread",
                    thread,
                    "message.dispatch",
                    Utc::now(),
                    |transaction| {
                        let projection = read_projection(transaction, "thread", thread)?.unwrap();
                        plan_message(&command, &projection, Utc::now())
                    },
                    thread::reduce,
                )
                .unwrap();
        }
        let claimed = store
            .claim_effect("old-process", Utc::now(), chrono::Duration::hours(1))
            .unwrap()
            .unwrap();
        let before = store.projections("thread").unwrap();
        let pending_id = before
            .iter()
            .find(|projection| projection["thread"]["id"] != claimed.thread_id)
            .unwrap()["runs"][0]["id"]
            .as_str()
            .unwrap();
        let pending_effect = store
            .read(|connection| {
                Ok(connection.query_row(
                    "SELECT id FROM rust_effect_outbox WHERE status='pending'",
                    [],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        let lease = store.acquire_runtime_lease().unwrap();
        crate::codex_runtime::recover(&store, &lease).unwrap();
        assert_eq!(
            store.effect(&claimed.id).unwrap().unwrap().status,
            "cancelled"
        );
        assert_eq!(
            store.effect(&pending_effect).unwrap().unwrap().status,
            "cancelled"
        );
        assert!(
            store
                .claim_effect("new-process", Utc::now(), chrono::Duration::seconds(1))
                .unwrap()
                .is_none()
        );
        for thread in [&a, &b] {
            let projection = store.projection("thread", thread).unwrap().unwrap();
            assert_eq!(projection["runs"][0]["status"], "cancelled");
            serde_json::from_value::<t3_contracts::ThreadProjection>(projection).unwrap();
        }
        assert!(!pending_id.is_empty());
    }
    #[tokio::test]
    async fn ownership_lease_cannot_authorize_a_different_database() {
        let a = Store::memory().unwrap();
        let b = Store::memory().unwrap();
        let lease = a.acquire_runtime_lease().unwrap();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(
            json!({"providerInstances":{"codex":{"driver":"codex","enabled":false}}}),
        )
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, std::env::temp_dir().as_path())
            .await
            .unwrap();
        assert!(ExecutionService::try_start_with_lease(b.clone(), providers, lease).is_err());
        assert_eq!(b.latest_sequence().unwrap(), 0);
        assert!(a.acquire_runtime_lease().is_ok());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_start_does_not_block_other_thread_stop_or_owned_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        project(&store, directory.path());
        let signal_path = directory.path().join("provider-signal");
        let signal = tokio::net::UnixDatagram::bind(&signal_path).unwrap();
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-provider.py");
        let settings:t3_contracts::ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary},"environment":[{"name":"FIXTURE_REQUEST_SIGNAL","value":signal_path}]}}})).unwrap();
        let providers = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        let stalled = launch(&store, "stalled");
        let fast = launch(&store, "fast");
        let shutdown_target = stalled.clone();
        execution
            .dispatch(
                &message(&stalled, "stall-message", "[hang-start]"),
                Utc::now(),
            )
            .unwrap();
        let mut buffer = [0u8; 128];
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(5), signal.recv(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        let first_pid = serde_json::from_slice::<Value>(&buffer[..received]).unwrap()["pid"]
            .as_u64()
            .unwrap();
        let pending = store.projection("thread", &stalled).unwrap().unwrap();
        assert_eq!(pending["runs"][0]["status"], "starting");
        execution
            .dispatch(&message(&fast, "fast-message", "normal"), Utc::now())
            .unwrap();
        milestone(&store, &mut events, &fast, |projection| {
            projection["runs"][0]["status"] == "completed"
        })
        .await;
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-starting","threadId":stalled,"runId":pending["runs"][0]["id"],"holdQueue":true}),Utc::now()).unwrap();
        let interrupted = milestone(&store, &mut events, &stalled, |projection| {
            projection["runs"][0]["status"] == "interrupted"
                && projection["providerSessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|session| session["status"] == "stopped")
        })
        .await;
        assert!(
            interrupted["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "run_interrupt_result")
        );
        execution
            .dispatch(
                &message(&shutdown_target, "shutdown-message", "[hang-start]"),
                Utc::now(),
            )
            .unwrap();
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(5), signal.recv(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        let last_pid = serde_json::from_slice::<Value>(&buffer[..received]).unwrap()["pid"]
            .as_u64()
            .unwrap();
        let starting = store.projection("thread", &stalled).unwrap().unwrap();
        let run_b = starting["runs"][1]["id"].clone();
        let recorded = store.receipt("stop-starting").unwrap().unwrap();
        let sequence = store.latest_sequence().unwrap();
        let replay=execution.dispatch(&json!({"type":"run.interrupt","commandId":"stop-starting","threadId":stalled,"runId":run_b}),Utc::now()).unwrap();
        assert_eq!(replay, recorded);
        assert_eq!(store.latest_sequence().unwrap(), sequence);
        // Cancellation is a process side effect: the new actor must not receive
        // a wake for an incoming payload that the immutable receipt did not commit.
        assert_eq!(execution.runtime.cancellation_target(&stalled), None);
        execution
            .dispatch(&message(&fast, "after-replay", "still-live"), Utc::now())
            .unwrap();
        milestone(&store, &mut events, &fast, |projection| {
            projection["runs"][1]["status"] == "completed"
        })
        .await;
        assert_eq!(
            store.projection("thread", &stalled).unwrap().unwrap()["runs"][1]["status"],
            "starting"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5),execution.shutdown()).await.expect("Shutdown must cancel the registered native request rather than wait for its 30-second timeout");
        assert!(store.acquire_runtime_lease().is_ok());
        for pid in [first_pid, last_pid] {
            assert!(
                !std::process::Command::new("/bin/kill")
                    .arg("-0")
                    .arg(pid.to_string())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "Shutdown must reap its exact owned provider process"
            );
        }
        let projection = store
            .projection("thread", &shutdown_target)
            .unwrap()
            .unwrap();
        assert!(matches!(
            projection["runs"][1]["status"].as_str(),
            Some("failed" | "interrupted")
        ));
        assert!(
            projection["providerSessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|session| session["status"] == "stopped")
        );
    }
    async fn milestone(
        store: &Store,
        events: &mut tokio::sync::broadcast::Receiver<Vec<crate::persistence::StoredEvent>>,
        thread_id: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let projection = store.projection("thread", thread_id).unwrap().unwrap();
                if predicate(&projection) {
                    return projection;
                }
                events.recv().await.unwrap();
            }
        })
        .await
        .expect("Provider milestone was not persisted")
    }
    #[tokio::test]
    async fn production_worker_streams_tools_approval_input_interrupt_and_replays_one_message() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-provider.py");
        let settings:t3_contracts::ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","config":{"binaryPath":binary},"environment":[{"name":"FIXTURE_NUMERIC_REQUEST_IDS","value":"1"}]}}})).unwrap();
        let providers = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        let execution = ExecutionService::start(store.clone(), providers);
        let launched=ThreadLaunchService::new(store.clone()).launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}),Utc::now()).unwrap();
        let thread_id = launched["threadId"].as_str().unwrap();
        let mut events = store.subscribe();
        let command = |id: &str, text: &str| json!({"type":"message.dispatch","commandId":id,"threadId":thread_id,"messageId":format!("message:{id}"),"text":text,"attachments":[],"createdBy":"user","creationSource":"web","deliveryIntent":"auto","dispatchMode":{"type":"start_immediately"}});
        let receipt = execution
            .dispatch(&command("approval", "[approval]"), Utc::now())
            .unwrap();
        assert_eq!(
            execution
                .dispatch(&command("approval", "[approval]"), Utc::now())
                .unwrap(),
            receipt
        );
        let pending = milestone(&store, &mut events, thread_id, |projection| {
            projection["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        })
        .await;
        let request = &pending["runtimeRequests"][0];
        let other = Store::open(directory.path().join("state.db")).unwrap();
        let sequence = store.latest_sequence().unwrap();
        assert!(ExecutionService::try_start(other, execution.providers.clone()).is_err());
        assert_eq!(store.latest_sequence().unwrap(), sequence);
        assert_eq!(
            store.projection("thread", thread_id).unwrap().unwrap()["runtimeRequests"][0]["status"],
            "pending"
        );
        assert_eq!(request["kind"], "command");
        let tool = pending["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "command_execution")
            .unwrap();
        assert!(tool.get("output").is_none());
        assert_eq!(tool["status"], "running");
        execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"answer-approval","threadId":thread_id,"requestId":request["id"],"decision":"acceptAlways"}),Utc::now()).unwrap();
        let completed = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][0]["status"] == "completed"
        })
        .await;
        assert_eq!(completed["runs"].as_array().unwrap().len(), 1);
        assert_eq!(
            completed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            1
        );
        let tool = completed["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "command_execution")
            .unwrap();
        assert_eq!(tool["output"], "fixture tool output\n");
        assert_eq!(tool["exitCode"], 0);
        assert!(
            completed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "assistant"
                    && message["text"].as_str().unwrap().contains("answered"))
        );
        serde_json::from_value::<t3_contracts::ThreadProjection>(completed).unwrap();
        execution
            .dispatch(&command("question", "[question]"), Utc::now())
            .unwrap();
        let pending = milestone(&store, &mut events, thread_id, |projection| {
            projection["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending" && request["kind"] == "user_input")
        })
        .await;
        let request = pending["runtimeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["status"] == "pending")
            .unwrap();
        execution.dispatch(&json!({"type":"runtime-request.respond","commandId":"answer-question","threadId":thread_id,"requestId":request["id"],"answers":{"choice":"One","unknown":"discard"}}),Utc::now()).unwrap();
        milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][1]["status"] == "completed"
        })
        .await;
        execution
            .dispatch(&command("hold", "[hold]"), Utc::now())
            .unwrap();
        let running = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][2]["status"] == "running"
                && projection["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["text"].as_str().unwrap().contains("[hold]"))
        })
        .await;
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"interrupt","threadId":thread_id,"runId":running["runs"][2]["id"]}),Utc::now()).unwrap();
        let interrupted = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][2]["status"] == "interrupted"
        })
        .await;
        serde_json::from_value::<t3_contracts::ThreadProjection>(interrupted).unwrap();
        execution
            .dispatch(&command("pending-interrupt", "[approval]"), Utc::now())
            .unwrap();
        let pending = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"].as_array().unwrap().len() == 4
                && projection["runtimeRequests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["status"] == "pending")
        })
        .await;
        let pending_request = pending["runtimeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["status"] == "pending")
            .unwrap();
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"pending-stop","threadId":thread_id,"runId":pending["runs"][3]["id"],"holdQueue":true}),Utc::now()).unwrap();
        let canceled = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][3]["status"] == "interrupted"
        })
        .await;
        let request = canceled["runtimeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["id"] == pending_request["id"])
            .unwrap();
        assert_eq!(request["status"], "cancelled");
        assert_eq!(request["responseCapability"]["type"], "not_resumable");
        execution
            .dispatch(&command("crash", "[crash]"), Utc::now())
            .unwrap();
        let crashed = milestone(&store, &mut events, thread_id, |projection| {
            projection["runs"][4]["status"] == "failed"
                && projection["providerSessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|session| session["status"] == "error")
                && projection["turnItems"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "error")
        })
        .await;
        assert!(
            crashed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["streaming"] != true)
        );
        assert!(
            crashed["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|node| node["runId"] == crashed["runs"][4]["id"])
                .all(|node| !matches!(
                    node["status"].as_str(),
                    Some("running" | "waiting" | "pending")
                ))
        );
        execution.shutdown().await;
        let lease = store.acquire_runtime_lease().unwrap();
        drop(lease);
        // Recreate a persisted pre-crash snapshot, then reconcile under a fresh
        // owner: this models an abrupt server death without graceful shutdown.
        store
            .transaction(|transaction| {
                crate::persistence::write_projection(transaction, "thread", thread_id, &pending)
            })
            .unwrap();
        let lease = store.acquire_runtime_lease().unwrap();
        crate::codex_runtime::recover(&store, &lease).unwrap();
        let recovered = store.projection("thread", thread_id).unwrap().unwrap();
        assert_eq!(recovered["runs"][3]["status"], "cancelled");
        assert!(
            recovered["providerSessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|session| matches!(session["status"].as_str(), Some("stopped" | "error")))
        );
        assert!(
            recovered["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request["status"] != "pending")
        );
        serde_json::from_value::<t3_contracts::ThreadProjection>(recovered).unwrap();
    }
}
